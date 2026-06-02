//! Phase 4 acceptance: `sync` under BOTH squash-merge and merge-commit landings, plus a
//! conflict-during-sync case. Real jj repo + local bare git remote; landings are simulated by
//! pushing to the remote from a throwaway git clone (as in the Phase 0 probe). Fake forge supplies
//! merged-state.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::Engine;
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

/// Seed a `main` trunk commit and push it to origin.
async fn seed_main(e: &mut Engine, root: &Path) {
    write(root, "README.md", "# smoke\n");
    e.vcs().snapshot().await.unwrap();
    let mut tx = e.vcs().begin_transaction().await.unwrap();
    let id = tx.finalize_working_copy("chore: seed trunk").await.unwrap();
    tx.create_bookmark("main", &id).await.unwrap();
    tx.commit().await.unwrap();
    e.vcs().push("origin", "main", PushOpts::default()).await.unwrap();
}

/// Clone the bare remote into a throwaway git workdir for simulating landings.
fn clone_remote(h: &RepoWithRemote) -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    let bare = h.remote.path().join("origin.git");
    git(
        work.path(),
        &["clone", bare.to_str().unwrap(), "."],
    );
    git(work.path(), &["config", "user.email", "land@example.com"]);
    git(work.path(), &["config", "user.name", "lander"]);
    work
}

async fn build_two_branch_stack(h: &mut RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root).await;
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "alpha\n");
    h.engine.commit("feat-a: alpha").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "bravo\n");
    h.engine.commit("feat-b: bravo").await.unwrap();
}

#[tokio::test]
async fn sync_after_squash_merge_of_bottom() {
    let mut h = setup_with_remote().await;
    build_two_branch_stack(&mut h).await;

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Simulate a SQUASH-merge of feat-a: a new commit on main carrying feat-a's content.
    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "a.txt", "alpha\n");
    git(work.path(), &["add", "a.txt"]);
    git(work.path(), &["commit", "-m", "feat-a (squash #1)"]);
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    let report = h.engine.sync(true).await.unwrap();
    assert!(report.conflicts.is_empty(), "no conflicts: {:?}", report.conflicts);

    let stack = h.engine.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-b"], "feat-a abandoned; feat-b survives");

    // feat-b sits directly on the new trunk and stays non-empty (clean diff: just b.txt).
    let feat_b = stack.branch("feat-b").unwrap();
    assert!(feat_b.commits[0].parents.contains(&stack.trunk));
    assert_eq!(feat_b.commit_count(), 1);
    assert!(!feat_b.commits[0].is_empty, "feat-b not garbled into empty");
    // feat-a bookmark really gone.
    assert!(!h.engine.vcs().bookmarks().await.unwrap().iter().any(|b| b.name == "feat-a"));
    // feat-b's PR retargeted to trunk.
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "main");
}

#[tokio::test]
async fn sync_after_merge_commit_of_bottom() {
    let mut h = setup_with_remote().await;
    build_two_branch_stack(&mut h).await;

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Simulate a MERGE-COMMIT landing of feat-a: real merge of origin/feat-a into main.
    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    git(
        work.path(),
        &["merge", "--no-ff", "origin/feat-a", "-m", "Merge pull request #1 (feat-a)"],
    );
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    let report = h.engine.sync(true).await.unwrap();
    assert!(report.conflicts.is_empty(), "no conflicts: {:?}", report.conflicts);

    let stack = h.engine.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-b"], "feat-a dropped (in trunk); feat-b survives");

    let feat_b = stack.branch("feat-b").unwrap();
    assert!(feat_b.commits[0].parents.contains(&stack.trunk));
    assert_eq!(feat_b.commit_count(), 1);
    assert!(!feat_b.commits[0].is_empty);
    assert!(!h.engine.vcs().bookmarks().await.unwrap().iter().any(|b| b.name == "feat-a"));
    assert_eq!(fake.pr_for("feat-b").unwrap().base, "main");

    // The trunk now actually contains feat-a's file (a.txt) — clean, non-duplicated history.
    assert!(h.repo.path().join("a.txt").exists());
    assert!(h.repo.path().join("b.txt").exists());
}

#[tokio::test]
async fn sync_with_no_merges_is_safe() {
    let mut h = setup_with_remote().await;
    build_two_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Nothing merged: sync should be a safe no-op on the stack shape.
    let report = h.engine.sync(true).await.unwrap();
    assert!(report.notes.iter().any(|n| n.contains("no merged")));
    let stack = h.engine.derive_stack().await.unwrap();
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feat-a", "feat-b"]);
}

#[tokio::test]
async fn sync_reports_conflict_without_aborting() {
    // feat-b modifies the same file feat-a created; squash-landing feat-a's content differently
    // makes feat-b conflict when rebased onto trunk. sync must complete and REPORT, not abort.
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root).await;

    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "shared.txt", "from-a\n");
    h.engine.commit("feat-a: shared").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "shared.txt", "from-b\n");
    h.engine.commit("feat-b: shared edit").await.unwrap();

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Squash-land feat-a with DIFFERENT content than feat-a had, so feat-b's edit conflicts.
    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "shared.txt", "from-trunk\n");
    git(work.path(), &["add", "shared.txt"]);
    git(work.path(), &["commit", "-m", "feat-a landed (modified)"]);
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    // Must not error — conflicts are reported, not fatal.
    let report = h.engine.sync(true).await.unwrap();
    assert!(
        !report.conflicts.is_empty(),
        "sync should surface the conflict, got notes {:?}",
        report.notes
    );
    // feat-b still present (conflicted), stack intact.
    assert!(h.engine.derive_stack().await.unwrap().branch("feat-b").is_some());
}

#[tokio::test]
async fn resolve_workflow_walks_conflicts_and_returns_home() {
    // Same conflict as `sync_reports_conflict_without_aborting`, then drive the git-style loop:
    // resolve (enter) → fix the file → resolve --continue (finish) lands back on the branch you
    // started on with the stack clean and the session cleared.
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root).await;

    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "shared.txt", "from-a\n");
    h.engine.commit("feat-a: shared").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "shared.txt", "from-b\n");
    h.engine.commit("feat-b: shared edit").await.unwrap();

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Squash-land feat-a with different content so feat-b conflicts on rebase.
    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "shared.txt", "from-trunk\n");
    git(work.path(), &["add", "shared.txt"]);
    git(work.path(), &["commit", "-m", "feat-a landed (modified)"]);
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    let report = h.engine.sync(true).await.unwrap();
    assert!(!report.conflicts.is_empty(), "sync should surface the conflict");

    // main.rs opens the session after a conflicting command; mirror that here, with no resume so
    // the finish step stays offline (the auto-resume path is exercised by `jjk sync` end-to-end).
    h.engine
        .begin_resolve_session_if_absent(None, vec![])
        .await
        .unwrap();
    assert!(h.engine.has_resolve_session());

    // Enter: jump to the conflicted change. Still conflicted until we fix and continue.
    h.engine.resolve(false).await.unwrap();
    assert!(h.engine.has_conflicts().await.unwrap());

    // Fix the conflict on disk, then continue: captures the fix, finds nothing left, returns home.
    write(&root, "shared.txt", "resolved\n");
    let report = h.engine.resolve_continue().await.unwrap();
    assert!(
        !h.engine.has_conflicts().await.unwrap(),
        "stack should be clean after continue: {:?}",
        report.notes
    );
    assert!(!h.engine.has_resolve_session(), "session cleared on finish");
    assert_eq!(
        h.engine.current_branch().await.unwrap().as_deref(),
        Some("feat-b"),
        "should land back on the branch we started on"
    );
}

#[tokio::test]
async fn resolve_abort_restores_pre_sync_state() {
    // After a conflicting sync, `resolve --abort` rewinds to the pre-sync checkpoint, the way
    // `git rebase --abort` would. We seed a checkpoint explicitly (main.rs records one per command).
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root).await;

    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "shared.txt", "from-a\n");
    h.engine.commit("feat-a: shared").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "shared.txt", "from-b\n");
    h.engine.commit("feat-b: shared edit").await.unwrap();

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Checkpoint before sync (as the binary would), so abort has a precise point to rewind to.
    h.engine.checkpoint().await.unwrap();

    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "shared.txt", "from-trunk\n");
    git(work.path(), &["add", "shared.txt"]);
    git(work.path(), &["commit", "-m", "feat-a landed (modified)"]);
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    h.engine.sync(true).await.unwrap();
    h.engine
        .begin_resolve_session_if_absent(None, vec![])
        .await
        .unwrap();
    assert!(h.engine.has_conflicts().await.unwrap());

    h.engine.resolve_abort().await.unwrap();
    assert!(!h.engine.has_resolve_session(), "session cleared on abort");
    assert!(
        !h.engine.has_conflicts().await.unwrap(),
        "abort should rewind past the conflicting rebase"
    );
}

#[tokio::test]
async fn repo_init_sets_git_conflict_markers() {
    // `repo init` should pick git-style conflict markers so they read familiarly.
    let h = setup_with_remote().await;
    assert_eq!(
        h.engine
            .vcs()
            .config_get("ui.conflict-marker-style")
            .await
            .unwrap()
            .as_deref(),
        Some("git")
    );
}

#[tokio::test]
async fn resolve_interactive_uses_merge_tool() {
    // `jjk resolve --interactive` drives jj's configured merge tool. We point ui.merge-editor at
    // the built-in `:ours` so it resolves non-interactively in the test, then assert it cleared the
    // conflict and returned us home.
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    h.engine
        .vcs()
        .set_config_repo("ui.merge-editor", ":ours")
        .await
        .unwrap();
    seed_main(&mut h.engine, &root).await;

    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "shared.txt", "from-a\n");
    h.engine.commit("feat-a: shared").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "shared.txt", "from-b\n");
    h.engine.commit("feat-b: shared edit").await.unwrap();

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "shared.txt", "from-trunk\n");
    git(work.path(), &["add", "shared.txt"]);
    git(work.path(), &["commit", "-m", "feat-a landed (modified)"]);
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    h.engine.sync(true).await.unwrap();
    h.engine
        .begin_resolve_session_if_absent(None, vec![])
        .await
        .unwrap();
    assert!(h.engine.has_conflicts().await.unwrap());

    // Merge-tool resolution clears the conflict and finishes back on the branch we started on.
    h.engine.resolve(true).await.unwrap();
    assert!(!h.engine.has_conflicts().await.unwrap(), "merge tool should resolve the conflict");
    assert!(!h.engine.has_resolve_session(), "session cleared on finish");
    assert_eq!(
        h.engine.current_branch().await.unwrap().as_deref(),
        Some("feat-b")
    );
}

/// Run a raw `jj` command in `root` (for test setup that jjk doesn't expose).
fn jj(root: &Path, args: &[&str]) {
    let ok = Command::new("jj")
        .arg("-R")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "jj {:?} failed in {:?}", args, root);
}

#[tokio::test]
async fn sync_with_branch_stacked_on_merged_immutable_commits() {
    // Regression: tracking an existing branch that was part of an already-merged stack leaves
    // immutable commits between trunk and your new branch. derive/ls must exclude them, and sync
    // must not try to rewrite them (jj refuses; it used to crash with "Commit ... is immutable").
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();

    // Seed trunk.
    write(&root, "base.txt", "base\n");
    jj(&root, &["describe", "-m", "trunk"]);
    jj(&root, &["bookmark", "create", "main", "-r", "@"]);
    jj(&root, &["new", "-m", ""]);

    // An older, already-landed branch: push it, then untrack + forget so its commits become
    // immutable (reachable only from an untracked remote bookmark).
    write(&root, "old.txt", "old\n");
    jj(&root, &["commit", "-m", "old work"]);
    jj(&root, &["bookmark", "create", "feat-old", "-r", "@-"]);
    jj(&root, &["git", "push", "-b", "main", "-b", "feat-old"]);
    jj(&root, &["bookmark", "untrack", "feat-old@origin"]);
    jj(&root, &["bookmark", "forget", "feat-old"]);

    // A new branch stacked on top of those (now immutable) commits.
    write(&root, "new.txt", "new\n");
    jj(&root, &["commit", "-m", "new work"]);
    jj(&root, &["bookmark", "create", "feat-new", "-r", "@-"]);

    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // Derivation excludes the immutable commits: feat-new is one branch with one commit.
    let stack = h.engine.derive_stack().await.unwrap();
    assert_eq!(
        stack.branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["feat-new"]
    );
    assert_eq!(stack.branch("feat-new").unwrap().commit_count(), 1, "no immutable commits in range");

    // sync must complete (not crash on immutable commits) and rebase feat-new straight onto trunk.
    h.engine.sync(true).await.unwrap();

    let after = h.engine.derive_stack().await.unwrap();
    let feat_new = after.branch("feat-new").unwrap();
    assert!(
        feat_new.commits[0].parents.contains(&after.trunk),
        "feat-new should be rebased directly onto trunk (parents {:?}, trunk {:?})",
        feat_new.commits[0].parents,
        after.trunk
    );
}

/// rev-parse a branch on the bare remote (to verify pushes / non-pushes).
fn remote_sha(bare: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["rev-parse", &format!("refs/heads/{branch}")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn sync_no_push_reconciles_locally_without_pushing() {
    let mut h = setup_with_remote().await;
    build_two_branch_stack(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(jjk::engine::SubmitScope::Stack).await.unwrap();

    // Squash-merge feat-a on the remote.
    let work = clone_remote(&h);
    git(work.path(), &["checkout", "main"]);
    write(work.path(), "a.txt", "alpha\n");
    git(work.path(), &["add", "a.txt"]);
    git(work.path(), &["commit", "-m", "feat-a (squash)"]);
    git(work.path(), &["push", "origin", "main"]);
    fake.set_merged("feat-a");

    let bare = h.remote.path().join("origin.git");
    let feat_b_before = remote_sha(&bare, "feat-b");

    // --no-push: reconcile local state only.
    let report = h.engine.sync(false).await.unwrap();
    assert!(
        report.notes.iter().any(|n| n.contains("local state only")),
        "should note it skipped pushing: {:?}",
        report.notes
    );

    // Local reconciliation DID happen: feat-a dropped, feat-b rebased onto trunk.
    let stack = h.engine.derive_stack().await.unwrap();
    assert_eq!(
        stack.branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["feat-b"]
    );
    assert!(stack.branch("feat-b").unwrap().commits[0].parents.contains(&stack.trunk));

    // But nothing was pushed and no PR was retargeted.
    assert_eq!(remote_sha(&bare, "feat-b"), feat_b_before, "feat-b must NOT be force-pushed");
    assert_eq!(
        fake.pr_for("feat-b").unwrap().base,
        "feat-a",
        "PR base must NOT be retargeted in --no-push mode"
    );
}
