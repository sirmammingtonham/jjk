//! The `Vcs` **port** (trait) — defined in terms of the domain operations the engine needs,
//! never in terms of any backend. No `jj-lib` type and no CLI/JSON shape appears here.
//!
//! Adapters live in submodules (`jj_cli` binary today; `jj_lib` crate later).

pub mod jj_cli;

use crate::error::Result;
use crate::model::{Bookmark, Capabilities, ChangeId, CommitInfo, WorkspaceInfo};
use std::path::Path;

/// Options for pushing a bookmark to a remote.
#[derive(Clone, Debug, Default)]
pub struct PushOpts {
    /// Push a deletion of this bookmark instead of its current target.
    pub delete: bool,
}

/// The VCS port. Read methods are direct; mutations are grouped inside [`Vcs::transaction`] so a
/// backend may make them atomic. The binary adapter runs each mutation as one `jj` invocation and
/// reports `atomic_transactions: false`.
pub trait Vcs {
    fn capabilities(&self) -> Capabilities;

    // ---- queries ----

    /// The trunk commit (`trunk()` revset). Falls back to root() when no remote default exists;
    /// callers that need the configured trunk bookmark should consult state instead.
    fn trunk(&self) -> Result<ChangeId>;

    /// Resolve a (neutral subset) revset into commits, newest-first as jj logs them.
    fn resolve(&self, revset: &str) -> Result<Vec<CommitInfo>>;

    /// The current workspace's working-copy commit (`@`). Reads are non-snapshotting (fast); call
    /// [`snapshot`](Vcs::snapshot) first if you need `@` to reflect on-disk edits.
    fn working_copy(&self) -> Result<CommitInfo>;

    /// Snapshot the working copy (capturing on-disk edits) and return the resulting `@`. Reads
    /// otherwise skip snapshotting for speed (snapshotting a large tree is the dominant per-call
    /// cost), so call this when `@` must reflect current edits.
    fn snapshot(&self) -> Result<CommitInfo>;

    /// All local bookmarks.
    fn bookmarks(&self) -> Result<Vec<Bookmark>>;

    /// Whether `trunk()` resolves to a real (non-root) commit, i.e. a remote default branch exists.
    fn has_remote_trunk(&self) -> Result<bool>;

    /// Human-readable diff for a revset (e.g. `base..tip`). Non-snapshotting.
    fn diff(&self, revset: &str) -> Result<String>;

    // ---- mutations (grouped) ----

    /// Run a sequence of mutations. The binary adapter executes them sequentially (best effort,
    /// non-atomic); the crate adapter maps it to a single jj-lib transaction.
    fn transaction(&self, f: &mut dyn FnMut(&mut dyn VcsTx) -> Result<()>) -> Result<()>;

    /// Undo the last operation (`jj undo`). Returns the human description jj printed.
    fn undo(&self) -> Result<String>;

    // ---- workspaces ----

    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>>;
    /// Number of workspaces (cheap; non-snapshotting). Staleness is only possible with >1.
    fn workspace_count(&self) -> Result<usize>;
    fn add_workspace(&self, path: &Path, name: &str, at: &ChangeId) -> Result<()>;
    fn forget_workspace(&self, name: &str) -> Result<()>;
    /// Update a stale workspace's working copy. No-op (Ok) if not stale.
    fn update_stale(&self) -> Result<()>;
    /// True if the current workspace's `@` is stale.
    fn is_stale(&self) -> Result<bool>;

    // ---- remote (git interop lives inside the VCS backend) ----

    fn fetch(&self, remote: &str) -> Result<()>;
    fn push(&self, remote: &str, bookmark: &str, opts: PushOpts) -> Result<()>;
    /// Push all pending bookmark deletions to the remote (`jj git push --deleted`).
    fn push_deleted(&self, remote: &str) -> Result<()>;
    fn add_remote(&self, name: &str, url: &str) -> Result<()>;
    /// Names of configured remotes (excludes the colocated `git` pseudo-remote).
    fn remotes(&self) -> Result<Vec<String>>;
    /// URL of a configured remote, if present.
    fn remote_url(&self, name: &str) -> Result<Option<String>>;
}

/// Mutation handle yielded inside [`Vcs::transaction`].
///
/// Methods that produce a new change return its (stable) [`ChangeId`] so the engine can refer to it
/// in subsequent steps without re-deriving it.
pub trait VcsTx {
    /// `jj commit -m <message>`: finalize `@` into a real commit and open a fresh empty `@`.
    /// Returns the finalized commit's change id (the just-created `@-`).
    fn finalize_working_copy(&mut self, message: &str) -> Result<ChangeId>;

    /// `jj describe <rev> -m <message>`.
    fn describe(&mut self, rev: &ChangeId, message: &str) -> Result<()>;

    /// Squash the working-copy (`@`) changes into `into` (amend). Descendants auto-rebase.
    fn squash_working_into(&mut self, into: &ChangeId) -> Result<()>;

    /// Squash all changes from `from` into `into` (`jj squash --from --into`). `from` is abandoned
    /// when it becomes empty; a bookmark on it moves to its parent (forget it separately).
    fn squash(&mut self, from: &ChangeId, into: &ChangeId) -> Result<()>;

    /// Like [`squash`](VcsTx::squash) but `from` is a revset (e.g. a whole range of commits to
    /// collapse into `into`).
    fn squash_revset(&mut self, from_revset: &str, into: &ChangeId) -> Result<()>;

    /// Rename a local bookmark.
    fn rename_bookmark(&mut self, old: &str, new: &str) -> Result<()>;

    /// `jj new <parent>`: create an empty child of `parent` and make it `@`. Returns its change id.
    fn new_child(&mut self, parent: &ChangeId) -> Result<ChangeId>;

    /// Make `rev` the working copy (`jj edit`), e.g. to resolve a conflict in place.
    fn edit(&mut self, rev: &ChangeId) -> Result<()>;

    /// Create a new bookmark (errors if it exists).
    fn create_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()>;
    /// Create-or-move a bookmark by name (`jj bookmark set`, `-B` for non-fast-forward).
    fn set_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()>;
    /// Delete a bookmark locally and schedule its remote deletion on next push.
    fn delete_bookmark(&mut self, name: &str) -> Result<()>;
    /// Forget a bookmark locally without scheduling a remote deletion.
    fn forget_bookmark(&mut self, name: &str) -> Result<()>;

    /// `jj rebase -s <source> -d <dest>`: rebase source and descendants onto dest.
    fn rebase(&mut self, source: &ChangeId, dest: &ChangeId) -> Result<()>;

    /// `jj abandon <revs...>`: abandon the commits in one op; descendants auto-rebase onto the
    /// abandoned range's parent. Bookmarks on abandoned commits are deleted (JJ_NOTES §2).
    fn abandon(&mut self, revs: &[ChangeId]) -> Result<()>;
}
