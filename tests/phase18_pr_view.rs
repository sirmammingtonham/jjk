//! `jjk pr view` opens the current branch's PR in the browser (or, with --print, returns its URL).
//! Errors clearly when not on a branch or the branch has no PR yet.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::SubmitScope;
use std::sync::Arc;

async fn one_branch(h: &mut RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").await.unwrap();
}

#[tokio::test]
async fn pr_view_print_returns_the_url() {
    let mut h = setup_with_remote().await;
    one_branch(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    let pr = fake.pr_for("feat-a").unwrap().number;

    let url = h.engine.pr_view(/*print=*/ true).await.unwrap();
    assert_eq!(url.as_deref(), Some(format!("https://example.test/pr/{pr}").as_str()));
    // --print must NOT open a browser.
    assert!(fake.events().iter().any(|e| e == &format!("view_pr:{pr}:print")));
    assert!(!fake.events().iter().any(|e| e.ends_with(":web")));
}

#[tokio::test]
async fn pr_view_opens_browser_by_default() {
    let mut h = setup_with_remote().await;
    one_branch(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    let pr = fake.pr_for("feat-a").unwrap().number;

    let url = h.engine.pr_view(/*print=*/ false).await.unwrap();
    assert_eq!(url, None, "browser path returns no URL to print");
    assert!(fake.events().iter().any(|e| e == &format!("view_pr:{pr}:web")), "opened in browser");
}

#[tokio::test]
async fn pr_view_errors_when_branch_has_no_pr() {
    let mut h = setup_with_remote().await;
    one_branch(&mut h).await; // tracked branch, but never submitted
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    let err = h.engine.pr_view(true).await.unwrap_err().to_string();
    assert!(err.contains("no PR"), "expected a 'no PR' error, got: {err}");
}

#[tokio::test]
async fn pr_view_errors_when_not_on_a_branch() {
    let mut h = setup_with_remote().await;
    // On trunk, not on any stacked branch.
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    assert!(h.engine.current_branch().await.unwrap().is_none(), "precondition: on trunk");
    assert!(h.engine.pr_view(true).await.is_err(), "should error when not on a branch");
}
