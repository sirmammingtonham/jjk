//! Phase 1 acceptance: drive the `Engine` against a **real temporary jj repo** (no mocks).
//! Covers the walkthrough steps 2–6 from PROMPT.md.

use jjk::engine::{Engine, NavDir};
use std::path::Path;
use std::sync::Once;
use tempfile::TempDir;

static IDENTITY: Once = Once::new();

/// Point jj at a throwaway config providing a user identity (CI has none configured).
fn init_identity() {
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

async fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).await.unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

fn write(root: &Path, name: &str, contents: &str) {
    std::fs::write(root.join(name), contents).unwrap();
}

#[tokio::test]
async fn step2_two_commits_in_one_branch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a1\n");
    e.commit("a1").await.unwrap();
    write(&root, "a.txt", "a1\na2\n");
    e.commit("a2").await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let feat_a = stack.branch("feat-a").expect("feat-a in stack");
    assert_eq!(feat_a.commit_count(), 2, "feat-a should have two commits");
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));
}

#[tokio::test]
async fn step3_stack_of_two_branches() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();

    e.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-a", "feat-b"], "bottom→top order");
    assert_eq!(stack.branch("feat-b").unwrap().commit_count(), 1);
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-b"));
}

#[tokio::test]
async fn step4_midstack_commit_restacks_upstack() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();
    e.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();

    // Go down to feat-a and commit a new mid-stack change.
    e.navigate(NavDir::Down).await.unwrap();
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));
    write(&root, "a.txt", "a\nmid\n");
    e.commit("a-mid").await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let feat_a = stack.branch("feat-a").unwrap();
    let feat_b = stack.branch("feat-b").unwrap();
    assert_eq!(feat_a.commit_count(), 2, "feat-a grew to two commits");
    // feat-b's bottom commit must now be parented on feat-a's new tip (auto-restacked).
    let feat_b_bottom = &feat_b.commits[0];
    assert!(
        feat_b_bottom.parents.contains(&feat_a.tip),
        "feat-b should ride feat-a's new tip (got parents {:?}, tip {:?})",
        feat_b_bottom.parents,
        feat_a.tip
    );
}

#[tokio::test]
async fn step4b_amend_restacks_upstack() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();
    e.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();

    e.navigate(NavDir::Down).await.unwrap();
    write(&root, "a.txt", "a-amended\n");
    e.commit_amend(None).await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let feat_a = stack.branch("feat-a").unwrap();
    let feat_b = stack.branch("feat-b").unwrap();
    assert_eq!(feat_a.commit_count(), 1, "amend keeps one commit");
    assert!(
        feat_b.commits[0].parents.contains(&feat_a.tip),
        "feat-b auto-rebased onto amended feat-a"
    );
}

#[tokio::test]
async fn step5_switching_with_uncommitted_changes_never_errors() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();
    e.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();

    // Repeatedly switch with uncommitted changes present — must never error.
    for _ in 0..3 {
        e.checkout("feat-a").await.unwrap();
        write(&root, "wip.txt", "work in progress\n");
        e.checkout("feat-b").await.unwrap();
        e.checkout("feat-a").await.unwrap();
    }
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));
}

#[tokio::test]
async fn step6_undo_reverses_last_operation() {
    let (tmp, mut e) = setup().await;
    let _ = tmp;

    e.branch_create("feat-a", true).await.unwrap();
    assert!(
        e.vcs().bookmarks().await.unwrap().iter().any(|b| b.name == "feat-a"),
        "feat-a exists after create"
    );

    e.undo().await.unwrap();
    assert!(
        !e.vcs().bookmarks().await.unwrap().iter().any(|b| b.name == "feat-a"),
        "feat-a gone after undo"
    );
}

#[tokio::test]
async fn navigation_top_and_bottom() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();
    e.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();

    e.navigate(NavDir::Bottom).await.unwrap();
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));
    e.navigate(NavDir::Top).await.unwrap();
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-b"));
}
