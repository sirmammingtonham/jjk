//! git-spice persists each PR's navigation-comment id in local state, so later submits/syncs edit
//! the comment in place instead of paginating the PR's comments to rediscover it. We do the same:
//! the comment id is cached in state, the next run skips `find_comment`, and a stale cached id
//! (comment deleted by the author) self-heals by recreating the comment.

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
async fn comment_id_is_cached_and_reused_to_skip_find() {
    let mut h = setup_with_remote().await;
    two_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // First submit: no cached ids yet, so it must find_comment (paginate) before creating.
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert!(fake.count_events("find_comment") >= 2, "first submit looks up comments");
    // The comment ids are now persisted in state.
    assert!(h.engine.state().nav_comment_of("feat-a").is_some(), "comment id cached for feat-a");
    assert!(h.engine.state().nav_comment_of("feat-b").is_some(), "comment id cached for feat-b");

    let finds_after_first = fake.count_events("find_comment");

    // Second submit: cached ids let it update in place — NO new find_comment calls.
    write(h.repo.path(), "a.txt", "a\nmore\n");
    h.engine.commit("feat-a: more").await.unwrap();
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    assert_eq!(
        fake.count_events("find_comment"),
        finds_after_first,
        "re-submit must reuse cached comment ids, not paginate to find them again"
    );
    // Still one comment per PR.
    let a = fake.pr_for("feat-a").unwrap().number;
    assert_eq!(fake.comments_on(a).len(), 1);
}

#[tokio::test]
async fn stale_cached_comment_id_self_heals() {
    let mut h = setup_with_remote().await;
    two_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    // The author deletes the nav comment on feat-a; its id is still cached in state (now stale).
    let a = fake.pr_for("feat-a").unwrap().number;
    fake.delete_comments_on(a);
    assert_eq!(fake.comments_on(a).len(), 0);

    // Re-submit: the cached-id update fails, so it falls back to find→create and a fresh comment
    // appears (no error).
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.comments_on(a).len(), 1, "deleted nav comment was recreated");
}
