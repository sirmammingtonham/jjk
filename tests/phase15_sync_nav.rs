//! `sync` now keeps the stack-navigation comments fresh (the stack changes during sync: merged
//! branches drop out, bases move). This also means the opt-in flourish, if configured *after* the
//! stack was first submitted, shows up on the next sync — it previously never did because sync
//! didn't touch comments.

mod common;

use common::{setup_with_remote, write, FakeForge, RepoWithRemote, SharedForge};
use jjk::engine::SubmitScope;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn two_branch_stack(h: &mut RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").unwrap();
    h.engine.branch_create("feat-b", true).unwrap();
    write(&root, "b.txt", "b\n");
    h.engine.commit("feat-b: first").unwrap();
}

fn jj_config_set(root: &Path, key: &str, value: &str) {
    let ok = Command::new("jj")
        .arg("-R")
        .arg(root)
        .args(["config", "set", "--repo", key, value])
        .status()
        .unwrap()
        .success();
    assert!(ok, "jj config set {key} failed");
}

#[tokio::test]
async fn sync_adds_the_flourish_configured_after_the_initial_submit() {
    let needle = "yuji-itadori-son.png";
    let mut h = setup_with_remote();
    two_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // Submit the stack with the flourish NOT yet configured.
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    let pr_a = fake.pr_for("feat-a").unwrap().number;
    assert!(
        !fake.comments_on(pr_a).iter().any(|c| c.contains(needle)),
        "no flourish before it is configured"
    );

    // Configure it now, then sync — which previously left comments untouched.
    jj_config_set(h.repo.path(), "yuji", "it_doesnt_matter");
    h.engine.sync(true).await.unwrap();

    // The nav comment was refreshed by sync and now carries the flourish (still one comment).
    assert_eq!(fake.comments_on(pr_a).len(), 1, "still exactly one nav comment");
    assert!(
        fake.comments_on(pr_a).iter().any(|c| c.contains(needle)),
        "sync refreshed the nav comment and added the flourish"
    );
}

#[tokio::test]
async fn sync_refreshes_nav_comments_without_duplicating() {
    let mut h = setup_with_remote();
    two_branch_stack(&mut h);
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    let pr_a = fake.pr_for("feat-a").unwrap().number;
    let pr_b = fake.pr_for("feat-b").unwrap().number;
    assert_eq!(fake.comments_on(pr_a).len(), 1);

    // A no-merge sync should refresh, not duplicate, the comments.
    h.engine.sync(true).await.unwrap();
    assert_eq!(fake.comments_on(pr_a).len(), 1, "no duplicate nav comment on feat-a");
    assert_eq!(fake.comments_on(pr_b).len(), 1, "no duplicate nav comment on feat-b");
}
