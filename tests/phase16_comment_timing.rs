//! Navigation-comment timing follows git-spice's two-phase model: open every PR first, then post
//! all nav comments in a single pass once every PR number is known — so each comment is written
//! correct the first time (no placeholder/renumber step). This asserts comments are NOT interleaved
//! with PR creation: every PR is opened before any nav comment is posted.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::SubmitScope;
use std::sync::Arc;

async fn three_branch_stack(h: &mut RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    h.engine.commit("feat-b: first").await.unwrap();
    h.engine.branch_create("feat-c", true).await.unwrap();
    write(&root, "c.txt", "c\n");
    h.engine.commit("feat-c: first").await.unwrap();
}

#[tokio::test]
async fn nav_comments_are_posted_after_all_prs_open() {
    let mut h = setup_with_remote().await;
    three_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(SubmitScope::Stack).await.unwrap();

    let events = fake.events();
    let first_comment = events.iter().position(|e| e.starts_with("create_comment:"));
    let last_pr = events.iter().rposition(|e| e.starts_with("create_pr:"));
    let (first_comment, last_pr) = (first_comment.unwrap(), last_pr.unwrap());

    // Two-phase like git-spice: every PR is opened before any nav comment is posted, so each
    // comment carries the full, correct set of PR numbers on its first write.
    assert!(
        last_pr < first_comment,
        "all PRs should open before any nav comment is posted; events: {events:?}"
    );

    // Sanity: still exactly one comment per PR, no duplicates.
    let a = fake.pr_for("feat-a").unwrap().number;
    let b = fake.pr_for("feat-b").unwrap().number;
    let c = fake.pr_for("feat-c").unwrap().number;
    assert_eq!(fake.comments_on(a).len(), 1);
    assert_eq!(fake.comments_on(b).len(), 1);
    assert_eq!(fake.comments_on(c).len(), 1);

    // And the final content is correct: bottom shows 1/3 with its own marker, top shows 3/3.
    assert!(fake.comments_on(a)[0].contains("1/3"));
    assert!(fake.comments_on(a)[0].contains(&format!("- #{a} ◀")));
    assert!(fake.comments_on(c)[0].contains("3/3"));
}
