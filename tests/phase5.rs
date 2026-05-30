//! Phase 5 acceptance: worktrees (jj workspaces) with per-workspace current branch + automatic
//! stale recovery, and stash round-trip. Real temporary jj repos.

mod common;

use common::{init_identity, write};
use jjk::engine::Engine;
use tempfile::TempDir;

fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

#[test]
fn stash_round_trips() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "committed\n");
    e.commit("a1").unwrap();

    // Uncommitted work in the working copy.
    write(&root, "wip.txt", "work in progress\n");
    e.stash().unwrap();
    assert!(
        e.vcs().working_copy().unwrap().is_empty,
        "working copy clean after stash"
    );
    assert!(!root.join("wip.txt").exists(), "wip parked off disk");
    // The stash bookmark must NOT leak into the derived stack.
    let stack = e.derive_stack().unwrap();
    assert_eq!(
        stack.branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["feat-a"],
        "stash bookmark excluded from the stack"
    );

    // Pop restores it.
    e.stash_pop().unwrap();
    assert!(root.join("wip.txt").exists(), "wip restored on disk");
    assert!(
        !e.vcs().working_copy().unwrap().is_empty,
        "working copy has the restored changes"
    );
    // No leftover stash bookmarks.
    assert!(
        !e.vcs()
            .bookmarks()
            .unwrap()
            .iter()
            .any(|b| b.name.starts_with("jjk/stash/")),
        "stash bookmark cleaned up"
    );
}

#[test]
fn stash_on_clean_working_copy_is_noop() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "x\n");
    e.commit("a1").unwrap();

    let report = e.stash().unwrap();
    assert!(report.notes.iter().any(|n| n.contains("nothing to stash")));
}

#[test]
fn per_workspace_current_branch() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").unwrap();
    e.branch_create("feat-b", true).unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("b1").unwrap();
    // Main workspace is on feat-b.
    assert_eq!(e.current_branch().unwrap().as_deref(), Some("feat-b"));

    // Add a second workspace started on feat-a.
    let ws_parent = tempfile::tempdir().unwrap();
    let ws2 = ws_parent.path().join("ws2");
    e.worktree_add(&ws2, Some("ws2"), Some("feat-a")).unwrap();

    // worktree list shows both with their own current branch.
    let rows = e.worktree_list().unwrap();
    let ws2_row = rows.iter().find(|r| r.name == "ws2").unwrap();
    assert_eq!(ws2_row.current_branch.as_deref(), Some("feat-a"));

    // An engine opened *in* ws2 reports feat-a as current (per-workspace @).
    let e2 = Engine::open(&ws2).unwrap();
    assert_eq!(e2.current_branch().unwrap().as_deref(), Some("feat-a"));
    // ...while the main workspace still reports feat-b.
    assert_eq!(e.current_branch().unwrap().as_deref(), Some("feat-b"));
}

#[test]
fn cross_workspace_stale_is_auto_recovered() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "original\n");
    e.commit("a1").unwrap();

    // Second workspace on feat-a.
    let ws_parent = tempfile::tempdir().unwrap();
    let ws2 = ws_parent.path().join("ws2");
    e.worktree_add(&ws2, Some("ws2"), Some("feat-a")).unwrap();

    // Rewrite feat-a's CONTENT from the main workspace → ws2's @ goes stale (JJ_NOTES §11).
    e.checkout("feat-a").unwrap();
    write(&root, "a.txt", "rewritten\n");
    e.commit_amend(None).unwrap();

    // ws2 is now stale.
    let mut e2 = Engine::open(&ws2).unwrap();
    assert!(e2.vcs().is_stale().unwrap(), "ws2 should be stale after rewrite");

    // Any command auto-recovers it (ensure_fresh → update-stale), leaving ws2 usable.
    e2.stash().unwrap(); // clean WC → no-op, but triggers recovery
    assert!(!e2.vcs().is_stale().unwrap(), "ws2 recovered");
    assert_eq!(e2.current_branch().unwrap().as_deref(), Some("feat-a"));
}

#[test]
fn worktree_remove_forgets_workspace() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").unwrap();

    let ws_parent = tempfile::tempdir().unwrap();
    let ws2 = ws_parent.path().join("ws2");
    e.worktree_add(&ws2, Some("ws2"), None).unwrap();
    assert!(e.worktree_list().unwrap().iter().any(|r| r.name == "ws2"));

    e.worktree_remove("ws2").unwrap();
    assert!(
        !e.worktree_list().unwrap().iter().any(|r| r.name == "ws2"),
        "ws2 no longer tracked"
    );
}
