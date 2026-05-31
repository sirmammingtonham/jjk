//! Binary `Vcs` adapter: shells out to the `jj` binary and parses **templated** `--no-graph`
//! output. All `jj` subprocess calls in the whole program live here, so version quirks stay in one
//! place. Developed against jj 0.41.x (see [`SUPPORTED_JJ_MINOR`]); verified against JJ_NOTES.md.

use crate::error::Result;
use crate::model::{
    Bookmark, Capabilities, ChangeId, CommitId, CommitInfo, RemoteRef, WorkspaceInfo,
};
use crate::vcs::{CommitScope, PushOpts, Vcs, VcsTx};
use anyhow::{anyhow, Context};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The jj minor version jjk is developed against (the `41` in `0.41.x`). jj is pre-1.0, so its CLI
/// can change between minor releases; patch releases are treated as compatible.
pub const SUPPORTED_JJ_MINOR: u32 = 41;

/// A compatibility note if the installed jj's minor version differs from [`SUPPORTED_JJ_MINOR`].
/// Returns `None` when jj is on a compatible version, or can't be found/parsed.
pub fn version_warning() -> Option<String> {
    let out = Command::new("jj").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    warning_for_version(&String::from_utf8_lossy(&out.stdout))
}

/// Pure compatibility check for the text of `jj --version` (e.g. `"jj 0.41.0"`). Patch versions are
/// compatible; only a differing major/minor warns.
fn warning_for_version(version_output: &str) -> Option<String> {
    let ver = version_output
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = ver.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    if (major, minor) == (0, SUPPORTED_JJ_MINOR) {
        return None;
    }
    Some(format!(
        "warning: jjk is built against jj 0.{SUPPORTED_JJ_MINOR}.x; you have jj {ver}.\n\
         jj is pre-1.0 and its CLI can change between releases, so compatibility isn't guaranteed.\n\
         If you hit issues, install jj 0.{SUPPORTED_JJ_MINOR}.x."
    ))
}

/// Tab-separated, one-record-per-line template. `description.first_line()` is **last** and cannot
/// contain a newline, so each record is exactly one line; the parser uses `splitn` so a tab inside
/// the subject is preserved. Field order must match [`parse_commit`].
const COMMIT_TEMPLATE: &str = concat!(
    r#"change_id ++ "\t" ++ commit_id ++ "\t" ++ empty ++ "\t" ++ conflict ++ "\t""#,
    r#" ++ current_working_copy ++ "\t" ++ immutable ++ "\t""#,
    r#" ++ local_bookmarks.map(|b| b.name()).join(",") ++ "\t""#,
    r#" ++ remote_bookmarks.map(|b| b.name() ++ "@" ++ b.remote()).join(",") ++ "\t""#,
    r#" ++ parents.map(|p| p.change_id()).join(",") ++ "\t""#,
    r#" ++ committer.timestamp().ago() ++ "\t""#,
    r#" ++ description.first_line() ++ "\n""#,
);

const NUM_FIELDS: usize = 11;

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

    /// Run a `jj` command that needs the terminal (interactive editors); inherits stdio.
    fn run_interactive(&self, args: &[&str]) -> Result<()> {
        let status = Command::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(args)
            .status()
            .context("failed to spawn `jj`")?;
        if !status.success() {
            return Err(anyhow!("jj {} failed", args.join(" ")));
        }
        Ok(())
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

    /// Templated log over a revset → neutral commits. Uses `--ignore-working-copy` so reads don't
    /// re-snapshot the working tree (the dominant per-call cost in large repos). Commands that need
    /// `@` to reflect on-disk edits call [`Vcs::snapshot`] first; mutating jj commands snapshot on
    /// their own.
    fn log(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        self.log_inner(revset, true)
    }

    fn log_inner(&self, revset: &str, ignore_wc: bool) -> Result<Vec<CommitInfo>> {
        let mut args = vec!["log", "--no-graph"];
        if ignore_wc {
            args.push("--ignore-working-copy");
        }
        args.extend(["--color=never", "-r", revset, "-T", COMMIT_TEMPLATE]);
        let stdout = self.run(&args)?;
        stdout
            .lines()
            .filter(|l| !l.is_empty())
            .map(parse_commit)
            .collect()
    }
}

/// Whether `path` is an executable file (git only runs hooks that are executable).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
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
        time_ago: f[9].to_string(),
        description: f[10].to_string(),
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

    fn diff(&self, revset: &str) -> Result<String> {
        self.run(&["diff", "--ignore-working-copy", "--color=never", "-r", revset])
    }

    fn staged_paths(&self) -> Result<Vec<String>> {
        // Query the colocated git index directly (jj doesn't touch it). Paths are root-relative.
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["diff", "--cached", "--name-only", "-z"])
            .output()
            .context("failed to spawn `git`")?;
        if !out.status.success() {
            return Ok(vec![]); // not colocated / no index — treat as nothing staged
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
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

    fn snapshot(&self) -> Result<CommitInfo> {
        // A log of `@` WITHOUT `--ignore-working-copy` snapshots the tree and returns the fresh `@`.
        self.log_inner("@", false)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not resolve working-copy commit @"))
    }

    fn split_interactive(&self, rev: &ChangeId) -> Result<()> {
        self.run_interactive(&["split", "-r", rev.as_str()])
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

    fn workspace_count(&self) -> Result<usize> {
        let out = self.run(&["workspace", "list", "--ignore-working-copy", "--color=never"])?;
        Ok(out.lines().filter(|l| l.contains(':')).count())
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

    fn current_op_id(&self) -> Result<String> {
        // Snapshot the working copy first (no --ignore-working-copy) so pending edits become part of
        // the returned operation; restoring to it later brings those edits back as uncommitted `@`.
        let out = self.run(&["op", "log", "--no-graph", "--color=never", "--limit", "1", "-T", "id"])?;
        let id = out.lines().next().unwrap_or("").trim().to_string();
        if id.is_empty() {
            return Err(anyhow!("could not resolve the current jj operation id"));
        }
        Ok(id)
    }

    fn restore_op(&self, op_id: &str) -> Result<String> {
        let (_out, stderr) = self.run_with_stderr(&["op", "restore", op_id])?;
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

    fn push_deleted(&self, remote: &str) -> Result<()> {
        self.run_with_stderr(&["git", "push", "--remote", remote, "--deleted"])?;
        Ok(())
    }

    fn run_pre_commit_hook(&self, scope: &CommitScope) -> Result<()> {
        // Resolve the hook path, honouring core.hooksPath and the git-dir location.
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["rev-parse", "--git-path", "hooks/pre-commit"])
            .output()
            .context("failed to spawn `git`")?;
        if !out.status.success() {
            return Ok(()); // not a git repo / can't resolve — nothing to run
        }
        let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let hook = self.root.join(rel);
        if !is_executable(&hook) {
            return Ok(());
        }
        // Stage the in-scope changes so index-based hooks (`git diff --cached`) see exactly what
        // will be committed. jj reads the working tree, not the index, so this doesn't affect the
        // commit jj makes. `Paths` stages just those files (so a partially-staged file is promoted
        // to its whole working-tree content, matching jj's file-granularity commit).
        let mut add = Command::new("git");
        add.arg("-C").arg(&self.root).arg("add");
        match scope {
            CommitScope::Paths(paths) => {
                add.arg("--");
                add.args(paths);
            }
            CommitScope::All | CommitScope::Interactive => {
                add.arg("-A");
            }
        }
        let _ = add.output();
        // Run the hook from the repo root, inheriting the terminal.
        let status = Command::new(&hook)
            .current_dir(&self.root)
            .status()
            .with_context(|| format!("failed to run pre-commit hook {}", hook.display()))?;
        if !status.success() {
            return Err(anyhow!(
                "pre-commit hook failed (exit {}); commit `--no-verify`/`-n` to skip",
                status.code().unwrap_or(-1)
            ));
        }
        Ok(())
    }

    fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.run(&["git", "remote", "add", name, url])?;
        Ok(())
    }

    fn remotes(&self) -> Result<Vec<String>> {
        // Config read; doesn't depend on `@`, so don't snapshot (tolerate a stale workspace).
        let out = self.run(&["git", "remote", "list", "--ignore-working-copy"])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|r| *r != "git")
            .map(|r| r.to_string())
            .collect())
    }

    fn remote_url(&self, name: &str) -> Result<Option<String>> {
        // `jj git remote list` prints `<name> <url>` per line. `--ignore-working-copy` so opening
        // the engine in a stale workspace doesn't error before recovery (JJ_NOTES §11).
        let out = self.run(&["git", "remote", "list", "--ignore-working-copy"])?;
        Ok(out.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next(), it.next()) {
                (Some(n), Some(url)) if n == name => Some(url.to_string()),
                _ => None,
            }
        }))
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
        self.finalize_working_copy_scoped(message, &CommitScope::All)
    }

    fn finalize_working_copy_scoped(
        &mut self,
        message: &str,
        scope: &CommitScope,
    ) -> Result<ChangeId> {
        match scope {
            CommitScope::All => {
                self.cli.run_with_stderr(&["commit", "-m", message])?;
            }
            CommitScope::Interactive => {
                // Opens jj's diff editor; needs the terminal. Unselected changes stay in the new @.
                self.cli.run_interactive(&["commit", "-i", "-m", message])?;
            }
            CommitScope::Paths(paths) => {
                // Anchor each path at the workspace root (jj resolves bare paths against CWD, but
                // ours are root-relative); only these filesets are finalized into @-.
                let mut args = vec!["commit".to_string(), "-m".to_string(), message.to_string()];
                args.extend(paths.iter().map(|p| format!("root:\"{p}\"")));
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                self.cli.run_with_stderr(&argv)?;
            }
        }
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

    fn squash(&mut self, from: &ChangeId, into: &ChangeId) -> Result<()> {
        self.cli
            .run_with_stderr(&["squash", "--from", from.as_str(), "--into", into.as_str()])?;
        Ok(())
    }

    fn squash_revset(&mut self, from_revset: &str, into: &ChangeId) -> Result<()> {
        self.cli
            .run_with_stderr(&["squash", "--from", from_revset, "--into", into.as_str()])?;
        Ok(())
    }

    fn rename_bookmark(&mut self, old: &str, new: &str) -> Result<()> {
        self.cli.run_with_stderr(&["bookmark", "rename", old, new])?;
        Ok(())
    }

    fn duplicate_after(&mut self, rev: &ChangeId, after: &ChangeId) -> Result<()> {
        self.cli
            .run_with_stderr(&["duplicate", rev.as_str(), "--insert-after", after.as_str()])?;
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

#[cfg(test)]
mod version_tests {
    use super::{warning_for_version, SUPPORTED_JJ_MINOR};

    #[test]
    fn supported_minor_and_patches_are_silent() {
        assert!(warning_for_version(&format!("jj 0.{SUPPORTED_JJ_MINOR}.0")).is_none());
        assert!(warning_for_version(&format!("jj 0.{SUPPORTED_JJ_MINOR}.7")).is_none());
        // pre-release/build suffix on the patch is still the supported minor
        assert!(warning_for_version(&format!("jj 0.{SUPPORTED_JJ_MINOR}.0-abc")).is_none());
    }

    #[test]
    fn other_minors_and_majors_warn() {
        assert!(warning_for_version(&format!("jj 0.{}.0", SUPPORTED_JJ_MINOR + 1)).is_some());
        assert!(warning_for_version(&format!("jj 0.{}.0", SUPPORTED_JJ_MINOR - 1)).is_some());
        assert!(warning_for_version("jj 1.0.0").is_some());
    }

    #[test]
    fn unparseable_is_silent() {
        assert!(warning_for_version("not a version").is_none());
        assert!(warning_for_version("").is_none());
    }
}
