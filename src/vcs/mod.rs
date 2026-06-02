//! The `Vcs` **port** (trait) — defined in terms of the domain operations the engine needs,
//! never in terms of any backend. No `jj-lib` type appears here.
//!
//! The sole adapter is [`jj_lib`], which links the `jj-lib` crate and runs in-process: a whole
//! jjk command loads the repo once and groups its mutations into a single jj-lib transaction
//! (atomic, one op-log entry) instead of spawning a `jj` subprocess per step.
//!
//! **Everything is `async`.** Reads can be fanned out concurrently (see
//! [`resolve_many`](Vcs::resolve_many)); a command's mutations run inside an async
//! [`VcsTx`] obtained from [`begin_transaction`](Vcs::begin_transaction) and finalized with
//! [`VcsTx::commit`].

pub mod jj_lib;

use crate::error::Result;
use crate::model::{Bookmark, Capabilities, ChangeId, CommitInfo, WorkspaceInfo};
use async_trait::async_trait;
use std::path::Path;

/// Options for pushing a bookmark to a remote.
#[derive(Clone, Debug, Default)]
pub struct PushOpts {
    /// Push a deletion of this bookmark instead of its current target.
    pub delete: bool,
}

/// Which subset of the working copy a `commit` should finalize. The remainder stays uncommitted in
/// `@`. Paths are **repo-root-relative** (the engine resolves CLI paths against the root).
#[derive(Clone, Debug, Default)]
pub enum CommitScope {
    /// Everything in the working copy (the default with no staging).
    #[default]
    All,
    /// Only these root-relative paths — used for explicit CLI paths and git-staged files.
    Paths(Vec<String>),
    /// Interactively pick hunks/files (`jj commit -i`; inherits the terminal).
    Interactive,
}

/// The VCS port. Reads are direct; a command's mutations are grouped inside a [`VcsTx`] obtained
/// from [`begin_transaction`](Vcs::begin_transaction), then finalized with [`VcsTx::commit`] — the
/// `jj_lib` adapter maps that to one atomic jj-lib transaction.
///
/// Everything is `async`. The futures are not required to be `Send` (`#[async_trait(?Send)]`)
/// because jj-lib's in-memory repo/transaction handles are not `Sync` and jjk never spawns a
/// command's work onto another thread — it drives one command to completion on the current
/// runtime. The adapter itself is `Send` (its mutable state lives behind interior mutability).
#[async_trait(?Send)]
pub trait Vcs: Send {
    fn capabilities(&self) -> Capabilities;

    // ---- queries ----

    /// The trunk commit (`trunk()` revset). Falls back to root() when no remote default exists;
    /// callers that need the configured trunk bookmark should consult state instead.
    async fn trunk(&self) -> Result<ChangeId>;

    /// Resolve a (neutral subset) revset into commits, newest-first as jj logs them.
    async fn resolve(&self, revset: &str) -> Result<Vec<CommitInfo>>;

    /// Resolve several **independent** revsets concurrently; results align positionally with
    /// `revsets` (result `i` is the resolution of `revsets[i]`). The binary adapter spawns the `jj`
    /// reads in parallel — one round-trip's latency instead of N — so callers should batch reads
    /// that don't depend on each other here. The default implementation resolves sequentially, so
    /// any adapter is correct without overriding it.
    async fn resolve_many(&self, revsets: &[&str]) -> Result<Vec<Vec<CommitInfo>>> {
        let mut out = Vec::with_capacity(revsets.len());
        for r in revsets {
            out.push(self.resolve(r).await?);
        }
        Ok(out)
    }

    /// The current workspace's working-copy commit (`@`). Reads are non-snapshotting (fast); call
    /// [`snapshot`](Vcs::snapshot) first if you need `@` to reflect on-disk edits.
    async fn working_copy(&self) -> Result<CommitInfo>;

    /// Snapshot the working copy (capturing on-disk edits) and return the resulting `@`. Reads
    /// otherwise skip snapshotting for speed (snapshotting a large tree is the dominant per-call
    /// cost), so call this when `@` must reflect current edits.
    async fn snapshot(&self) -> Result<CommitInfo>;

    /// Split a revision into two interactively (`jj split` opens a diff editor; inherits the
    /// terminal). Descendants auto-rebase.
    async fn split_interactive(&self, rev: &ChangeId) -> Result<()>;

    /// All local bookmarks.
    async fn bookmarks(&self) -> Result<Vec<Bookmark>>;

    /// Whether `trunk()` resolves to a real (non-root) commit, i.e. a remote default branch exists.
    async fn has_remote_trunk(&self) -> Result<bool>;

    /// Human-readable diff for a revset (e.g. `base..tip`). Non-snapshotting.
    async fn diff(&self, revset: &str) -> Result<String>;

    /// Repo-root-relative paths that are conflicted in `rev` (`jj resolve --list -r <rev>`); empty
    /// when there are none. A display helper for guiding conflict resolution — best-effort, so
    /// callers may treat any error as "no paths".
    async fn conflicted_paths(&self, rev: &ChangeId) -> Result<Vec<String>>;

    /// Resolve the conflicts in `rev` with jj's configured merge tool (`jj resolve`), one file at a
    /// time — the `git mergetool`-style focused flow. Inherits the terminal. Any error (e.g. no
    /// merge tool configured) surfaces jj's own guidance.
    async fn resolve_with_merge_tool(&self, rev: &ChangeId) -> Result<()>;

    /// Repo-root-relative paths currently staged in the colocated git index
    /// (`git diff --cached --name-only`). Empty when nothing is staged. jj ignores the index, so
    /// this is purely a signal of what the user staged (e.g. via their editor).
    async fn staged_paths(&self) -> Result<Vec<String>>;

    /// The commit the colocated git `HEAD` points at (`git rev-parse HEAD`), or `None` if there is
    /// no git HEAD (unborn / not colocated). Used to detect an external `git checkout`.
    async fn git_head(&self) -> Result<Option<String>>;

    /// Attach the colocated git `HEAD` symbolically to `branch` (`git symbolic-ref`), so plain git
    /// shows the same branch jjk is on. jj leaves `HEAD` detached when it moves `@`; this re-points
    /// it. No-op if `refs/heads/<branch>` doesn't exist. Only rewrites the ref — never touches the
    /// working tree or index.
    async fn set_git_head_branch(&self, branch: &str) -> Result<()>;

    // ---- mutations (grouped) ----

    /// Begin a transaction. Issue the command's mutations on the returned handle, then call
    /// [`VcsTx::commit`] to finalize them (the `jj_lib` adapter applies the whole batch as one
    /// atomic jj-lib operation; dropping the handle without committing rolls them back).
    async fn begin_transaction(&self) -> Result<Box<dyn VcsTx + '_>>;

    /// Undo the last operation (`jj undo`). Returns the human description jj printed.
    async fn undo(&self) -> Result<String>;

    /// The id of the current head operation in jj's op log. Snapshots the working copy first (so the
    /// returned op captures any pending edits), making it a faithful "before" checkpoint to restore
    /// to. Used to make a whole jjk command (which is several jj operations) one undo unit.
    async fn current_op_id(&self) -> Result<String>;

    /// Restore the repo to an earlier operation (`jj op restore <id>`); reverts commits, bookmarks
    /// and the working copy in one step. Returns the human description jj printed.
    async fn restore_op(&self, op_id: &str) -> Result<String>;

    // ---- workspaces ----

    async fn workspaces(&self) -> Result<Vec<WorkspaceInfo>>;
    /// Number of workspaces (cheap; non-snapshotting). Staleness is only possible with >1.
    async fn workspace_count(&self) -> Result<usize>;
    async fn add_workspace(&self, path: &Path, name: &str, at: &ChangeId) -> Result<()>;
    async fn forget_workspace(&self, name: &str) -> Result<()>;
    /// Update a stale workspace's working copy. No-op (Ok) if not stale.
    async fn update_stale(&self) -> Result<()>;
    /// True if the current workspace's `@` is stale.
    async fn is_stale(&self) -> Result<bool>;

    // ---- remote (git interop lives inside the VCS backend) ----

    /// Fetch from `remote`. When `branch` is `Some`, fetch only that bookmark (the trunk) instead
    /// of every remote branch — stacking only needs trunk advanced, and a full fetch can fail on
    /// unrelated remote branches that won't fast-forward (`refs/remotes/...` update errors).
    async fn fetch(&self, remote: &str, branch: Option<&str>) -> Result<()>;
    async fn push(&self, remote: &str, bookmark: &str, opts: PushOpts) -> Result<()>;
    /// Push all pending bookmark deletions to the remote (`jj git push --deleted`).
    async fn push_deleted(&self, remote: &str) -> Result<()>;
    /// Run the git `pre-commit` hook (respecting `core.hooksPath`) the way `git commit` would:
    /// stage the in-scope changes so index-based hooks see them, then run the hook with the
    /// terminal. `All`/`Interactive` stage everything (`git add -A`); `Paths` stages just those
    /// paths so the hook sees exactly what will be committed. Returns `Err` if the hook exits
    /// non-zero; `Ok` if it passes or there is no executable hook.
    async fn run_pre_commit_hook(&self, scope: &CommitScope) -> Result<()>;

    async fn add_remote(&self, name: &str, url: &str) -> Result<()>;
    /// Names of configured remotes (excludes the colocated `git` pseudo-remote).
    async fn remotes(&self) -> Result<Vec<String>>;
    /// URL of a configured remote, if present.
    async fn remote_url(&self, name: &str) -> Result<Option<String>>;

    /// Value of a jj config key (`jj config get <key>`), or `None` if it is unset. Lets jjk read
    /// user-set knobs (e.g. an opt-in flag in the repo's jj config) without its own config file.
    async fn config_get(&self, key: &str) -> Result<Option<String>>;

    /// Set a repo-local jj config value (`jj config set --repo <key> <value>`), written to the
    /// repo's `.jj/repo/config.toml`. Used by `repo init` to pick git-friendly defaults.
    async fn set_config_repo(&self, key: &str, value: &str) -> Result<()>;
}

/// Mutation handle from [`Vcs::begin_transaction`]. Issue mutations, then call [`commit`](VcsTx::commit).
///
/// Methods that produce a new change return its (stable) [`ChangeId`] so the engine can refer to it
/// in subsequent steps without re-deriving it.
#[async_trait(?Send)]
pub trait VcsTx {
    /// `jj commit -m <message>`: finalize `@` into a real commit and open a fresh empty `@`.
    /// Returns the finalized commit's change id (the just-created `@-`).
    async fn finalize_working_copy(&mut self, message: &str) -> Result<ChangeId>;

    /// Like [`finalize_working_copy`](VcsTx::finalize_working_copy) but only finalizes the part of
    /// `@` named by `scope`; anything outside the scope stays uncommitted in the new `@`.
    async fn finalize_working_copy_scoped(
        &mut self,
        message: &str,
        scope: &CommitScope,
    ) -> Result<ChangeId>;

    /// `jj describe <rev> -m <message>`.
    async fn describe(&mut self, rev: &ChangeId, message: &str) -> Result<()>;

    /// Squash the working-copy (`@`) changes into `into` (amend). Descendants auto-rebase.
    async fn squash_working_into(&mut self, into: &ChangeId) -> Result<()>;

    /// Squash all changes from `from` into `into` (`jj squash --from --into`). `from` is abandoned
    /// when it becomes empty; a bookmark on it moves to its parent (forget it separately).
    async fn squash(&mut self, from: &ChangeId, into: &ChangeId) -> Result<()>;

    /// Like [`squash`](VcsTx::squash) but `from` is a revset (e.g. a whole range of commits to
    /// collapse into `into`).
    async fn squash_revset(&mut self, from_revset: &str, into: &ChangeId) -> Result<()>;

    /// Rename a local bookmark.
    async fn rename_bookmark(&mut self, old: &str, new: &str) -> Result<()>;

    /// Copy `rev` as a new commit inserted right after `after` (`jj duplicate --insert-after`),
    /// rebasing `after`'s existing children onto the copy.
    async fn duplicate_after(&mut self, rev: &ChangeId, after: &ChangeId) -> Result<()>;

    /// `jj new <parent>`: create an empty child of `parent` and make it `@`. Returns its change id.
    async fn new_child(&mut self, parent: &ChangeId) -> Result<ChangeId>;

    /// Make `rev` the working copy (`jj edit`), e.g. to resolve a conflict in place.
    async fn edit(&mut self, rev: &ChangeId) -> Result<()>;

    /// Create a new bookmark (errors if it exists).
    async fn create_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()>;
    /// Create-or-move a bookmark by name (`jj bookmark set`, `-B` for non-fast-forward).
    async fn set_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()>;
    /// Delete a bookmark locally and schedule its remote deletion on next push.
    async fn delete_bookmark(&mut self, name: &str) -> Result<()>;
    /// Forget a bookmark locally without scheduling a remote deletion.
    async fn forget_bookmark(&mut self, name: &str) -> Result<()>;

    /// `jj rebase -s <source> -d <dest>`: rebase source and descendants onto dest.
    async fn rebase(&mut self, source: &ChangeId, dest: &ChangeId) -> Result<()>;

    /// `jj abandon <revs...>`: abandon the commits in one op; descendants auto-rebase onto the
    /// abandoned range's parent. Bookmarks on abandoned commits are deleted (JJ_NOTES §2).
    async fn abandon(&mut self, revs: &[ChangeId]) -> Result<()>;

    /// Finalize the transaction: apply all issued mutations as one atomic jj-lib operation and
    /// update the on-disk working copy. Consumes the handle.
    async fn commit(self: Box<Self>) -> Result<()>;
}
