//! `sync` reconciles branches that are *done* but weren't cleanly merged — PRs closed without
//! merging, or changes that already landed in trunk via a separate PR — by offering to drop them
//! (confirmed, default keep). Plus `jjk stack drop` for removing the whole local stack on demand.
//! Real jj repo + local bare git remote; landings simulated by pushing from a throwaway clone.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::Engine;
use jjk::prompt::{PrDraft, Prompter};
use jjk::vcs::PushOpts;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {:?} failed in {:?}", args, dir);
}

/// A `Prompter` whose `confirm` always returns a fixed answer (new_pr just accepts the defaults).
struct FixedConfirm(bool);
impl Prompter for FixedConfirm {
    fn new_pr(&self, _b: &str, _base: &str, d: PrDraft) -> jjk::error::Result<Option<PrDraft>> {
        Ok(Some(d))
    }
    fn confirm(&self, _prompt: &str, _default: bool) -> jjk::error::Result<bool> {
        Ok(self.0)
    }
}

async fn seed_main(e: &mut Engine, root: &Path) {
    write(root, "README.md", "# smoke\n");
    e.vcs()
        .transaction(&mut |tx| {
            let id = tx.finalize_working_copy("chore: seed trunk")?;
            tx.create_bookmark("main", &id)?;
            Ok(())
        })
        .unwrap();
    e.vcs().push("origin", "main", PushOpts::default()).await.unwrap();
}

fn clone_remote(h: &RepoWithRemote) -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    let bare = h.remote.path().join("origin.git");
    git(work.path(), &["clone", bare.to_str().unwrap(), "."]);
    git(work.path(), &["config", "user.email", "land@example.com"]);
    git(work.path(), &["config", "user.name", "lander"]);
    work
}

/// feat-a (a.txt) → feat-b (b.txt), both tracked, submitted via the fake forge.
async fn stack_and_submit(h: &mut RepoWithRemote, fake: &Arc<FakeForge>) {
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root).await;
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "alpha\n");
    h.engine.commit("feat-a: alpha").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "bravo\n");
    h.engine.commit("feat-b: bravo").await.unwrap();
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
}

/// Land the WHOLE stack's content as one squash commit on origin/main (as if a separate PR took
/// the top of the stack straight into trunk and merged).
fn land_whole_stack_squashed(h: &RepoWithRemote) {
    let work = clone_remote(h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "a.txt", "alpha\n");
    write(work.path(), "b.txt", "bravo\n");
    git(work.path(), &["add", "a.txt", "b.txt"]);
    git(work.path(), &["commit", "-m", "ship it all (squash)"]);
    git(work.path(), &["push", "origin", "main"]);
}

#[tokio::test]
async fn sync_drops_closed_pr_branches_after_confirm() {
    let mut h = setup_with_remote().await;
    let fake = Arc::new(FakeForge::new());
    stack_and_submit(&mut h, &fake).await;

    // The stack shipped via a separate squash PR; the original stack PRs were CLOSED, not merged.
    land_whole_stack_squashed(&h);
    fake.set_closed("feat-a");
    fake.set_closed("feat-b");

    h.engine.set_prompter(Box::new(FixedConfirm(true)));
    let report = h.engine.sync(true).await.unwrap();
    assert!(report.conflicts.is_empty(), "no conflicts: {:?}", report.conflicts);
    assert!(
        report.notes.iter().any(|n| n.contains("dropped")),
        "reports the drop: {:?}",
        report.notes
    );

    // Both layers are gone; the stack is empty and the bookmarks are deleted.
    let stack = h.engine.derive_stack().await.unwrap();
    assert!(stack.branches.is_empty(), "stack emptied: {:?}", stack.branches);
    let bms = h.engine.vcs().bookmarks().await.unwrap();
    assert!(!bms.iter().any(|b| b.name == "feat-a" || b.name == "feat-b"));
}

#[tokio::test]
async fn sync_detects_branches_already_landed_in_trunk() {
    let mut h = setup_with_remote().await;
    let fake = Arc::new(FakeForge::new());
    stack_and_submit(&mut h, &fake).await;

    // Same landing, but the PRs are still OPEN — divergence is detected purely from the trees
    // (the branches become empty once rebased onto the trunk that already contains them).
    land_whole_stack_squashed(&h);

    h.engine.set_prompter(Box::new(FixedConfirm(true)));
    let report = h.engine.sync(true).await.unwrap();
    assert!(report.conflicts.is_empty(), "no conflicts: {:?}", report.conflicts);

    let stack = h.engine.derive_stack().await.unwrap();
    assert!(stack.branches.is_empty(), "diverged branches dropped: {:?}", stack.branches);
}

#[tokio::test]
async fn sync_keeps_closed_branches_when_declined() {
    let mut h = setup_with_remote().await;
    let fake = Arc::new(FakeForge::new());
    stack_and_submit(&mut h, &fake).await;

    land_whole_stack_squashed(&h);
    fake.set_closed("feat-a");
    fake.set_closed("feat-b");

    // Decline the drop — nothing is removed.
    h.engine.set_prompter(Box::new(FixedConfirm(false)));
    let report = h.engine.sync(true).await.unwrap();
    assert!(
        report.notes.iter().any(|n| n.contains("kept the local branches")),
        "reports keeping: {:?}",
        report.notes
    );

    let stack = h.engine.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-a", "feat-b"], "branches kept");
}

#[tokio::test]
async fn stack_drop_removes_the_whole_local_stack() {
    let mut h = setup_with_remote().await;
    let fake = Arc::new(FakeForge::new());
    stack_and_submit(&mut h, &fake).await;

    // Explicit, no prompt: drop the entire local stack.
    let report = h.engine.stack_drop(/*assume_yes=*/ true).await.unwrap();
    assert!(
        report.notes.iter().any(|n| n.contains("dropped 2")),
        "reports the count: {:?}",
        report.notes
    );

    let stack = h.engine.derive_stack().await.unwrap();
    assert!(stack.branches.is_empty(), "stack emptied: {:?}", stack.branches);
    let bms = h.engine.vcs().bookmarks().await.unwrap();
    assert!(!bms.iter().any(|b| b.name == "feat-a" || b.name == "feat-b"));
    // Trunk itself is untouched.
    assert!(bms.iter().any(|b| b.name == "main"));
}
