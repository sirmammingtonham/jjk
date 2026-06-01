//! Problem C: the stack-navigation comment should be posted right after each PR opens (so it's
//! likely the first comment, before CI bots), and earlier PRs updated as the stack grows — rather
//! than deferring all comments to a single pass after every PR is open. This asserts the comment
//! posting is *interleaved* with PR creation.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::SubmitScope;
use std::sync::Arc;

fn three_branch_stack(h: &mut RepoWithRemote) {
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
async fn nav_comments_are_posted_while_the_stack_is_still_opening() {
    let mut h = setup_with_remote();
    three_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(SubmitScope::Stack).await.unwrap();

    let events = fake.events();
    let first_comment = events.iter().position(|e| e.starts_with("create_comment:"));
    let last_pr = events.iter().rposition(|e| e.starts_with("create_pr:"));
    let (first_comment, last_pr) = (first_comment.unwrap(), last_pr.unwrap());

    // A nav comment is posted before the final PR is even created — i.e. comments are interleaved
    // with opening PRs, not deferred until the whole stack is open. (Old behavior: every
    // create_comment came after every create_pr.)
    assert!(
        first_comment < last_pr,
        "expected a nav comment before the last PR opens; events: {events:?}"
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
