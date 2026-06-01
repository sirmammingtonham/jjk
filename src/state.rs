//! Persistent state in `.jj/jjk/state.toml`. We persist **only what jj cannot derive** (ARCH §3.4):
//! config (trunk bookmark, remote, backends) and the branch→PR map. The stack graph itself is
//! always derived from jj at runtime, never stored.

use crate::error::{JjkError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Trunk bookmark name (e.g. `main`). Stored because `trunk()` falls back to root() with no
    /// remote (JJ_NOTES §8).
    pub trunk: String,
    /// Remote name for fetch/push (e.g. `origin`).
    pub remote: String,
    #[serde(default = "default_forge")]
    pub forge: String,
    #[serde(default = "default_vcs_backend")]
    pub vcs_backend: String,
    #[serde(default = "default_forge_backend")]
    pub forge_backend: String,
}

fn default_forge() -> String {
    "github".into()
}
fn default_vcs_backend() -> String {
    "jj_cli".into()
}
fn default_forge_backend() -> String {
    "gh_cli".into()
}

/// Per-branch persisted record. Keyed by branch name; validated against the jj change id so it
/// survives amends/rebases and is dropped if the change vanishes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchEntry {
    /// jj change id of the branch tip when last recorded (stable across rewrites).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// PR number on the forge, once submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// Forge id of this branch's stack-navigation comment, once posted. Cached so a later
    /// submit/sync edits it in place instead of paginating the PR's comments to rediscover it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nav_comment_id: Option<u64>,
    /// Whether this branch is stack-tracked (created via `branch create`/`track`).
    #[serde(default)]
    pub tracked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub config: Config,
    #[serde(default)]
    pub branches: BTreeMap<String, BranchEntry>,
}

impl State {
    /// `.jj/jjk/state.toml` under a repo root.
    pub fn path_for(root: &Path) -> PathBuf {
        root.join(".jj").join("jjk").join("state.toml")
    }

    pub fn new(trunk: impl Into<String>, remote: impl Into<String>) -> Self {
        State {
            config: Config {
                trunk: trunk.into(),
                remote: remote.into(),
                forge: default_forge(),
                vcs_backend: default_vcs_backend(),
                forge_backend: default_forge_backend(),
            },
            branches: BTreeMap::new(),
        }
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path_for(root);
        let text = std::fs::read_to_string(&path).map_err(|_| JjkError::NotInitialized)?;
        let state: State =
            toml::from_str(&text).map_err(|e| JjkError::Msg(format!("corrupt state.toml: {e}")))?;
        Ok(state)
    }

    pub fn exists(root: &Path) -> bool {
        Self::path_for(root).exists()
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path_for(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| JjkError::Msg(format!("failed to serialize state: {e}")))?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Mutable access to a branch record, creating it if absent.
    pub fn branch_mut(&mut self, name: &str) -> &mut BranchEntry {
        self.branches.entry(name.to_string()).or_default()
    }

    pub fn is_tracked(&self, name: &str) -> bool {
        self.branches.get(name).map(|b| b.tracked).unwrap_or(false)
    }

    pub fn pr_of(&self, name: &str) -> Option<u64> {
        self.branches.get(name).and_then(|b| b.pr)
    }

    /// Cached forge id of a branch's stack-navigation comment, if known.
    pub fn nav_comment_of(&self, name: &str) -> Option<u64> {
        self.branches.get(name).and_then(|b| b.nav_comment_id)
    }
}
