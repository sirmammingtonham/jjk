//! The `Forge` **port** (trait) for GitHub PR operations. Async (octocrab is async; the `gh_cli`
//! adapter wraps subprocess calls to satisfy it). Fully implemented in Phase 3.

pub mod gh_cli;

use crate::error::Result;
use crate::model::PrRef;
use async_trait::async_trait;

#[async_trait]
pub trait Forge: Send + Sync {
    /// The open/merged/closed PR whose head is `branch`, if any.
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>>;
    async fn create_pr(&self, head: &str, base: &str, title: &str, body: &str) -> Result<PrRef>;
    async fn update_pr(&self, pr: u64, base: Option<&str>, body: Option<&str>) -> Result<()>;
    async fn is_merged(&self, pr: u64) -> Result<bool>;
}
