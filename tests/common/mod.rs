//! Shared test helpers: jj identity, repo setup (optionally with a local bare git remote), and a
//! fake `Forge` for unit tests.

#![allow(dead_code)]

use jjk::engine::Engine;
use jjk::error::Result;
use jjk::forge::Forge;
use jjk::model::{PrRef, PrState};
use async_trait::async_trait;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, Once};
use tempfile::TempDir;

static IDENTITY: Once = Once::new();

pub fn init_identity() {
    IDENTITY.call_once(|| {
        let dir = std::env::temp_dir().join("jjk-test-jjconfig");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            "[user]\nname = \"jjk test\"\nemail = \"jjk-test@example.com\"\n",
        )
        .unwrap();
        std::env::set_var("JJ_CONFIG", &cfg);
    });
}

pub fn write(root: &Path, name: &str, contents: &str) {
    std::fs::write(root.join(name), contents).unwrap();
}

/// A jjk repo with a **local bare git remote** wired as `origin`, so `jj git push` succeeds in
/// tests without touching the network.
pub struct RepoWithRemote {
    pub repo: TempDir,
    pub remote: TempDir,
    pub engine: Engine,
}

pub fn setup_with_remote() -> RepoWithRemote {
    init_identity();
    let remote = tempfile::tempdir().unwrap();
    let remote_path = remote.path().join("origin.git");
    let ok = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&remote_path)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init --bare failed");

    let repo = tempfile::tempdir().unwrap();
    Engine::repo_init(repo.path(), Some("main".into()), Some("origin".into())).unwrap();
    let engine = Engine::open(repo.path()).unwrap();
    engine
        .vcs()
        .add_remote("origin", remote_path.to_str().unwrap())
        .unwrap();

    RepoWithRemote {
        repo,
        remote,
        engine,
    }
}

/// In-memory `Forge` used by unit tests. No network.
#[derive(Default)]
pub struct FakeForge {
    inner: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    next: u64,
    prs: Vec<PrRef>,
}

impl FakeForge {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeState { next: 1, prs: vec![] }),
        }
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().prs.len()
    }

    pub fn pr_for(&self, branch: &str) -> Option<PrRef> {
        self.inner
            .lock()
            .unwrap()
            .prs
            .iter()
            .find(|p| p.head == branch)
            .cloned()
    }

    /// Mark a branch's PR merged (for sync tests).
    pub fn set_merged(&self, branch: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(p) = st.prs.iter_mut().find(|p| p.head == branch) {
            p.state = PrState::Merged;
        }
    }
}

/// Newtype so a test can keep an inspectable `Arc<FakeForge>` while the engine owns a
/// `Box<dyn Forge>` (the orphan rule forbids `impl Forge for Arc<FakeForge>` in the test crate).
pub struct SharedForge(pub std::sync::Arc<FakeForge>);

#[async_trait]
impl Forge for SharedForge {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>> {
        self.0.get_pr(branch).await
    }
    async fn create_pr(&self, head: &str, base: &str, title: &str, body: &str) -> Result<PrRef> {
        self.0.create_pr(head, base, title, body).await
    }
    async fn update_pr(&self, pr: u64, base: Option<&str>, body: Option<&str>) -> Result<()> {
        self.0.update_pr(pr, base, body).await
    }
    async fn is_merged(&self, pr: u64) -> Result<bool> {
        self.0.is_merged(pr).await
    }
}

#[async_trait]
impl Forge for FakeForge {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>> {
        Ok(self.pr_for(branch))
    }

    async fn create_pr(&self, head: &str, base: &str, title: &str, body: &str) -> Result<PrRef> {
        let mut st = self.inner.lock().unwrap();
        // Idempotency guard: never create a duplicate for the same head.
        if st.prs.iter().any(|p| p.head == head) {
            panic!("create_pr called for existing head '{head}' — not idempotent");
        }
        let _ = body;
        let number = st.next;
        st.next += 1;
        let pr = PrRef {
            number,
            head: head.to_string(),
            base: base.to_string(),
            state: PrState::Open,
            url: format!("https://example.test/pr/{number}"),
            title: title.to_string(),
        };
        st.prs.push(pr.clone());
        Ok(pr)
    }

    async fn update_pr(&self, pr: u64, base: Option<&str>, _body: Option<&str>) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        if let Some(p) = st.prs.iter_mut().find(|p| p.number == pr) {
            if let Some(b) = base {
                p.base = b.to_string();
            }
        }
        Ok(())
    }

    async fn is_merged(&self, pr: u64) -> Result<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .prs
            .iter()
            .find(|p| p.number == pr)
            .map(|p| p.state == PrState::Merged)
            .unwrap_or(false))
    }
}
