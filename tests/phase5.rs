//! Phase 5 acceptance: worktrees (jj workspaces) with per-workspace current branch + automatic
//! stale recovery, and stash round-trip. Real temporary jj repos.

mod common;

use common::{init_identity, write};
use jjk::engine::Engine;
use tempfile::TempDir;

async fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).await.unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

#[tokio::test]
async fn stash_round_trips() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "committed\n");
    e.commit("a1").await.unwrap();

    // Uncommitted work in the working copy.
    write(&root, "wip.txt", "work in progress\n");
    e.stash().await.unwrap();
    assert!(
        e.vcs().working_copy().await.unwrap().is_empty,
        "working copy clean after stash"
    );
    assert!(!root.join("wip.txt").exists(), "wip parked off disk");
    // The stash bookmark must NOT leak into the derived stack.
    let stack = e.derive_stack().await.unwrap();
    assert_eq!(
        stack.branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["feat-a"],
        "stash bookmark excluded from the stack"
    );

    // Pop restores it.
    e.stash_pop().await.unwrap();
    assert!(root.join("wip.txt").exists(), "wip restored on disk");
    assert!(
        !e.vcs().working_copy().await.unwrap().is_empty,
        "working copy has the restored changes"
    );
    // No leftover stash bookmarks.
    assert!(
        !e.vcs()
            .bookmarks()
            .await
            .unwrap()
            .iter()
            .any(|b| b.name.starts_with("jjk/stash/")),
        "stash bookmark cleaned up"
    );
}

#[tokio::test]
async fn stash_on_clean_working_copy_is_noop() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "x\n");
    e.commit("a1").await.unwrap();

    let report = e.stash().await.unwrap();
    assert!(report.notes.iter().any(|n| n.contains("nothing to stash")));
}

#[tokio::test]
async fn per_workspace_current_branch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();
    e.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();
    // Main workspace is on feat-b.
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-b"));

    // Add a second workspace started on feat-a.
    let ws_parent = tempfile::tempdir().unwrap();
    let ws2 = ws_parent.path().join("ws2");
    e.worktree_add(&ws2, Some("ws2"), Some("feat-a")).await.unwrap();

    // worktree list shows both with their own current branch.
    let rows = e.worktree_list().await.unwrap();
    let ws2_row = rows.iter().find(|r| r.name == "ws2").unwrap();
    assert_eq!(ws2_row.current_branch.as_deref(), Some("feat-a"));

    // An engine opened *in* ws2 reports feat-a as current (per-workspace @).
    let e2 = Engine::open(&ws2).unwrap();
    assert_eq!(e2.current_branch().await.unwrap().as_deref(), Some("feat-a"));
    // ...while the main workspace still reports feat-b.
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-b"));
}

#[tokio::test]
async fn cross_workspace_stale_is_auto_recovered() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "original\n");
    e.commit("a1").await.unwrap();

    // Second workspace on feat-a.
    let ws_parent = tempfile::tempdir().unwrap();
    let ws2 = ws_parent.path().join("ws2");
    e.worktree_add(&ws2, Some("ws2"), Some("feat-a")).await.unwrap();

    // Rewrite feat-a's CONTENT from the main workspace → ws2's @ goes stale (JJ_NOTES §11).
    e.checkout("feat-a").await.unwrap();
    write(&root, "a.txt", "rewritten\n");
    e.commit_amend(None).await.unwrap();

    // ws2 is now stale.
    let mut e2 = Engine::open(&ws2).unwrap();
    assert!(e2.vcs().is_stale().await.unwrap(), "ws2 should be stale after rewrite");

    // Any command auto-recovers it (ensure_fresh → update-stale), leaving ws2 usable.
    e2.stash().await.unwrap(); // clean WC → no-op, but triggers recovery
    assert!(!e2.vcs().is_stale().await.unwrap(), "ws2 recovered");
    assert_eq!(e2.current_branch().await.unwrap().as_deref(), Some("feat-a"));
}

#[tokio::test]
async fn worktree_remove_forgets_workspace() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();

    let ws_parent = tempfile::tempdir().unwrap();
    let ws2 = ws_parent.path().join("ws2");
    e.worktree_add(&ws2, Some("ws2"), None).await.unwrap();
    assert!(e.worktree_list().await.unwrap().iter().any(|r| r.name == "ws2"));

    e.worktree_remove("ws2").await.unwrap();
    assert!(
        !e.worktree_list().await.unwrap().iter().any(|r| r.name == "ws2"),
        "ws2 no longer tracked"
    );
}
