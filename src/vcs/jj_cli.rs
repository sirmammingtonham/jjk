//! Binary `Vcs` adapter: shells out to the `jj` binary and parses **templated** `--no-graph`
//! output. All `jj` subprocess calls in the whole program live here, so version quirks stay in one
//! place. Developed against jj 0.41.x (see [`SUPPORTED_JJ_MINOR`]); verified against JJ_NOTES.md.

use crate::error::Result;
use crate::model::{
    Bookmark, Capabilities, ChangeId, CommitId, CommitInfo, DiffLine, FileChangeKind, FileDiff,
    Hunk, RemoteRef, WorkspaceInfo,
};
use crate::vcs::{CommitScope, PushOpts, Vcs, VcsTx};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::process::Command as AsyncCommand;

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

    /// Run a `jj` command in this repo (blocking), returning stdout. On failure, surface jj's real
    /// stderr. Used by the synchronous mutation path ([`JjTx`]); reads use [`run_async`].
    fn run_blocking(&self, args: &[&str]) -> Result<String> {
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

    /// Async sibling of [`run_blocking`]: spawn a `jj` read via `tokio::process` and return stdout.
    /// Independent reads built on this can be awaited concurrently (see [`resolve_many`]).
    async fn run_async(&self, args: &[&str]) -> Result<String> {
        let out = AsyncCommand::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(args)
            .output()
            .await
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

    /// Raw git-format diff between two revisions (`jj diff --git --from <from> --to <to>`). Content
    /// diff between the two trees, ignoring ancestry. Non-snapshotting. Parsed by [`parse_git_diff`].
    async fn diff_git_raw(&self, from: &str, to: &str) -> Result<String> {
        self.run_async(&[
            "diff",
            "--ignore-working-copy",
            "--color=never",
            "--git",
            "--from",
            from,
            "--to",
            to,
        ])
        .await
    }

    /// Restore the working copy (`@`) to match `rev`'s tree for **all** paths (`jj restore --from`),
    /// then leave it for the caller to [`snapshot`](Vcs::snapshot). Used by domain-expansion to build
    /// a remainder commit whose tree equals the monolith. Operates on this adapter's workspace.
    pub async fn restore_all_from(&self, rev: &ChangeId) -> Result<()> {
        self.run_blocking(&["restore", "--from", rev.as_str()])?;
        Ok(())
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
    async fn log(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        self.log_inner(revset, true).await
    }

    async fn log_inner(&self, revset: &str, ignore_wc: bool) -> Result<Vec<CommitInfo>> {
        let args = log_args(revset, ignore_wc);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        parse_log(&self.run_async(&argv).await?)
    }

    /// Blocking sibling of [`log`] for the synchronous mutation path: [`JjTx`] reads back the
    /// change id of `@`/`@-` right after a mutating `jj` op, where there is no concurrency to gain.
    fn log_blocking(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        let args = log_args(revset, true);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        parse_log(&self.run_blocking(&argv)?)
    }
}

/// Argument vector for a templated `--no-graph` log over `revset` (shared by the async, blocking,
/// and concurrent-batch read paths so they stay byte-for-byte identical).
fn log_args(revset: &str, ignore_wc: bool) -> Vec<String> {
    let mut args = vec!["log".to_string(), "--no-graph".to_string()];
    if ignore_wc {
        args.push("--ignore-working-copy".to_string());
    }
    args.extend(["--color=never", "-r", revset, "-T", COMMIT_TEMPLATE].map(str::to_string));
    args
}

/// Parse the stdout of a [`log_args`] invocation into neutral commits.
fn parse_log(stdout: &str) -> Result<Vec<CommitInfo>> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_commit)
        .collect()
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

#[async_trait]
impl Vcs for JjCli {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            atomic_transactions: false,
            in_process: false,
        }
    }

    async fn trunk(&self) -> Result<ChangeId> {
        self.log("trunk()")
            .await?
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .ok_or_else(|| anyhow!("trunk() resolved to nothing"))
    }

    async fn has_remote_trunk(&self) -> Result<bool> {
        // trunk() falls back to root() when no remote default exists (JJ_NOTES §8).
        Ok(!self.log("trunk() ~ root()").await?.is_empty())
    }

    async fn diff(&self, revset: &str) -> Result<String> {
        self.run_async(&["diff", "--ignore-working-copy", "--color=never", "-r", revset])
            .await
    }

    async fn diff_hunks(&self, from: &ChangeId, to: &ChangeId) -> Result<Vec<FileDiff>> {
        let text = self.diff_git_raw(from.as_str(), to.as_str()).await?;
        Ok(parse_git_diff(&text))
    }

    async fn trees_equal(&self, a: &ChangeId, b: &ChangeId) -> Result<bool> {
        // Empty `--from a --to b` diff ⇔ identical trees (content, not ancestry).
        let text = self.diff_git_raw(a.as_str(), b.as_str()).await?;
        Ok(text.trim().is_empty())
    }

    async fn conflicted_paths(&self, rev: &ChangeId) -> Result<Vec<String>> {
        // `jj resolve --list` prints one line per conflicted file: `<path><padding><description>`
        // (e.g. `src/a.rs    2-sided conflict`). The path column is padded with ≥2 spaces while the
        // description uses single spaces, so the path is everything before the first run of 2+
        // spaces. Best-effort: with no conflicts jj exits non-zero ("No conflicts found …"), which
        // we map to an empty list rather than an error.
        let out = match self
            .run_async(&["resolve", "--list", "--color=never", "-r", rev.as_str()])
            .await
        {
            Ok(o) => o,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(out
            .lines()
            .filter_map(|l| {
                let path = l.split("  ").next().unwrap_or("").trim();
                (!path.is_empty()).then(|| path.to_string())
            })
            .collect())
    }

    async fn resolve_with_merge_tool(&self, rev: &ChangeId) -> Result<()> {
        // jj's own resolver launches `ui.merge-editor` on each conflicted file in `rev`; inherit
        // the terminal so the tool can interact (like `split_interactive`).
        self.run_interactive(&["resolve", "-r", rev.as_str()])
    }

    async fn staged_paths(&self) -> Result<Vec<String>> {
        // Query the colocated git index directly (jj doesn't touch it). Paths are root-relative.
        let out = AsyncCommand::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["diff", "--cached", "--name-only", "-z"])
            .output()
            .await
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

    async fn git_head(&self) -> Result<Option<String>> {
        let out = AsyncCommand::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .await
            .context("failed to spawn `git`")?;
        if !out.status.success() {
            return Ok(None); // unborn HEAD / not colocated
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((!s.is_empty()).then_some(s))
    }

    async fn set_git_head_branch(&self, branch: &str) -> Result<()> {
        let refname = format!("refs/heads/{branch}");
        // Only attach if jj has exported the bookmark to a git ref; otherwise leave HEAD as-is.
        let exists = AsyncCommand::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["show-ref", "--verify", "--quiet", &refname])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !exists {
            return Ok(());
        }
        AsyncCommand::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["symbolic-ref", "HEAD", &refname])
            .output()
            .await
            .context("failed to spawn `git`")?;
        Ok(())
    }

    async fn resolve(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        self.log(revset).await
    }

    async fn resolve_many(&self, revsets: &[&str]) -> Result<Vec<Vec<CommitInfo>>> {
        // Spawn each templated log as its own `jj` process and await them together: N independent
        // reads cost ~one round-trip instead of N. Results stay aligned with `revsets` because
        // `try_join_all` preserves input order regardless of completion order.
        let outputs = futures::future::try_join_all(revsets.iter().map(|r| {
            let args = log_args(r, true);
            async move {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                self.run_async(&argv).await
            }
        }))
        .await?;
        outputs.iter().map(|o| parse_log(o)).collect()
    }

    async fn working_copy(&self) -> Result<CommitInfo> {
        self.log("@")
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not resolve working-copy commit @"))
    }

    async fn snapshot(&self) -> Result<CommitInfo> {
        // A log of `@` WITHOUT `--ignore-working-copy` snapshots the tree and returns the fresh `@`.
        self.log_inner("@", false)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not resolve working-copy commit @"))
    }

    async fn split_interactive(&self, rev: &ChangeId) -> Result<()> {
        self.run_interactive(&["split", "-r", rev.as_str()])
    }

    async fn bookmarks(&self) -> Result<Vec<Bookmark>> {
        // Derive from a log over bookmarked commits; `bookmarks()` revset is local-only (JJ_NOTES §1).
        let mut out = Vec::new();
        for c in self.log("bookmarks()").await? {
            for name in c.local_bookmarks {
                out.push(Bookmark {
                    name,
                    target: c.change_id.clone(),
                });
            }
        }
        Ok(out)
    }

    async fn workspace_count(&self) -> Result<usize> {
        let out = self
            .run_async(&["workspace", "list", "--ignore-working-copy", "--color=never"])
            .await?;
        Ok(out.lines().filter(|l| l.contains(':')).count())
    }

    fn transaction(&self, f: &mut dyn FnMut(&mut dyn VcsTx) -> Result<()>) -> Result<()> {
        // Binary adapter: non-atomic, each VcsTx call is one jj invocation.
        let mut tx = JjTx { cli: self };
        f(&mut tx)
    }

    async fn undo(&self) -> Result<String> {
        let (_out, stderr) = self.run_with_stderr(&["undo"])?;
        Ok(stderr.trim().to_string())
    }

    async fn current_op_id(&self) -> Result<String> {
        // Snapshot the working copy first (no --ignore-working-copy) so pending edits become part of
        // the returned operation; restoring to it later brings those edits back as uncommitted `@`.
        let out = self
            .run_async(&["op", "log", "--no-graph", "--color=never", "--limit", "1", "-T", "id"])
            .await?;
        let id = out.lines().next().unwrap_or("").trim().to_string();
        if id.is_empty() {
            return Err(anyhow!("could not resolve the current jj operation id"));
        }
        Ok(id)
    }

    async fn restore_op(&self, op_id: &str) -> Result<String> {
        let (_out, stderr) = self.run_with_stderr(&["op", "restore", op_id])?;
        Ok(stderr.trim().to_string())
    }

    async fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        // `jj workspace list` prints `name: <commit-summary>`; pair each name with its @ change id
        // via the `<name>@` revset. Per-workspace staleness detection is refined in Phase 5; here
        // only the current workspace's staleness is known cheaply.
        let listing = self
            .run_async(&["workspace", "list", "--color=never"])
            .await?;
        let current_stale = self.is_stale().await.unwrap_or(false);
        // Resolve every workspace's `<name>@` target concurrently (one round-trip, not one per ws).
        let names: Vec<String> = listing
            .lines()
            .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim().to_string()))
            .collect();
        let revsets: Vec<String> = names.iter().map(|n| format!("{n}@")).collect();
        let revset_refs: Vec<&str> = revsets.iter().map(String::as_str).collect();
        let resolved = self.resolve_many(&revset_refs).await?;
        let mut out = Vec::with_capacity(names.len());
        for (name, commits) in names.into_iter().zip(resolved) {
            let target = commits
                .into_iter()
                .next()
                .map(|c| c.change_id)
                .ok_or_else(|| anyhow!("workspace '{}' has no working copy", name))?;
            let is_stale = current_stale && name == "default";
            out.push(WorkspaceInfo {
                name,
                working_copy: target,
                is_stale,
            });
        }
        Ok(out)
    }

    async fn add_workspace(&self, path: &Path, name: &str, at: &ChangeId) -> Result<()> {
        self.run_blocking(&[
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

    async fn forget_workspace(&self, name: &str) -> Result<()> {
        self.run_blocking(&["workspace", "forget", name])?;
        Ok(())
    }

    async fn update_stale(&self) -> Result<()> {
        self.run_with_stderr(&["workspace", "update-stale"])?;
        Ok(())
    }

    async fn is_stale(&self) -> Result<bool> {
        // No direct query; `jj status` errors with "stale" when the current @ is stale (JJ_NOTES §11).
        let out = AsyncCommand::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(["status", "--color=never"])
            .output()
            .await
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

    async fn fetch(&self, remote: &str, branch: Option<&str>) -> Result<()> {
        let mut args = vec!["git", "fetch", "--remote", remote];
        if let Some(b) = branch {
            args.push("--branch");
            args.push(b);
        }
        self.run_with_stderr(&args)?;
        Ok(())
    }

    async fn push(&self, remote: &str, bookmark: &str, _opts: PushOpts) -> Result<()> {
        // jj push is force-with-lease by default; `-b <name>` also creates/deletes by name
        // (no `--allow-new` needed — deprecated). (JJ_NOTES §10)
        let args = ["git", "push", "--remote", remote, "-b", bookmark];
        if let Err(e) = self.run_with_stderr(&args) {
            let msg = e.to_string();
            if msg.contains("Non-tracking remote bookmark") {
                // A remote bookmark from an earlier push exists but the local bookmark doesn't track
                // it (e.g. the branch was pushed / opened as a PR before being tracked in jjk). jj
                // refuses to push by name in that case. Adopt the remote bookmark, then retry — the
                // push then updates the existing remote/PR rather than erroring.
                self.run_with_stderr(&["bookmark", "track", bookmark, "--remote", remote])?;
                self.run_with_stderr(&args)?;
            } else if msg.contains("is conflicted") {
                // Local and remote diverged into a conflicted bookmark (e.g. the local stack was
                // rebased after the branch was already pushed). jjk's local stack is the source of
                // truth, so resolve the bookmark to its local (git) position and force-push it.
                let local = format!("{bookmark}@git");
                self.run_with_stderr(&["bookmark", "set", bookmark, "-r", &local, "-B"])?;
                eprintln!(
                    "note: resolved conflicted bookmark '{bookmark}' to the local position before pushing"
                );
                self.run_with_stderr(&args)?;
            } else {
                return Err(e);
            }
        }
        Ok(())
    }

    async fn push_deleted(&self, remote: &str) -> Result<()> {
        self.run_with_stderr(&["git", "push", "--remote", remote, "--deleted"])?;
        Ok(())
    }

    async fn run_pre_commit_hook(&self, scope: &CommitScope) -> Result<()> {
        // Resolve the hook path, honouring core.hooksPath and the git-dir location. Stays blocking:
        // the hook itself inherits the terminal and must run to completion before the commit.
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

    async fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.run_blocking(&["git", "remote", "add", name, url])?;
        Ok(())
    }

    async fn remotes(&self) -> Result<Vec<String>> {
        // Config read; doesn't depend on `@`, so don't snapshot (tolerate a stale workspace).
        let out = self
            .run_async(&["git", "remote", "list", "--ignore-working-copy"])
            .await?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|r| *r != "git")
            .map(|r| r.to_string())
            .collect())
    }

    async fn remote_url(&self, name: &str) -> Result<Option<String>> {
        // `jj git remote list` prints `<name> <url>` per line. `--ignore-working-copy` so opening
        // the engine in a stale workspace doesn't error before recovery (JJ_NOTES §11).
        let out = self
            .run_async(&["git", "remote", "list", "--ignore-working-copy"])
            .await?;
        Ok(out.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next(), it.next()) {
                (Some(n), Some(url)) if n == name => Some(url.to_string()),
                _ => None,
            }
        }))
    }

    async fn config_get(&self, key: &str) -> Result<Option<String>> {
        // `jj config get <key>` prints the value, or exits non-zero when the key is unset. A
        // missing key is not an error here — it's just `None`.
        let out = AsyncCommand::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(["config", "get", key])
            .output()
            .await
            .context("failed to spawn `jj`")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    async fn set_config_repo(&self, key: &str, value: &str) -> Result<()> {
        self.run_async(&["config", "set", "--repo", key, value]).await?;
        Ok(())
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
            .log_blocking("@-")?
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
            .log_blocking("@")?
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

/// Parse `jj diff --git` (git-format unified diff) into structured per-file hunks. Handles
/// add/modify/delete/rename and binary/mode-only changes (the latter carry no textual hunks).
fn parse_git_diff(text: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let (mut path, mut old_path) = parse_diff_git_paths(rest);
        let mut change = FileChangeKind::Modified;
        let mut is_binary = false;

        // Metadata lines (index/mode/---/+++/rename/binary) up to the first hunk or next file.
        while let Some(peek) = lines.peek() {
            if peek.starts_with("@@") || peek.starts_with("diff --git ") {
                break;
            }
            let meta = lines.next().unwrap();
            if meta.starts_with("new file") {
                change = FileChangeKind::Added;
            } else if meta.starts_with("deleted file") {
                change = FileChangeKind::Deleted;
            } else if let Some(src) = meta.strip_prefix("rename from ") {
                old_path = Some(src.to_string());
                change = FileChangeKind::Renamed;
            } else if let Some(dst) = meta.strip_prefix("rename to ") {
                path = dst.to_string();
            } else if meta.starts_with("Binary files") || meta.starts_with("GIT binary patch") {
                is_binary = true;
            }
        }

        let mut hunks = Vec::new();
        while let Some(peek) = lines.peek() {
            if !peek.starts_with("@@") {
                break;
            }
            let header = lines.next().unwrap();
            let (old_start, old_len, new_start, new_len) = parse_hunk_header(header);
            let mut hlines = Vec::new();
            while let Some(peek2) = lines.peek() {
                if peek2.starts_with("@@") || peek2.starts_with("diff --git ") {
                    break;
                }
                let l = lines.next().unwrap();
                if l.starts_with('\\') {
                    continue; // "\ No newline at end of file"
                }
                match l.as_bytes().first() {
                    Some(b'+') => hlines.push(DiffLine::Added(l[1..].to_string())),
                    Some(b'-') => hlines.push(DiffLine::Removed(l[1..].to_string())),
                    Some(b' ') => hlines.push(DiffLine::Context(l[1..].to_string())),
                    _ => hlines.push(DiffLine::Context(l.to_string())),
                }
            }
            hunks.push(Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
                lines: hlines,
            });
        }
        if is_binary {
            change = FileChangeKind::Binary;
        }
        files.push(FileDiff {
            path,
            old_path,
            change,
            hunks,
        });
    }
    files
}

/// Parse the `a/<old> b/<new>` tail of a `diff --git` line. Renames are corrected later from the
/// `rename from`/`rename to` lines, so for the common modify case (a == b) `old_path` is `None`.
fn parse_diff_git_paths(rest: &str) -> (String, Option<String>) {
    if let Some(idx) = rest.find(" b/") {
        let a = rest[..idx].trim_start_matches("a/").to_string();
        let b = rest[idx + 1..].trim_start_matches("b/").to_string();
        let old = (a != b).then_some(a);
        return (b, old);
    }
    (rest.trim_start_matches("a/").to_string(), None)
}

/// Parse `@@ -old_start,old_len +new_start,new_len @@ ...` (lengths default to 1 when omitted).
fn parse_hunk_header(h: &str) -> (u32, u32, u32, u32) {
    let core = h.split("@@").nth(1).unwrap_or("").trim();
    let mut parts = core.split_whitespace();
    let (os, ol) = parse_hunk_range(parts.next().unwrap_or("-0,0").trim_start_matches('-'));
    let (ns, nl) = parse_hunk_range(parts.next().unwrap_or("+0,0").trim_start_matches('+'));
    (os, ol, ns, nl)
}

fn parse_hunk_range(s: &str) -> (u32, u32) {
    let mut it = s.split(',');
    let start = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let len = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    (start, len)
}

#[cfg(test)]
mod diff_parse_tests {
    use super::*;

    #[test]
    fn parses_modify_add_delete_and_binary() {
        let text = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 keep
-old
+new
+extra
 tail
diff --git a/new.txt b/new.txt
new file mode 100644
index 000..333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 444..000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/logo.png b/logo.png
index 555..666 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = parse_git_diff(text);
        assert_eq!(files.len(), 4);

        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].change, FileChangeKind::Modified);
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (1, 3, 1, 4));
        assert_eq!(
            h.lines,
            vec![
                DiffLine::Context("keep".into()),
                DiffLine::Removed("old".into()),
                DiffLine::Added("new".into()),
                DiffLine::Added("extra".into()),
                DiffLine::Context("tail".into()),
            ]
        );

        assert_eq!(files[1].change, FileChangeKind::Added);
        assert_eq!(files[1].hunks[0].lines.len(), 2);
        assert_eq!(files[2].change, FileChangeKind::Deleted);
        assert_eq!(files[3].change, FileChangeKind::Binary);
        assert!(files[3].hunks.is_empty());
    }

    #[test]
    fn parses_rename() {
        let text = "\
diff --git a/old/path.rs b/new/path.rs
similarity index 90%
rename from old/path.rs
rename to new/path.rs
index 111..222 100644
--- a/old/path.rs
+++ b/new/path.rs
@@ -1 +1 @@
-a
+b
";
        let files = parse_git_diff(text);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].change, FileChangeKind::Renamed);
        assert_eq!(files[0].path, "new/path.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("old/path.rs"));
    }

    #[test]
    fn single_line_hunk_header_defaults_len_to_one() {
        assert_eq!(parse_hunk_header("@@ -5 +6 @@"), (5, 1, 6, 1));
        assert_eq!(parse_hunk_header("@@ -1,0 +1,3 @@ fn foo()"), (1, 0, 1, 3));
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
