//! The derived stack model. **Never persisted** (ARCH §3.4) — reconstructed from jj at runtime.

use crate::model::{ChangeId, CommitInfo};

/// One branch (≈ one PR): a contiguous range of commits with a bookmark riding the tip.
#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub tip: ChangeId,
    /// Commits in this branch's range, ordered bottom → top (tip last).
    pub commits: Vec<CommitInfo>,
    pub pr: Option<u64>,
    pub tracked: bool,
}

impl Branch {
    pub fn has_conflict(&self) -> bool {
        self.commits.iter().any(|c| c.has_conflict)
    }
    pub fn commit_count(&self) -> usize {
        self.commits.len()
    }
}

/// The stack containing the current working copy, ordered bottom (nearest trunk) → top.
#[derive(Clone, Debug)]
pub struct Stack {
    pub trunk_name: String,
    pub trunk: ChangeId,
    pub branches: Vec<Branch>,
    /// Name of the branch the working copy currently sits on (`None` = on trunk).
    pub current: Option<String>,
}

impl Stack {
    pub fn branch(&self, name: &str) -> Option<&Branch> {
        self.branches.iter().find(|b| b.name == name)
    }

    /// Index of a branch in bottom→top order.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.branches.iter().position(|b| b.name == name)
    }

    /// The branch immediately below `name` (toward trunk), or None if it's the bottom.
    pub fn downstack(&self, name: &str) -> Option<&Branch> {
        let i = self.index_of(name)?;
        if i == 0 {
            None
        } else {
            self.branches.get(i - 1)
        }
    }

    /// The branch immediately above `name` (away from trunk), or None if it's the top.
    pub fn upstack(&self, name: &str) -> Option<&Branch> {
        let i = self.index_of(name)?;
        self.branches.get(i + 1)
    }

    pub fn bottom(&self) -> Option<&Branch> {
        self.branches.first()
    }
    pub fn top(&self) -> Option<&Branch> {
        self.branches.last()
    }
}
