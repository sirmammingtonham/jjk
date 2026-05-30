//! Phase 3 acceptance (offline): `submit` idempotency + correct bases against a fake forge, with a
//! real jj repo + local bare git remote so `jj git push` actually runs. The live GitHub smoke test
//! lives in `phase3_live.rs` (ignored by default).

mod common;

use common::{setup_with_remote, write, FakeForge, SharedForge};
use std::sync::Arc;

fn build_three_branch_stack(h: &mut common::RepoWithRemote) {
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
async fn submit_creates_three_correctly_based_prs_and_is_idempotent() {
    let mut h = setup_with_remote();
    build_three_branch_stack(&mut h);

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // First submit: three PRs, based bottom-up.
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.count(), 3, "one PR per tracked branch");
    assert_eq!(fake.pr_for("feat-a").unwrap().base, "main", "bottom based on trunk");
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "feat-a");
    assert_eq!(fake.pr_for("feat-c").unwrap().base, "feat-b");

    let n_a = fake.pr_for("feat-a").unwrap().number;

    // Edit + re-submit: updates in place, no duplicates (FakeForge::create_pr panics on dup head).
    write(h.repo.path(), "a.txt", "a\nedited\n");
    h.engine.commit("feat-a: more").unwrap();
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    assert_eq!(fake.count(), 3, "no duplicate PRs on re-submit");
    assert_eq!(fake.pr_for("feat-a").unwrap().number, n_a, "same PR number");
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "feat-a", "bases preserved");
}

#[tokio::test]
async fn submit_records_pr_numbers_in_state() {
    let mut h = setup_with_remote();
    build_three_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
    for b in ["feat-a", "feat-b", "feat-c"] {
        assert!(
            h.engine.state().pr_of(b).is_some(),
            "{b} PR number persisted in state"
        );
    }
}

#[tokio::test]
async fn submit_skips_untracked_branches() {
    let mut h = setup_with_remote();
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("a").unwrap();
    // Untracked branch on top.
    h.engine.branch_create("scratch", /*tracked=*/ false).unwrap();
    write(&root, "s.txt", "s\n");
    h.engine.commit("s").unwrap();

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    assert_eq!(fake.count(), 1, "only the tracked branch gets a PR");
    assert!(fake.pr_for("feat-a").is_some());
    assert!(fake.pr_for("scratch").is_none());
}

#[tokio::test]
async fn submit_posts_idempotent_navigation_comments() {
    let mut h = setup_with_remote();
    build_three_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    let a = fake.pr_for("feat-a").unwrap().number;
    let b = fake.pr_for("feat-b").unwrap().number;
    let c = fake.pr_for("feat-c").unwrap().number;

    // Each PR gets exactly one nav comment.
    assert_eq!(fake.comments_on(a).len(), 1);
    assert_eq!(fake.comments_on(c).len(), 1);

    // The comment lists the whole stack bottom→top (bare #PR), with ◀ on the PR it lives on,
    // a prominent x/N footer linking jjk, and the hidden marker.
    let body_a = fake.comments_on(a).remove(0);
    assert!(body_a.contains(&format!("- #{a} ◀")), "a marked: {body_a}");
    assert!(body_a.contains(&format!("        - #{c}\n")), "c nested 2 levels: {body_a}");
    // Position x/N, jjk link, and the hidden marker (loose checks — wording may evolve).
    assert!(body_a.contains("1/3"), "position 1/3: {body_a}");
    assert!(
        body_a.contains("[jjk](https://github.com/sirmammingtonham/jjk)"),
        "jjk link: {body_a}"
    );
    assert!(body_a.contains("<!-- jjk:nav -->"));

    let body_c = fake.comments_on(c).remove(0);
    assert!(body_c.contains(&format!("        - #{c} ◀")), "c marked deepest: {body_c}");
    assert!(body_c.contains(&format!("- #{a}\n")), "a at root: {body_c}");
    assert!(body_c.contains("3/3"), "position 3/3: {body_c}");

    // Re-submit: comments are updated in place, not duplicated.
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.comments_on(a).len(), 1, "no duplicate nav comment");
    assert_eq!(fake.comments_on(b).len(), 1);
    assert_eq!(fake.comments_on(c).len(), 1);
}

#[tokio::test]
async fn granular_submit_scopes() {
    use jjk::engine::SubmitScope;
    let mut h = setup_with_remote();
    build_three_branch_stack(&mut h); // feat-a, feat-b, feat-c (currently on feat-c)
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // branch submit: only the current branch (feat-c).
    h.engine.submit(SubmitScope::Branch).await.unwrap();
    assert_eq!(fake.count(), 1, "only current branch submitted");
    assert!(fake.pr_for("feat-c").is_some());
    // feat-c's base is its real downstack branch, not trunk.
    assert_eq!(fake.pr_for("feat-c").unwrap().base, "feat-b");

    // downstack submit from feat-c: feat-c + everything below (feat-a, feat-b).
    h.engine.submit(SubmitScope::Downstack).await.unwrap();
    assert_eq!(fake.count(), 3, "downstack filled in feat-a and feat-b");
    assert_eq!(fake.pr_for("feat-a").unwrap().base, "main");
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "feat-a");

    // No duplicates on re-submit of the whole stack.
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.count(), 3);
}

#[tokio::test]
async fn upstack_submit_only_current_and_above() {
    use jjk::engine::{NavDir, SubmitScope};
    let mut h = setup_with_remote();
    build_three_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // Move to feat-b, then upstack submit → feat-b + feat-c (not feat-a).
    h.engine.navigate(NavDir::Down).unwrap(); // feat-c -> feat-b
    assert_eq!(h.engine.current_branch().unwrap().as_deref(), Some("feat-b"));
    h.engine.submit(SubmitScope::Upstack).await.unwrap();
    assert_eq!(fake.count(), 2, "feat-b and feat-c only");
    assert!(fake.pr_for("feat-b").is_some());
    assert!(fake.pr_for("feat-c").is_some());
    assert!(fake.pr_for("feat-a").is_none(), "feat-a not submitted");
    // feat-b still based on its real downstack (feat-a), even though feat-a wasn't submitted.
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "feat-a");
}
