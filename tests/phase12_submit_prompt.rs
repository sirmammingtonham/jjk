//! `submit` interactive UX (git-spice-style): for a **new** branch it consults the installed
//! `Prompter` for title/body/draft; existing PRs are updated without prompting. Also covers the
//! `--draft` option (via `SubmitOptions`) and the opt-in PR-body easter egg.

mod common;

use common::{setup_with_remote, write, FakeForge, SharedForge};
use jjk::engine::{SubmitOptions, SubmitScope};
use jjk::prompt::{PrDraft, Prompter};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A prompter that counts how often it's asked and rewrites the draft to fixed values.
struct ScriptedPrompter {
    calls: Arc<AtomicUsize>,
    title: String,
    body: String,
    draft: bool,
}

impl Prompter for ScriptedPrompter {
    fn new_pr(&self, _branch: &str, _base: &str, _defaults: PrDraft) -> jjk::error::Result<Option<PrDraft>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(PrDraft {
            title: self.title.clone(),
            body: self.body.clone(),
            draft: self.draft,
        }))
    }
}

/// A prompter that always declines to create the PR.
struct DecliningPrompter {
    calls: Arc<AtomicUsize>,
}

impl Prompter for DecliningPrompter {
    fn new_pr(&self, _branch: &str, _base: &str, _defaults: PrDraft) -> jjk::error::Result<Option<PrDraft>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

async fn one_branch(h: &mut common::RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").await.unwrap();
}

/// A two-branch stack (so the stack-navigation comment is created).
async fn two_branches(h: &mut common::RepoWithRemote) {
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feat-a", true).await.unwrap();
    write(&root, "a.txt", "a\n");
    h.engine.commit("feat-a: first").await.unwrap();
    h.engine.branch_create("feat-b", true).await.unwrap();
    write(&root, "b.txt", "b\n");
    h.engine.commit("feat-b: first").await.unwrap();
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
async fn prompts_only_for_new_prs_and_uses_the_prompted_values() {
    let mut h = setup_with_remote().await;
    one_branch(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    let calls = Arc::new(AtomicUsize::new(0));
    h.engine.set_prompter(Box::new(ScriptedPrompter {
        calls: calls.clone(),
        title: "Custom title".into(),
        body: "Custom body".into(),
        draft: true,
    }));

    // First submit: the branch is new, so the prompter is consulted and its values are used.
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "prompted once for the new PR");
    assert_eq!(fake.pr_for("feat-a").unwrap().title, "Custom title");
    assert_eq!(fake.body_for("feat-a").as_deref(), Some("Custom body"));
    assert_eq!(fake.draft_for("feat-a"), Some(true), "created as a draft");

    // Re-submit: the PR now exists, so it is updated *without* prompting again.
    write(h.repo.path(), "a.txt", "a\nmore\n");
    h.engine.commit("feat-a: more").await.unwrap();
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "existing PR is not re-prompted");
    assert_eq!(fake.count(), 1, "no duplicate PR");
}

#[tokio::test]
async fn declining_the_prompt_skips_pr_creation() {
    let mut h = setup_with_remote().await;
    one_branch(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    let calls = Arc::new(AtomicUsize::new(0));
    h.engine.set_prompter(Box::new(DecliningPrompter { calls: calls.clone() }));

    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "prompted");
    assert_eq!(fake.count(), 0, "no PR created when the prompt is declined");
    assert!(h.engine.state().pr_of("feat-a").is_none(), "no PR recorded in state");
}

#[tokio::test]
async fn draft_option_fills_through_the_default_prompter() {
    let mut h = setup_with_remote().await;
    one_branch(&mut h).await;
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // No prompter installed → engine keeps its AutoFill default; --draft still applies.
    h.engine
        .submit_with(
            SubmitScope::Stack,
            SubmitOptions {
                draft: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fake.draft_for("feat-a"), Some(true), "draft flag flows through AutoFill");
}

#[tokio::test]
async fn yuji_easter_egg_appends_to_nav_comment_only_when_configured() {
    let needle = "yuji-itadori-son.png";

    // Control: without the config, neither the nav comment nor the body carries the flourish.
    {
        let mut h = setup_with_remote().await;
        two_branches(&mut h).await;
        let fake = Arc::new(FakeForge::new());
        h.engine.set_forge(Box::new(SharedForge(fake.clone())));
        h.engine.submit(SubmitScope::Stack).await.unwrap();
        let pr_a = fake.pr_for("feat-a").unwrap().number;
        assert!(
            !fake.comments_on(pr_a).iter().any(|c| c.contains(needle)),
            "no easter egg in the nav comment without the config"
        );
        assert!(
            !fake.body_for("feat-a").unwrap().contains(needle),
            "and never in the PR body"
        );
    }

    // Opt in via jj config → the flourish lands in the stack-navigation comment (not the body).
    {
        let mut h = setup_with_remote().await;
        jj_config_set(h.repo.path(), "yuji", "it_doesnt_matter");
        two_branches(&mut h).await;
        let fake = Arc::new(FakeForge::new());
        h.engine.set_forge(Box::new(SharedForge(fake.clone())));
        h.engine.submit(SubmitScope::Stack).await.unwrap();
        let pr_a = fake.pr_for("feat-a").unwrap().number;
        assert!(
            fake.comments_on(pr_a).iter().any(|c| c.contains(needle)),
            "easter egg present in the nav comment when yuji = it_doesnt_matter"
        );
        assert!(
            !fake.body_for("feat-a").unwrap().contains(needle),
            "easter egg stays out of the PR body"
        );
    }
}
