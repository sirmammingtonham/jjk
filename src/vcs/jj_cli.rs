//! Binary `Vcs` adapter: shells out to the pinned `jj` binary (0.41.0) and parses **templated**
//! `--no-graph` output. All `jj` subprocess calls in the whole program live here, so version
//! quirks stay in one place. Verified against JJ_NOTES.md.

use crate::error::Result;
use crate::model::{
    Bookmark, Capabilities, ChangeId, CommitId, CommitInfo, RemoteRef, WorkspaceInfo,
};
use crate::vcs::{PushOpts, Vcs, VcsTx};
use anyhow::{anyhow, Context};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Tab-separated, one-record-per-line template. `description.first_line()` is **last** and cannot
/// contain a newline, so each record is exactly one line; the parser uses `splitn` so a tab inside
/// the subject is preserved. Field order must match [`parse_commit`].
const COMMIT_TEMPLATE: &str = concat!(
    r#"change_id ++ "\t" ++ commit_id ++ "\t" ++ empty ++ "\t" ++ conflict ++ "\t""#,
    r#" ++ current_working_copy ++ "\t" ++ immutable ++ "\t""#,
    r#" ++ local_bookmarks.map(|b| b.name()).join(",") ++ "\t""#,
    r#" ++ remote_bookmarks.map(|b| b.name() ++ "@" ++ b.remote()).join(",") ++ "\t""#,
    r#" ++ parents.map(|p| p.change_id()).join(",") ++ "\t""#,
    r#" ++ description.first_line() ++ "\n""#,
);

const NUM_FIELDS: usize = 10;

/// Adapter over the `jj` binary rooted at a repo.
pub struct JjCli {
    root: PathBuf,
}

impl JjCli {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `jj git init --colocate <dir>` then return an adapter. Used by `jjk repo init`.
    pub fn init_colocated(dir: &Path) -> Result<Self> {
        let out = Command::new("jj")
            .args(["git", "init", "--colocate"])
            .arg(dir)
            .output()
            .context("failed to spawn `jj` — is it installed and on PATH?")?;
        if !out.status.success() {
            return Err(anyhow!(
                "jj git init failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(Self::new(dir))
    }

    /// Run a `jj` command in this repo, returning stdout. On failure, surface jj's real stderr.
    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(args)
            .output()
            .context("failed to spawn `jj`")?;
        if !out.status.success() {
            return Err(anyhow!(
                "jj {} failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Like [`run`] but also returns stderr (jj prints progress like "Rebased N commits" there).
    fn run_with_stderr(&self, args: &[&str]) -> Result<(String, String)> {
        let out = Command::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(args)
            .output()
            .context("failed to spawn `jj`")?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(anyhow!("jj {} failed:\n{}", args.join(" "), stderr.trim()));
        }
        Ok((String::from_utf8_lossy(&out.stdout).into_owned(), stderr))
    }

    /// Templated log over a revset → neutral commits.
    fn log(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        let stdout = self.run(&[
            "log",
            "--no-graph",
            "--color=never",
            "-r",
            revset,
            "-T",
            COMMIT_TEMPLATE,
        ])?;
        stdout
            .lines()
            .filter(|l| !l.is_empty())
            .map(parse_commit)
            .collect()
    }
}

/// Parse one tab-separated record produced by [`COMMIT_TEMPLATE`].
fn parse_commit(line: &str) -> Result<CommitInfo> {
    let f: Vec<&str> = line.splitn(NUM_FIELDS, '\t').collect();
    if f.len() < NUM_FIELDS {
        return Err(anyhow!(
            "unexpected jj template output ({} fields): {:?}",
            f.len(),
            line
        ));
    }
    let parse_bool = |s: &str| s == "true";
    let split_csv = |s: &str| -> Vec<String> {
        if s.is_empty() {
            vec![]
        } else {
            s.split(',').map(|x| x.to_string()).collect()
        }
    };
    let remote_bookmarks = if f[7].is_empty() {
        vec![]
    } else {
        f[7]
            .split(',')
            .filter_map(|tok| {
                let (name, remote) = tok.rsplit_once('@')?;
                // Ignore the colocated `git` pseudo-remote (JJ_NOTES §0).
                if remote == "git" {
                    return None;
                }
                Some(RemoteRef {
                    name: name.to_string(),
                    remote: remote.to_string(),
                })
            })
            .collect()
    };

    Ok(CommitInfo {
        change_id: ChangeId(f[0].to_string()),
        commit_id: CommitId(f[1].to_string()),
        is_empty: parse_bool(f[2]),
        has_conflict: parse_bool(f[3]),
        is_working_copy: parse_bool(f[4]),
        is_immutable: parse_bool(f[5]),
        local_bookmarks: split_csv(f[6]),
        remote_bookmarks,
        parents: split_csv(f[8]).into_iter().map(ChangeId).collect(),
        description: f[9].to_string(),
    })
}

impl Vcs for JjCli {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            atomic_transactions: false,
            in_process: false,
        }
    }

    fn trunk(&self) -> Result<ChangeId> {
        self.log("trunk()")?
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .ok_or_else(|| anyhow!("trunk() resolved to nothing"))
    }

    fn has_remote_trunk(&self) -> Result<bool> {
        // trunk() falls back to root() when no remote default exists (JJ_NOTES §8).
        Ok(!self.log("trunk() ~ root()")?.is_empty())
    }

    fn resolve(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        self.log(revset)
    }

    fn working_copy(&self) -> Result<CommitInfo> {
        self.log("@")?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not resolve working-copy commit @"))
    }

    fn bookmarks(&self) -> Result<Vec<Bookmark>> {
        // Derive from a log over bookmarked commits; `bookmarks()` revset is local-only (JJ_NOTES §1).
        let mut out = Vec::new();
        for c in self.log("bookmarks()")? {
            for name in c.local_bookmarks {
                out.push(Bookmark {
                    name,
                    target: c.change_id.clone(),
                });
            }
        }
        Ok(out)
    }

    fn transaction(&self, f: &mut dyn FnMut(&mut dyn VcsTx) -> Result<()>) -> Result<()> {
        // Binary adapter: non-atomic, each VcsTx call is one jj invocation.
        let mut tx = JjTx { cli: self };
        f(&mut tx)
    }

    fn undo(&self) -> Result<String> {
        let (_out, stderr) = self.run_with_stderr(&["undo"])?;
        Ok(stderr.trim().to_string())
    }

    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        // `jj workspace list` prints `name: <commit-summary>`; pair each name with its @ change id
        // via the `<name>@` revset. Per-workspace staleness detection is refined in Phase 5; here
        // only the current workspace's staleness is known cheaply.
        let listing = self.run(&["workspace", "list", "--color=never"])?;
        let current_stale = self.is_stale().unwrap_or(false);
        let mut out = Vec::new();
        for line in listing.lines() {
            let Some((name, _)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_string();
            let target = self.workspace_target(&name)?;
            let is_stale = current_stale && name == "default";
            out.push(WorkspaceInfo {
                name,
                working_copy: target,
                is_stale,
            });
        }
        Ok(out)
    }

    fn add_workspace(&self, path: &Path, name: &str, at: &ChangeId) -> Result<()> {
        self.run(&[
            "workspace",
            "add",
            "--name",
            name,
            "-r",
            at.as_str(),
            path.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?,
        ])?;
        Ok(())
    }

    fn forget_workspace(&self, name: &str) -> Result<()> {
        self.run(&["workspace", "forget", name])?;
        Ok(())
    }

    fn update_stale(&self) -> Result<()> {
        self.run_with_stderr(&["workspace", "update-stale"])?;
        Ok(())
    }

    fn is_stale(&self) -> Result<bool> {
        // No direct query; `jj status` errors with "stale" when the current @ is stale (JJ_NOTES §11).
        let out = Command::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(["status", "--color=never"])
            .output()
            .context("failed to spawn `jj`")?;
        if out.status.success() {
            return Ok(false);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("stale") {
            Ok(true)
        } else {
            Err(anyhow!("jj status failed:\n{}", stderr.trim()))
        }
    }

    fn fetch(&self, remote: &str) -> Result<()> {
        self.run_with_stderr(&["git", "fetch", "--remote", remote])?;
        Ok(())
    }

    fn push(&self, remote: &str, bookmark: &str, _opts: PushOpts) -> Result<()> {
        // jj push is force-with-lease by default; `-b <name>` also creates/deletes by name
        // (no `--allow-new` needed — deprecated). (JJ_NOTES §10)
        self.run_with_stderr(&["git", "push", "--remote", remote, "-b", bookmark])?;
        Ok(())
    }

    fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.run(&["git", "remote", "add", name, url])?;
        Ok(())
    }

    fn remotes(&self) -> Result<Vec<String>> {
        let out = self.run(&["git", "remote", "list"])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|r| *r != "git")
            .map(|r| r.to_string())
            .collect())
    }
}

impl JjCli {
    /// Change id of the working copy of workspace `name`.
    fn workspace_target(&self, name: &str) -> Result<ChangeId> {
        let revset = format!("{}@", name);
        self.log(&revset)?
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .ok_or_else(|| anyhow!("workspace '{}' has no working copy", name))
    }
}

/// Sequential, non-atomic transaction handle for the binary adapter.
struct JjTx<'a> {
    cli: &'a JjCli,
}

impl<'a> VcsTx for JjTx<'a> {
    fn finalize_working_copy(&mut self, message: &str) -> Result<ChangeId> {
        self.cli.run_with_stderr(&["commit", "-m", message])?;
        // The finalized commit is now @-.
        self.cli
            .log("@-")?
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .ok_or_else(|| anyhow!("could not resolve @- after commit"))
    }

    fn describe(&mut self, rev: &ChangeId, message: &str) -> Result<()> {
        self.cli
            .run_with_stderr(&["describe", rev.as_str(), "-m", message])?;
        Ok(())
    }

    fn squash_working_into(&mut self, into: &ChangeId) -> Result<()> {
        self.cli
            .run_with_stderr(&["squash", "--into", into.as_str()])?;
        Ok(())
    }

    fn new_child(&mut self, parent: &ChangeId) -> Result<ChangeId> {
        self.cli.run_with_stderr(&["new", parent.as_str()])?;
        self.cli
            .log("@")?
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .ok_or_else(|| anyhow!("could not resolve @ after new"))
    }

    fn edit(&mut self, rev: &ChangeId) -> Result<()> {
        self.cli.run_with_stderr(&["edit", rev.as_str()])?;
        Ok(())
    }

    fn create_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()> {
        self.cli
            .run_with_stderr(&["bookmark", "create", name, "-r", target.as_str()])?;
        Ok(())
    }

    fn set_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()> {
        // `-B` allows non-fast-forward (sideways/backwards) moves needed during restack.
        self.cli.run_with_stderr(&[
            "bookmark",
            "set",
            name,
            "-r",
            target.as_str(),
            "-B",
        ])?;
        Ok(())
    }

    fn delete_bookmark(&mut self, name: &str) -> Result<()> {
        self.cli.run_with_stderr(&["bookmark", "delete", name])?;
        Ok(())
    }

    fn forget_bookmark(&mut self, name: &str) -> Result<()> {
        self.cli.run_with_stderr(&["bookmark", "forget", name])?;
        Ok(())
    }

    fn rebase(&mut self, source: &ChangeId, dest: &ChangeId) -> Result<()> {
        self.cli
            .run_with_stderr(&["rebase", "-s", source.as_str(), "-d", dest.as_str()])?;
        Ok(())
    }

    fn abandon(&mut self, revs: &[ChangeId]) -> Result<()> {
        if revs.is_empty() {
            return Ok(());
        }
        let mut args = vec!["abandon"];
        args.extend(revs.iter().map(|r| r.as_str()));
        self.cli.run_with_stderr(&args)?;
        Ok(())
    }
}
