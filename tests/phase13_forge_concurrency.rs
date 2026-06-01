//! Guards the `submit`/`sync` forge-call parallelization: the independent reads now run
//! concurrently (`futures::try_join_all`), and correctness relies on results being mapped back by
//! position, not completion order. This wraps the fake forge so calls complete in *inverted* order
//! (earlier-issued calls finish last), then asserts submit/sync still produce the right PRs, bases,
//! nav comments, and merged-reconciliation — i.e. interleaving doesn't corrupt the mapping.

mod common;

use async_trait::async_trait;
use common::{setup_with_remote, write, FakeForge};
use jjk::error::Result;
use jjk::forge::Forge;
use jjk::model::PrRef;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Wraps `FakeForge` and staggers each call so earlier-issued ones yield more and thus resolve
/// *after* later ones — forcing `try_join_all` to complete its children out of issue order.
struct ReorderForge {
    inner: Arc<FakeForge>,
    seq: AtomicUsize,
}

impl ReorderForge {
    fn new(inner: Arc<FakeForge>) -> Self {
        Self { inner, seq: AtomicUsize::new(0) }
    }
    async fn stagger(&self) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        // Earlier calls (smaller n) yield more times → they finish later.
        for _ in 0..(32usize.saturating_sub(n % 32)) {
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait]
impl Forge for ReorderForge {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>> {
        self.stagger().await;
        self.inner.get_pr(branch).await
    }
    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrRef> {
        self.stagger().await;
        self.inner.create_pr(head, base, title, body, draft).await
    }
    async fn update_pr(&self, pr: u64, base: Option<&str>) -> Result<()> {
        self.stagger().await;
        self.inner.update_pr(pr, base).await
    }
    async fn is_merged(&self, pr: u64) -> Result<bool> {
        self.stagger().await;
        self.inner.is_merged(pr).await
    }
    async fn view_pr(&self, pr: u64, web: bool) -> Result<Option<String>> {
        self.stagger().await;
        self.inner.view_pr(pr, web).await
    }
    async fn find_comment(&self, pr: u64, marker: &str) -> Result<Option<u64>> {
        self.stagger().await;
        self.inner.find_comment(pr, marker).await
    }
    async fn create_comment(&self, pr: u64, body: &str) -> Result<u64> {
        self.stagger().await;
        self.inner.create_comment(pr, body).await
    }
    async fn update_comment(&self, comment_id: u64, body: &str) -> Result<()> {
        self.stagger().await;
        self.inner.update_comment(comment_id, body).await
    }
}

fn three_branch_stack(h: &mut common::RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").unwrap();
    h.engine.branch_create("feat-b", true).unwrap();
    write(&root, "b.txt", "b\n");
    h.engine.commit("feat-b: first").unwrap();
    h.engine.branch_create("feat-c", true).unwrap();
    write(&root, "c.txt", "c\n");
    h.engine.commit("feat-c: first").unwrap();
}

#[tokio::test]
async fn submit_maps_results_correctly_under_out_of_order_completion() {
    let mut h = setup_with_remote();
    three_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(ReorderForge::new(fake.clone())));

    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Each branch got its own PR with the correct downstack base — proves the concurrent get_pr /
    // create_pr results were mapped back to the right branch, not whoever finished first.
    assert_eq!(fake.count(), 3);
    assert_eq!(fake.pr_for("feat-a").unwrap().base, "main");
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "feat-a");
    assert_eq!(fake.pr_for("feat-c").unwrap().base, "feat-b");

    // Exactly one nav comment per PR, with the ◀ marker on the right PR (concurrent upsert).
    let a = fake.pr_for("feat-a").unwrap().number;
    let c = fake.pr_for("feat-c").unwrap().number;
    assert_eq!(fake.comments_on(a).len(), 1);
    assert_eq!(fake.comments_on(c).len(), 1);
    assert!(fake.comments_on(a)[0].contains(&format!("- #{a} ◀")));
    assert!(fake.comments_on(c)[0].contains(&format!("        - #{c} ◀")));

    // Re-submit under the same staggering: still idempotent (no duplicate PRs or comments).
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.count(), 3, "no duplicate PRs");
    assert_eq!(fake.comments_on(a).len(), 1, "no duplicate nav comment");
}

#[tokio::test]
async fn sync_merged_detection_maps_correctly_under_out_of_order_completion() {
    let mut h = setup_with_remote();
    three_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(ReorderForge::new(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Mark the MIDDLE branch's PR merged. The concurrent is_merged batch must attribute the merged
    // flag to feat-b specifically (not whichever call finished first).
    fake.set_merged("feat-b");

    // --no-push so the test stays local; reconciliation (which consumes the is_merged results)
    // still runs.
    let report = h.engine.sync(false).await.unwrap();
    assert!(
        report.notes.iter().any(|n| n.contains("merged: feat-b")),
        "should detect exactly feat-b merged: {:?}",
        report.notes
    );
    assert!(
        !report.notes.iter().any(|n| n.contains("feat-a") && n.contains("merged:")),
        "must not misattribute the merge to another branch"
    );
}
