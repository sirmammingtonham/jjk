//! Switching branches must not lose uncommitted changes. Like `git checkout` with a dirty tree,
//! jjk carries the working-copy changes onto the target branch (instead of stranding them as a
//! nameless, off-disk commit on the old branch). A carry that conflicts with the target keeps the
//! changes with jj conflict markers and is reported so the resolve flow kicks in.

mod common;

use common::{init_identity, write};
use jjk::engine::{Engine, NavDir};
use tempfile::TempDir;

async fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).await.unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

#[tokio::test]
async fn checkout_carries_uncommitted_changes_to_the_target_branch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("A", true).await.unwrap();
    write(&root, "shared.txt", "a\n");
    e.commit("A work").await.unwrap();
    e.branch_create("B", true).await.unwrap();
    write(&root, "other.txt", "b\n");
    e.commit("B work").await.unwrap();

    // On A, make an uncommitted change, then switch to B.
    e.checkout("A").await.unwrap();
    write(&root, "newfile.txt", "carry me\n");
    let report = e.checkout("B").await.unwrap();
    assert!(report.conflicts.is_empty(), "clean carry has no conflicts: {:?}", report.conflicts);

    // The change rode along: still on disk, still uncommitted, now landing on B.
    assert!(root.join("newfile.txt").exists(), "uncommitted change survived the switch");
    assert!(!e.vcs().snapshot().await.unwrap().is_empty, "working copy still dirty after switch");
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("B"), "now on B");

    // A must NOT have gained a stray commit (no dangling parked change).
    let stack = e.derive_stack().await.unwrap();
    assert_eq!(stack.branch("A").unwrap().commit_count(), 1, "A unchanged (no parked commit)");

    // Committing now lands the carried change on B.
    e.commit("land on B").await.unwrap();
    let diff = e.vcs().diff("@-").await.unwrap();
    assert!(diff.contains("newfile.txt"), "carried change committed onto B: {diff}");
}

#[tokio::test]
async fn navigating_down_the_stack_carries_changes() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("base", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("base").await.unwrap();
    e.branch_create("feature", true).await.unwrap();
    write(&root, "f.txt", "f\n");
    e.commit("feature").await.unwrap();

    // On feature, write a change that really belongs on base, then move down.
    write(&root, "fix.txt", "belongs on base\n");
    e.navigate(NavDir::Down).await.unwrap();

    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("base"), "moved down to base");
    assert!(root.join("fix.txt").exists(), "change carried down to base");
    e.commit("fix on base").await.unwrap();
    let diff = e.vcs().diff("@-").await.unwrap();
    assert!(diff.contains("fix.txt"), "carried change committed on base: {diff}");
}

#[tokio::test]
async fn clean_switch_starts_empty() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("A", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("A").await.unwrap();
    e.branch_create("B", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    e.commit("B").await.unwrap();

    // No uncommitted changes → switching leaves a clean (empty) working copy.
    let report = e.checkout("A").await.unwrap();
    assert!(report.conflicts.is_empty());
    assert!(e.vcs().snapshot().await.unwrap().is_empty, "clean switch starts with an empty @");
}

#[tokio::test]
async fn conflicting_carry_keeps_changes_and_reports_conflicts() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    // A and B both change c.txt differently (off trunk), so a carried edit to c.txt can't apply
    // cleanly to the other branch.
    e.branch_create("A", true).await.unwrap();
    write(&root, "c.txt", "line\nA-version\n");
    e.commit("A").await.unwrap();
    e.trunk_checkout().await.unwrap();
    e.branch_create("B", true).await.unwrap();
    write(&root, "c.txt", "line\nB-version\n");
    e.commit("B").await.unwrap();

    // On A, edit c.txt (uncommitted), then switch to B — the carry conflicts.
    e.checkout("A").await.unwrap();
    write(&root, "c.txt", "line\nA-version\nMY EDIT\n");
    let report = e.checkout("B").await.unwrap();

    assert!(!report.conflicts.is_empty(), "conflicting carry is reported");
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("B"), "still completed the switch");
    assert!(e.vcs().working_copy().await.unwrap().has_conflict, "working copy is conflicted");
    // The change was not lost: c.txt on disk now carries jj conflict markers.
    let on_disk = std::fs::read_to_string(root.join("c.txt")).unwrap();
    assert!(on_disk.contains("MY EDIT"), "the user's edit is preserved in the conflict: {on_disk}");
}

#[tokio::test]
async fn changes_on_a_freshly_created_branch_are_not_lost_on_switch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("A", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("A").await.unwrap();

    // New branch X rides the empty @ (no commit yet); make changes, then switch away.
    e.branch_create("X", true).await.unwrap();
    write(&root, "xwork.txt", "work on X\n");
    e.checkout("A").await.unwrap();

    // The work stayed on X (visible, recoverable) rather than vanishing.
    e.checkout("X").await.unwrap();
    assert!(root.join("xwork.txt").exists(), "work on the new branch survived the round-trip");
}
