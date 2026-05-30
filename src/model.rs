//! Backend-neutral domain types that cross the `Vcs`/`Forge` trait boundary.
//!
//! Nothing in here may reference a concrete backend (no `jj-lib` type, no CLI/JSON shape).
//! Adapters translate *into* these types; the `engine` speaks only these.

use std::fmt;

/// A jj change id — **stable across rewrites** (amend/rebase). The state map is keyed on this.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeId(pub String);

/// A git commit id (sha) — changes on every rewrite. Used for force-push lease reasoning, display.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CommitId(pub String);

impl ChangeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Short form for display (jj resolves unambiguous prefixes).
    pub fn short(&self) -> &str {
        let n = self.0.len().min(8);
        &self.0[..n]
    }
}

impl CommitId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn short(&self) -> &str {
        let n = self.0.len().min(8);
        &self.0[..n]
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({})", self.short())
    }
}
impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Debug for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitId({})", self.short())
    }
}

/// A remote-tracking bookmark reference, e.g. `feat-a@origin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRef {
    pub name: String,
    pub remote: String,
}

/// A single commit as seen through the trait boundary.
#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub change_id: ChangeId,
    pub commit_id: CommitId,
    pub parents: Vec<ChangeId>,
    /// Local bookmarks pointing at this commit (never includes the `@git` pseudo-remote).
    pub local_bookmarks: Vec<String>,
    /// Remote-tracking bookmarks pointing here (the `git` pseudo-remote is filtered out).
    pub remote_bookmarks: Vec<RemoteRef>,
    pub description: String,
    /// Relative commit time for display, e.g. "2 days ago".
    pub time_ago: String,
    pub is_empty: bool,
    pub has_conflict: bool,
    /// True if this commit is the working-copy commit of the *current* workspace (`@`).
    pub is_working_copy: bool,
    /// True if this commit is immutable (ancestor of `trunk()` / in `immutable_heads()`).
    pub is_immutable: bool,
}

impl CommitInfo {
    /// First line of the description (subject), trimmed.
    pub fn subject(&self) -> &str {
        self.description.lines().next().unwrap_or("").trim()
    }
}

/// A local bookmark and its target change.
#[derive(Clone, Debug)]
pub struct Bookmark {
    pub name: String,
    pub target: ChangeId,
}

/// A jj workspace (≈ git worktree).
#[derive(Clone, Debug)]
pub struct WorkspaceInfo {
    pub name: String,
    pub working_copy: ChangeId,
    pub is_stale: bool,
}

/// What a backend can promise about how it executes mutations.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    /// Mutations grouped in a `transaction` are applied atomically (single op-log entry).
    pub atomic_transactions: bool,
    /// Runs in-process (no subprocess spawn per call).
    pub in_process: bool,
}

/// A pull-request reference as seen through the `Forge` boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRef {
    pub number: u64,
    pub head: String,
    pub base: String,
    pub state: PrState,
    pub url: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

impl fmt::Display for PrState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PrState::Open => "open",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        };
        f.write_str(s)
    }
}
