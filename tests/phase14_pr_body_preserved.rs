//! Guarantee: jjk sets a PR's description once at creation and then NEVER overwrites it. A
//! re-submit (which retargets the base) and a sync (which retargets bases of survivors) must leave
//! an author-edited body untouched. Regression for bodies being clobbered on update.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::SubmitScope;
use std::sync::Arc;

async fn two_branch_stack(h: &mut RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    h.engine.commit("feat-b: first").await.unwrap();
}

#[tokio::test]
async fn resubmit_does_not_overwrite_an_edited_pr_body() {
    let mut h = setup_with_remote().await;
    two_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(SubmitScope::Stack).await.unwrap();

    // The author rewrites feat-a's description by hand.
    let edited = "## My hand-written description\n\nDo not clobber me.";
    fake.set_body("feat-a", edited);

    // Add a commit and re-submit the whole stack (this retargets/updates existing PRs).
    write(h.repo.path(), "a.txt", "a\nmore\n");
    h.engine.commit("feat-a: more").await.unwrap();
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    assert_eq!(
        fake.body_for("feat-a").as_deref(),
        Some(edited),
        "re-submit must not touch the author's PR description"
    );
}

#[tokio::test]
async fn sync_does_not_overwrite_an_edited_pr_body() {
    let mut h = setup_with_remote().await;
    two_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    let edited = "Edited body that sync must preserve.";
    fake.set_body("feat-b", edited);

    // Sync (retargets bases). Must not rewrite the description.
    h.engine.sync(true).await.unwrap();

    assert_eq!(
        fake.body_for("feat-b").as_deref(),
        Some(edited),
        "sync must not touch the author's PR description"
    );
}
