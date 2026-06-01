//! Following a plain `git checkout`: jjk's position must track git HEAD so you can drive with git
//! and let jj handle the stack. jjk's fast reads use --ignore-working-copy (skipping jj's HEAD
//! import), so the engine reconciles explicitly.

mod common;

use common::{init_identity, write};
use jjk::engine::Engine;
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

fn git_checkout(root: &Path, branch: &str) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["checkout", branch])
        .status()
        .unwrap()
        .success();
    assert!(ok, "git checkout {branch} failed");
}

fn git_head_branch(root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn two_branch_stack(e: &mut Engine, root: &Path) {
    e.branch_create("feat-a", true).await.unwrap();
    write(root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();
    e.branch_create("feat-b", true).await.unwrap();
    write(root, "b.txt", "b\n");
    e.commit("b1").await.unwrap();
}

#[tokio::test]
async fn jjk_follows_git_checkout() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    two_branch_stack(&mut e, &root).await;
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-b"));

    git_checkout(&root, "feat-a");
    // The fast read still lags (jj hasn't imported the new HEAD)...
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-b"), "stale before reconcile");

    // ...until we reconcile, after which position follows git.
    assert!(e.reconcile_git_head().await.unwrap(), "HEAD moved, so it reconciles");
    assert_eq!(e.current_branch().await.unwrap().as_deref(), Some("feat-a"));

    // Idempotent: a second reconcile is a no-op (no spurious snapshot/note).
    assert!(!e.reconcile_git_head().await.unwrap(), "already in sync");
}

#[tokio::test]
async fn commit_after_git_checkout_lands_on_that_branch_and_restacks() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    two_branch_stack(&mut e, &root).await;

    git_checkout(&root, "feat-a");
    e.reconcile_git_head().await.unwrap();

    write(&root, "a.txt", "a\nmore\n");
    e.commit("a2").await.unwrap();

    let stack = e.derive_stack().await.unwrap();
    assert_eq!(stack.branch("feat-a").unwrap().commit_count(), 2, "a2 landed on feat-a");
    // feat-b was restacked onto the new feat-a tip (jj handles the rebase).
    assert_eq!(stack.branch("feat-b").unwrap().commit_count(), 1);
}

#[tokio::test]
async fn jjk_checkout_attaches_git_head_to_the_branch() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    two_branch_stack(&mut e, &root).await; // ends on feat-b

    e.checkout("feat-a").await.unwrap();
    e.sync_git_head_to_current().await.unwrap();
    assert_eq!(git_head_branch(&root), "feat-a", "git follows jjk to feat-a");

    e.checkout("feat-b").await.unwrap();
    e.sync_git_head_to_current().await.unwrap();
    assert_eq!(git_head_branch(&root), "feat-b", "and back to feat-b");
}

#[tokio::test]
async fn fresh_branch_does_not_attach_head_to_its_empty_working_copy() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    e.commit("a1").await.unwrap();

    // Brand-new branch with no commit yet: its bookmark rides the empty `@` (tip == @, not @-).
    // Attaching HEAD there would break jj's `HEAD == @-` invariant, so sync must leave it alone.
    e.branch_create("feat-b", true).await.unwrap();
    e.sync_git_head_to_current().await.unwrap();
    assert_ne!(git_head_branch(&root), "feat-b", "must not attach HEAD to an empty new branch");
}

#[tokio::test]
async fn no_reconcile_needed_in_pure_jjk_workflow() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    two_branch_stack(&mut e, &root).await;

    // jjk's own navigation keeps git HEAD in sync, so reconcile is a no-op (no spurious note).
    e.checkout("feat-a").await.unwrap();
    assert!(!e.reconcile_git_head().await.unwrap(), "jjk checkout already synced HEAD");
}
