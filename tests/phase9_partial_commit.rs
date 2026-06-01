//! Partial commits: `jjk commit` honors git-staged files (and explicit path scopes), committing
//! only those and leaving the rest uncommitted — bridging editor-driven git staging into jj.

mod common;

use common::{init_identity, write};
use jjk::engine::Engine;
use jjk::vcs::CommitScope;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

async fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).await.unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

fn git_add(root: &Path, file: &str) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["add", file])
        .status()
        .unwrap()
        .success();
    assert!(ok, "git add {file} failed");
}

#[tokio::test]
async fn staged_files_survive_jjk_snapshots_and_scope_the_commit() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    write(&root, "b.txt", "b\n");

    // Stage only a.txt via git (as an editor would).
    git_add(&root, "a.txt");

    // The dispatcher records an undo checkpoint (which snapshots the working copy) before commit —
    // that must not wipe the git index.
    e.checkpoint().await.unwrap();
    let staged = e.staged_paths().await.unwrap();
    assert_eq!(staged, vec!["a.txt".to_string()], "staging survives jjk's snapshot");

    e.commit_scoped("only a", &CommitScope::Paths(staged)).await.unwrap();

    // Exactly one commit, and it contains a.txt but not b.txt.
    assert_eq!(e.derive_stack().await.unwrap().branch("feat").unwrap().commit_count(), 1);
    let diff = e.vcs().diff("@-").await.unwrap();
    assert!(diff.contains("a.txt"), "committed a.txt: {diff}");
    assert!(!diff.contains("b.txt"), "did NOT commit b.txt: {diff}");

    // b.txt is still an uncommitted working-copy change.
    assert!(!e.vcs().snapshot().await.unwrap().is_empty, "b.txt remains uncommitted");
}

#[tokio::test]
async fn nothing_staged_commits_everything() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    write(&root, "b.txt", "b\n");
    assert!(e.staged_paths().await.unwrap().is_empty(), "nothing staged");

    e.commit("everything").await.unwrap();
    let diff = e.vcs().diff("@-").await.unwrap();
    assert!(diff.contains("a.txt") && diff.contains("b.txt"), "both files committed: {diff}");
    assert!(e.vcs().snapshot().await.unwrap().is_empty, "working copy clean after committing all");
}

#[tokio::test]
async fn explicit_path_scope_leaves_the_rest_behind() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    e.branch_create("feat", true).await.unwrap();
    write(&root, "keep.txt", "k\n");
    write(&root, "later.txt", "l\n");

    e.commit_scoped("just keep", &CommitScope::Paths(vec!["keep.txt".into()])).await.unwrap();

    let diff = e.vcs().diff("@-").await.unwrap();
    assert!(diff.contains("keep.txt") && !diff.contains("later.txt"), "scoped to keep.txt: {diff}");
    assert!(!e.vcs().snapshot().await.unwrap().is_empty, "later.txt stays uncommitted");
}
