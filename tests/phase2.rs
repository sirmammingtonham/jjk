//! Phase 2 acceptance: stack derivation, restack, track/untrack, branch delete (heal-the-gap).
//! Runs against a **real temporary jj repo**.

use jjk::engine::{Engine, NavDir};
use std::path::Path;
use std::sync::Once;
use tempfile::TempDir;

static IDENTITY: Once = Once::new();

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

fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

fn write(root: &Path, name: &str, contents: &str) {
    std::fs::write(root.join(name), contents).unwrap();
}

/// Build a three-branch stack: main ← feat-a ← feat-b ← feat-c.
fn three_branch_stack(e: &mut Engine, root: &Path) {
    e.branch_create("feat-a", true).unwrap();
    write(root, "a.txt", "a\n");
    e.commit("a1").unwrap();
    e.branch_create("feat-b", true).unwrap();
    write(root, "b.txt", "b\n");
    e.commit("b1").unwrap();
    e.branch_create("feat-c", true).unwrap();
    write(root, "c.txt", "c\n");
    e.commit("c1").unwrap();
}

#[test]
fn delete_middle_branch_reconnects_upstack() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    three_branch_stack(&mut e, &root);

    let before = e.derive_stack().unwrap();
    let feat_a_tip = before.branch("feat-a").unwrap().tip.clone();

    // Delete the middle branch.
    e.branch_delete("feat-b").unwrap();

    let after = e.derive_stack().unwrap();
    let names: Vec<_> = after.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-a", "feat-c"], "feat-b removed; stack heals");

    // feat-c's bottom commit must now be parented on feat-a's tip (the deleted branch's parent).
    let feat_c = after.branch("feat-c").unwrap();
    assert!(
        feat_c.commits[0].parents.contains(&feat_a_tip),
        "feat-c should reconnect to feat-a (parents {:?}, feat-a tip {:?})",
        feat_c.commits[0].parents,
        feat_a_tip
    );
    // The bookmark is really gone.
    assert!(
        !e.vcs().bookmarks().unwrap().iter().any(|b| b.name == "feat-b"),
        "feat-b bookmark deleted"
    );
    // And dropped from persisted state.
    assert!(e.state().pr_of("feat-b").is_none());
}

#[test]
fn delete_bottom_branch_reconnects_to_trunk() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    three_branch_stack(&mut e, &root);

    e.branch_delete("feat-a").unwrap();

    let after = e.derive_stack().unwrap();
    let names: Vec<_> = after.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-b", "feat-c"]);
    // feat-b now sits directly on trunk.
    let feat_b = after.branch("feat-b").unwrap();
    assert!(
        feat_b.commits[0].parents.contains(&after.trunk),
        "feat-b should reconnect to trunk"
    );
}

#[test]
fn restack_is_noop_on_healthy_stack() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    three_branch_stack(&mut e, &root);

    let report = e.restack().unwrap();
    assert!(
        report.notes.iter().any(|n| n.contains("up to date")),
        "restack on a healthy stack is a no-op, got: {:?}",
        report.notes
    );
    assert!(report.conflicts.is_empty());

    // Stack shape unchanged.
    let stack = e.derive_stack().unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-a", "feat-b", "feat-c"]);
}

#[test]
fn track_and_untrack_toggle_state() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();

    // `checkout -b` creates an untracked branch.
    e.branch_create("feat-x", /*tracked=*/ false).unwrap();
    write(&root, "x.txt", "x\n");
    e.commit("x1").unwrap();
    assert!(!e.state().is_tracked("feat-x"), "checkout -b is untracked");

    e.set_tracked(Some("feat-x"), true).unwrap();
    assert!(e.state().is_tracked("feat-x"), "now tracked");

    // Default to current branch when no name is given.
    e.set_tracked(None, false).unwrap();
    assert!(!e.state().is_tracked("feat-x"), "untracked again (current)");
}

#[test]
fn midstack_commit_rides_into_upstack() {
    // Phase 2 gate restatement: commit into a middle branch; upstack rides the new commit.
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    three_branch_stack(&mut e, &root);

    e.navigate(NavDir::Bottom).unwrap(); // feat-a
    assert_eq!(e.current_branch().unwrap().as_deref(), Some("feat-a"));
    write(&root, "a.txt", "a\nmore\n");
    e.commit("a2").unwrap();

    let stack = e.derive_stack().unwrap();
    let feat_a = stack.branch("feat-a").unwrap();
    let feat_b = stack.branch("feat-b").unwrap();
    assert_eq!(feat_a.commit_count(), 2);
    assert!(
        feat_b.commits[0].parents.contains(&feat_a.tip),
        "feat-b rides feat-a's new tip"
    );
    // feat-c remains on top, still reachable.
    assert!(stack.branch("feat-c").is_some());
}
