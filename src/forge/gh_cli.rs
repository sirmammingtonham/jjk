//! GitHub forge adapter shelling out to the `gh` CLI. Implemented in Phase 3.

use crate::error::Result;
use crate::forge::Forge;
use crate::model::PrRef;
use async_trait::async_trait;

/// Forge adapter backed by the `gh` binary. Reuses the user's existing `gh auth` token.
pub struct GhCli {
    #[allow(dead_code)]
    remote: String,
}

impl GhCli {
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
        }
    }
}

#[async_trait]
impl Forge for GhCli {
    async fn get_pr(&self, _branch: &str) -> Result<Option<PrRef>> {
        anyhow::bail!("forge operations are implemented in Phase 3")
    }
    async fn create_pr(&self, _head: &str, _base: &str, _title: &str, _body: &str) -> Result<PrRef> {
        anyhow::bail!("forge operations are implemented in Phase 3")
    }
    async fn update_pr(&self, _pr: u64, _base: Option<&str>, _body: Option<&str>) -> Result<()> {
        anyhow::bail!("forge operations are implemented in Phase 3")
    }
    async fn is_merged(&self, _pr: u64) -> Result<bool> {
        anyhow::bail!("forge operations are implemented in Phase 3")
    }
}
