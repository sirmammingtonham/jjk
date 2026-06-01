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
    comments: Vec<FakeComment>,
    next_comment: u64,
    /// Latest body seen per head (on create or update), for assertions.
    bodies: std::collections::HashMap<String, String>,
    /// Draft flag captured at create time, per head.
    drafts: std::collections::HashMap<String, bool>,
}

#[derive(Clone)]
struct FakeComment {
    id: u64,
    pr: u64,
    body: String,
}

impl FakeForge {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeState {
                next: 1,
                next_comment: 1,
                ..Default::default()
            }),
        }
    }

    /// All comment bodies on a PR (for assertions).
    pub fn comments_on(&self, pr: u64) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .comments
            .iter()
            .filter(|c| c.pr == pr)
            .map(|c| c.body.clone())
            .collect()
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

    /// Latest PR body seen for a branch's head (create or update).
    pub fn body_for(&self, branch: &str) -> Option<String> {
        self.inner.lock().unwrap().bodies.get(branch).cloned()
    }

    /// Whether the PR for a branch was created as a draft.
    pub fn draft_for(&self, branch: &str) -> Option<bool> {
        self.inner.lock().unwrap().drafts.get(branch).copied()
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
    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrRef> {
        self.0.create_pr(head, base, title, body, draft).await
    }
    async fn update_pr(&self, pr: u64, base: Option<&str>, body: Option<&str>) -> Result<()> {
        self.0.update_pr(pr, base, body).await
    }
    async fn is_merged(&self, pr: u64) -> Result<bool> {
        self.0.is_merged(pr).await
    }
    async fn find_comment(&self, pr: u64, marker: &str) -> Result<Option<u64>> {
        self.0.find_comment(pr, marker).await
    }
    async fn create_comment(&self, pr: u64, body: &str) -> Result<u64> {
        self.0.create_comment(pr, body).await
    }
    async fn update_comment(&self, comment_id: u64, body: &str) -> Result<()> {
        self.0.update_comment(comment_id, body).await
    }
}

#[async_trait]
impl Forge for FakeForge {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>> {
        Ok(self.pr_for(branch))
    }

    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrRef> {
        let mut st = self.inner.lock().unwrap();
        // Idempotency guard: never create a duplicate for the same head.
        if st.prs.iter().any(|p| p.head == head) {
            panic!("create_pr called for existing head '{head}' — not idempotent");
        }
        st.bodies.insert(head.to_string(), body.to_string());
        st.drafts.insert(head.to_string(), draft);
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

    async fn update_pr(&self, pr: u64, base: Option<&str>, body: Option<&str>) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        let head = st.prs.iter().find(|p| p.number == pr).map(|p| p.head.clone());
        if let Some(p) = st.prs.iter_mut().find(|p| p.number == pr) {
            if let Some(b) = base {
                p.base = b.to_string();
            }
        }
        if let (Some(h), Some(b)) = (head, body) {
            st.bodies.insert(h, b.to_string());
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

    async fn find_comment(&self, pr: u64, marker: &str) -> Result<Option<u64>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .comments
            .iter()
            .find(|c| c.pr == pr && c.body.contains(marker))
            .map(|c| c.id))
    }

    async fn create_comment(&self, pr: u64, body: &str) -> Result<u64> {
        let mut st = self.inner.lock().unwrap();
        let id = st.next_comment;
        st.next_comment += 1;
        st.comments.push(FakeComment {
            id,
            pr,
            body: body.to_string(),
        });
        Ok(id)
    }

    async fn update_comment(&self, comment_id: u64, body: &str) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        if let Some(c) = st.comments.iter_mut().find(|c| c.id == comment_id) {
            c.body = body.to_string();
        }
        Ok(())
    }
}
