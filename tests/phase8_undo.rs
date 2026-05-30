//! `jjk undo` reverts a whole jjk command as one unit (not just the last jj operation).
//!
//! Regression: a single `jjk commit` is several jj operations (commit + bookmark set + restacks).
//! Plain `jj undo` reverted only the bookmark move, leaving the commit behind — so re-committing
//! produced a duplicate commit on the branch. `jjk undo` now `jj op restore`s past the whole
//! command, and the dispatcher records a checkpoint before each mutating command.

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
fn undo_after_commit_does_not_leave_a_duplicate() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();

    // A tracked branch with one commit (the user's starting point).
    e.branch_create("feat", true).unwrap();
    write(&root, "a.txt", "first\n");
    e.commit("add first thing").unwrap();
    assert_eq!(e.derive_stack().unwrap().branch("feat").unwrap().commit_count(), 1);

    // Make a change and commit — but this is the command we'll undo (mimics dispatcher: checkpoint
    // first, then run the command).
    write(&root, "a.txt", "first\nsecond\n");
    e.checkpoint().unwrap();
    e.commit("better error handling").unwrap();
    assert_eq!(e.derive_stack().unwrap().branch("feat").unwrap().commit_count(), 2);

    // Undo should fully revert the commit, not just move the bookmark.
    e.undo().unwrap();
    let stack = e.derive_stack().unwrap();
    assert_eq!(
        stack.branch("feat").unwrap().commit_count(),
        1,
        "undo must drop the just-made commit entirely"
    );

    // Re-commit (the user's redo). Different content avoids jj's sub-second identical-hash quirk;
    // the regression we guard is structural: there must be exactly ONE new commit on top of the
    // base, not the original commit lingering alongside the redo.
    write(&root, "a.txt", "first\nsecond redone\n");
    e.checkpoint().unwrap();
    e.commit("better error handling").unwrap();
    let stack = e.derive_stack().unwrap();
    assert_eq!(
        stack.branch("feat").unwrap().commit_count(),
        2,
        "re-committing after undo must not produce a duplicate commit"
    );
}

#[test]
fn undo_restores_uncommitted_changes_to_working_copy() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();

    e.branch_create("feat", true).unwrap();
    write(&root, "a.txt", "v1\n");
    e.commit("c1").unwrap();

    // Edit, then commit-then-undo: the edit should come back as an uncommitted change in `@`.
    write(&root, "a.txt", "v1\nv2\n");
    e.checkpoint().unwrap();
    e.commit("c2").unwrap();
    e.undo().unwrap();

    assert!(!e.vcs().snapshot().unwrap().is_empty, "edits return to the working copy after undo");
    assert_eq!(e.derive_stack().unwrap().branch("feat").unwrap().commit_count(), 1);
}

#[test]
fn undo_walks_back_multiple_commands() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();

    e.branch_create("feat", true).unwrap();
    // Each command mirrors the dispatcher: edit, checkpoint (snapshots the edit), then commit.
    write(&root, "a.txt", "base\n");
    e.checkpoint().unwrap();
    e.commit("base").unwrap();
    write(&root, "a.txt", "base\ntwo\n");
    e.checkpoint().unwrap();
    e.commit("two").unwrap();
    write(&root, "a.txt", "base\ntwo\nthree\n");
    e.checkpoint().unwrap();
    e.commit("three").unwrap();
    assert_eq!(e.derive_stack().unwrap().branch("feat").unwrap().commit_count(), 3);

    e.undo().unwrap(); // undo "three"
    assert_eq!(e.derive_stack().unwrap().branch("feat").unwrap().commit_count(), 2);
    e.undo().unwrap(); // undo "two"
    assert_eq!(e.derive_stack().unwrap().branch("feat").unwrap().commit_count(), 1);
}

#[test]
fn undo_with_no_checkpoint_falls_back_to_jj_undo() {
    let (tmp, mut e) = setup();
    let root = tmp.path().to_path_buf();
    e.branch_create("feat", true).unwrap();
    write(&root, "a.txt", "1\n");
    e.commit("one").unwrap();

    // No checkpoint recorded for this commit; undo falls back to a single `jj undo` and still runs.
    assert!(e.undo().is_ok(), "undo without a checkpoint falls back cleanly");
}
