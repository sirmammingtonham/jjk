//! `jjk sync` rebases the stack onto an advanced trunk even when no PR of ours merged — trunk just
//! moved (someone else landed work). `sync -n` does this locally and skips pushing. Regression: the
//! local trunk bookmark didn't auto-advance on fetch (when not tracking the remote), so the stack
//! never rebased; sync now fast-forwards the local trunk to the fetched remote position first.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::vcs::PushOpts;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git").current_dir(dir).args(args).status().unwrap().success(),
        "git {args:?} failed"
    );
}

fn jj(root: &Path, args: &[&str]) {
    let _ = Command::new("jj").arg("-R").arg(root).args(args).status();
}

fn remote_sha(bare: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["rev-parse", &format!("refs/heads/{branch}")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Seed + push trunk, then build and submit a two-branch stack. Returns after submit.
async fn seeded_stack(h: &mut RepoWithRemote, fake: &Arc<FakeForge>) {
    let root = h.repo.path().to_path_buf();
    write(&root, "README.md", "# seed\n");
    h.engine
        .vcs()
        .transaction(&mut |tx| {
            let id = tx.finalize_working_copy("chore: seed trunk")?;
            tx.create_bookmark("main", &id)?;
            Ok(())
        })
        .unwrap();
    h.engine.vcs().push("origin", "main", PushOpts::default()).unwrap();

    // Simulate a repo whose local trunk bookmark does NOT track the remote (so plain fetch won't
    // advance it) — the condition under which the stack failed to rebase.
    jj(&root, &["bookmark", "untrack", "main@origin"]);

    h.engine.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").unwrap();
    h.engine.branch_create("feat-b", true).unwrap();
    write(&root, "b.txt", "b\n");
    h.engine.commit("feat-b: first").unwrap();

    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
}

/// Land an UNRELATED commit on main from a throwaway clone (no PR of ours merged).
fn advance_trunk_externally(h: &RepoWithRemote) {
    let work = tempfile::tempdir().unwrap();
    let bare = h.remote.path().join("origin.git");
    git(work.path(), &["clone", bare.to_str().unwrap(), "."]);
    git(work.path(), &["config", "user.email", "x@e.com"]);
    git(work.path(), &["config", "user.name", "x"]);
    git(work.path(), &["checkout", "main"]);
    std::fs::write(work.path().join("unrelated.txt"), "z\n").unwrap();
    git(work.path(), &["add", "unrelated.txt"]);
    git(work.path(), &["commit", "-m", "unrelated trunk work"]);
    git(work.path(), &["push", "origin", "main"]);
}

#[tokio::test]
async fn sync_no_push_rebases_stack_onto_advanced_trunk() {
    let mut h = setup_with_remote();
    let fake = Arc::new(FakeForge::new());
    seeded_stack(&mut h, &fake).await;

    let trunk_before = h.engine.derive_stack().unwrap().trunk;
    let bare = h.remote.path().join("origin.git");
    let feat_a_remote_before = remote_sha(&bare, "feat-a");

    advance_trunk_externally(&h);
    h.engine.sync(false).await.unwrap();

    let after = h.engine.derive_stack().unwrap();
    assert_ne!(after.trunk, trunk_before, "trunk should advance to the fetched remote position");
    assert!(
        after.branch("feat-a").unwrap().commits[0].parents.contains(&after.trunk),
        "feat-a should be rebased onto the new trunk"
    );
    assert_eq!(after.branch("feat-b").unwrap().commit_count(), 1, "feat-b still rides above feat-a");

    // -n means local-only: nothing pushed.
    assert_eq!(remote_sha(&bare, "feat-a"), feat_a_remote_before, "sync -n must not push feat-a");
}

#[tokio::test]
async fn sync_does_not_repush_unchanged_branches() {
    let mut h = setup_with_remote();
    let fake = Arc::new(FakeForge::new());
    seeded_stack(&mut h, &fake).await;

    // Nothing changed since submit (trunk hasn't moved): sync must not re-push any branch.
    let report = h.engine.sync(true).await.unwrap();
    assert!(
        !report.notes.iter().any(|n| n.starts_with("pushed ")),
        "sync should not re-push unchanged branches; notes: {:?}",
        report.notes
    );
}

#[tokio::test]
async fn resubmit_does_not_repush_unchanged_branches() {
    let mut h = setup_with_remote();
    let fake = Arc::new(FakeForge::new());
    seeded_stack(&mut h, &fake).await; // already submitted once

    // Re-submit with nothing changed: no PRs created/updated (bases unchanged), no pushes needed.
    let report = h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();
    assert!(
        !report.notes.iter().any(|n| n.starts_with("created ") || n.starts_with("updated ")),
        "re-submit of an unchanged stack should be a no-op on the forge; notes: {:?}",
        report.notes
    );
    assert_eq!(fake.count(), 2, "no duplicate PRs");
}

#[tokio::test]
async fn sync_push_rebases_then_pushes_onto_advanced_trunk() {
    let mut h = setup_with_remote();
    let fake = Arc::new(FakeForge::new());
    seeded_stack(&mut h, &fake).await;

    let trunk_before = h.engine.derive_stack().unwrap().trunk;
    let bare = h.remote.path().join("origin.git");
    let feat_a_remote_before = remote_sha(&bare, "feat-a");

    advance_trunk_externally(&h);
    h.engine.sync(true).await.unwrap();

    let after = h.engine.derive_stack().unwrap();
    assert_ne!(after.trunk, trunk_before, "trunk advanced");
    assert!(after.branch("feat-a").unwrap().commits[0].parents.contains(&after.trunk));
    // With push, the rebased branch is force-pushed.
    assert_ne!(remote_sha(&bare, "feat-a"), feat_a_remote_before, "sync (push) updates feat-a");
}
