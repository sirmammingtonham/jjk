//! Branch restructuring ops: trunk, branch onto/rename/diff/squash/fold, commit --fixup.

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

async fn commit_branch(e: &mut Engine, root: &std::path::Path, name: &str, file: &str, body: &str) {
    e.branch_create(name, true).await.unwrap();
    write(root, file, body);
    e.commit(&format!("{name}: {file}")).await.unwrap();
}

#[tokio::test]
async fn trunk_switches_to_trunk() {
    let (tmp, mut e) = setup().await;
    commit_branch(&mut e, tmp.path(), "feat-a", "a.txt", "a").await;
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));
    e.trunk_checkout().await.unwrap();
    assert_eq!(e.current_branch().await.unwrap(), None, "now on trunk");
}

#[tokio::test]
async fn branch_onto_moves_branch_to_new_base() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    commit_branch(&mut e, &root, "feat-a", "a.txt", "a").await;
    // A separate branch off trunk.
    e.trunk_checkout().await.unwrap();
    commit_branch(&mut e, &root, "feat-base", "base.txt", "base").await;
    // feat-a currently sits on trunk; move it onto feat-base.
    e.checkout("feat-a").await.unwrap();
    e.branch_onto("feat-base").await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-base", "feat-a"], "feat-a now stacked on feat-base");
    let feat_base_tip = stack.branch("feat-base").unwrap().tip.clone();
    assert!(stack.branch("feat-a").unwrap().commits[0].parents.contains(&feat_base_tip));
}

#[tokio::test]
async fn branch_rename_keeps_tracking() {
    let (tmp, mut e) = setup().await;
    commit_branch(&mut e, tmp.path(), "feat-a", "a.txt", "a").await;
    e.branch_rename(None, "feat-z").await.unwrap();

    assert!(e.vcs().bookmarks().await.unwrap().iter().any(|b| b.name == "feat-z"));
    assert!(!e.vcs().bookmarks().await.unwrap().iter().any(|b| b.name == "feat-a"));
    assert!(e.state().is_tracked("feat-z"));
    assert!(!e.state().is_tracked("feat-a"));
}

#[tokio::test]
async fn branch_diff_shows_branch_changes() {
    let (tmp, mut e) = setup().await;
    commit_branch(&mut e, tmp.path(), "feat-a", "feature.txt", "hello feature").await;
    let diff = e.branch_diff().await.unwrap();
    assert!(diff.contains("feature.txt"), "diff names the file: {diff}");
    assert!(diff.contains("hello feature"), "diff shows content: {diff}");
}

#[tokio::test]
async fn branch_squash_collapses_to_one_commit() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "1\n");
    e.commit("a1").await.unwrap();
    write(&root, "a.txt", "1\n2\n");
    e.commit("a2").await.unwrap();
    write(&root, "a.txt", "1\n2\n3\n");
    e.commit("a3").await.unwrap();
    assert_eq!(e.derive_stack().await.unwrap().branch("feat-a").unwrap().commit_count(), 3);

    e.branch_squash(Some("squashed feat-a")).await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let feat_a = stack.branch("feat-a").unwrap();
    assert_eq!(feat_a.commit_count(), 1, "collapsed to one commit");
    assert_eq!(feat_a.commits[0].subject(), "squashed feat-a");
    // Content preserved.
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "1\n2\n3\n");
}

#[tokio::test]
async fn branch_fold_merges_into_base() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    commit_branch(&mut e, &root, "feat-a", "a.txt", "a").await;
    commit_branch(&mut e, &root, "feat-b", "b.txt", "b").await;
    // On feat-b; fold it into feat-a.
    e.branch_fold().await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-a"], "feat-b folded away");
    assert_eq!(stack.branch("feat-a").unwrap().commit_count(), 2, "feat-a absorbed feat-b's commit");
    assert!(!e.state().is_tracked("feat-b"));
}

#[tokio::test]
async fn commit_fixup_folds_into_downstack_commit() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    commit_branch(&mut e, &root, "feat-a", "a.txt", "a\n").await;
    commit_branch(&mut e, &root, "feat-b", "b.txt", "b\n").await;
    let feat_a_tip_before = e.derive_stack().await.unwrap().branch("feat-a").unwrap().tip.clone();

    // On feat-b, make a change destined for feat-a, then fix it up into feat-a.
    write(&root, "a.txt", "a\nfixed\n");
    e.commit_fixup("feat-a").await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    // feat-a's change id is stable but it absorbed the edit; feat-b still one commit, rebased.
    let feat_a = stack.branch("feat-a").unwrap();
    assert_eq!(feat_a.tip, feat_a_tip_before, "fixup keeps the change id");
    assert_eq!(stack.branch("feat-b").unwrap().commit_count(), 1);
    // The fix landed in feat-a (its file now has the edit; working copy is clean again).
    assert!(e.vcs().snapshot().await.unwrap().is_empty, "working copy clean after fixup");
}

#[tokio::test]
async fn commit_pick_copies_a_commit_onto_current_branch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    commit_branch(&mut e, &root, "feat-a", "a.txt", "a\n").await;
    commit_branch(&mut e, &root, "feat-b", "b.txt", "b\n").await;
    let b_commit = e.derive_stack().await.unwrap().branch("feat-b").unwrap().tip.clone();

    e.checkout("feat-a").await.unwrap();
    e.commit_pick(b_commit.as_str()).await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    assert_eq!(stack.branch("feat-a").unwrap().commit_count(), 2, "feat-a gained the picked commit");
    // The picked content is now present on feat-a's working copy.
    assert!(root.join("b.txt").exists(), "picked commit's file is present");
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));
}

#[tokio::test]
async fn branch_split_divides_a_branch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "1\n");
    e.commit("a1").await.unwrap();
    write(&root, "a.txt", "1\n2\n");
    e.commit("a2").await.unwrap();
    write(&root, "a.txt", "1\n2\n3\n");
    e.commit("a3").await.unwrap();

    // Split at the second commit (a2): feat-lower = a1,a2 ; feat-a = a3.
    let a2 = e.derive_stack().await.unwrap().branch("feat-a").unwrap().commits[1].change_id.clone();
    e.branch_split("feat-lower", a2.as_str()).await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-lower", "feat-a"]);
    assert_eq!(stack.branch("feat-lower").unwrap().commit_count(), 2);
    assert_eq!(stack.branch("feat-a").unwrap().commit_count(), 1);
    assert!(e.state().is_tracked("feat-lower"));
}
