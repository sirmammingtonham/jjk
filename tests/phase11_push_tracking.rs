//! `jjk push`/`submit` adopts a non-tracking remote bookmark automatically. Workflow: a branch was
//! pushed / opened as a PR before being tracked in jjk, leaving a non-tracking `name@origin`; jj
//! then refuses to push by name. jjk should `bookmark track` and retry rather than erroring.

mod common;

use common::{setup_with_remote, write, RepoWithRemote};
use jjk::engine::Engine;
use jjk::vcs::PushOpts;
use std::path::Path;
use std::process::Command;

fn jj(dir: &Path, args: &[&str]) {
    let ok = Command::new("jj")
        .arg("-R")
        .arg(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "jj {args:?} failed in {dir:?}");
}

fn seed_main(e: &mut Engine, root: &Path) {
    write(root, "README.md", "# seed\n");
    e.vcs()
        .transaction(&mut |tx| {
            let id = tx.finalize_working_copy("chore: seed trunk")?;
            tx.create_bookmark("main", &id)?;
            Ok(())
        })
        .unwrap();
    e.vcs().push("origin", "main", PushOpts::default()).unwrap();
}

#[test]
fn push_auto_tracks_a_non_tracking_remote_bookmark() {
    let mut h: RepoWithRemote = setup_with_remote();
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root);

    h.engine.branch_create("feat", true).unwrap();
    write(&root, "f.txt", "x\n");
    h.engine.commit("feat work").unwrap();
    h.engine.push_current().unwrap();

    // Simulate the pre-tracking situation: drop the local→remote tracking link.
    let ok = Command::new("jj")
        .arg("-R")
        .arg(&root)
        .args(["bookmark", "untrack", "feat", "--remote", "origin"])
        .status()
        .unwrap()
        .success();
    assert!(ok, "untrack failed");

    // Advance and push again — previously this failed with "Non-tracking remote bookmark exists".
    write(&root, "f.txt", "x\nmore\n");
    h.engine.commit("feat more").unwrap();
    h.engine.push_current().expect("push auto-tracks the non-tracking remote bookmark and succeeds");
}

#[test]
fn push_auto_resolves_a_conflicted_bookmark() {
    let mut h: RepoWithRemote = setup_with_remote();
    let root = h.repo.path().to_path_buf();
    seed_main(&mut h.engine, &root);

    h.engine.branch_create("feat", true).unwrap();
    write(&root, "f.txt", "x\n");
    h.engine.commit("feat work").unwrap();
    h.engine.push_current().unwrap();

    // Diverge the remote from a second clone: rewrite `feat` to a sibling and push it.
    let bare = h.remote.path().join("origin.git");
    let other = tempfile::tempdir().unwrap();
    let clone_dir = other.path().join("clone");
    let ok = Command::new("jj")
        .args(["git", "clone", "--colocate", bare.to_str().unwrap()])
        .arg(&clone_dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "jj git clone failed");
    let b = clone_dir.as_path();
    jj(b, &["bookmark", "track", "feat@origin"]);
    std::fs::write(b.join("r.txt"), "remote rewrite\n").unwrap();
    jj(b, &["describe", "-m", "remote rewrite"]);
    jj(b, &["bookmark", "set", "feat", "-r", "@", "-B"]);
    jj(b, &["new"]);
    jj(b, &["git", "push", "--remote", "origin", "-b", "feat"]);

    // Rewrite `feat` locally a different way, then fetch `feat` — the bookmark becomes conflicted.
    // (engine.fetch() only fetches trunk now, so fetch the branch directly to reproduce the clash.)
    write(&root, "f.txt", "x\nlocal2\n");
    h.engine.commit("feat more local").unwrap();
    jj(&root, &["git", "fetch", "--remote", "origin", "--branch", "feat"]);

    // Previously errored "Bookmark feat is conflicted"; now resolves to local and pushes.
    h.engine.push_current().expect("push resolves the conflicted bookmark to local and succeeds");
}
