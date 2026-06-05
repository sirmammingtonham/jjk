//! Domain Expansion: the splitter pipeline (diff → atoms → split → preview) against real jj.
//! Uses the deterministic / scripted `FakeSplitter` so no network is touched.

mod common;

use common::{init_identity, setup_with_remote, write, FakeForge, SharedForge};
use jjk::engine::expansion::{ExpansionState, Mode};
use jjk::engine::{Engine, SubmitScope};
use jjk::llm::{FakeSplitter, LayerSpec, SplitPlan};
use jjk::model::ChangeId;
use jjk::prompt::{PrDraft, Prompter, SplitReview};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A prompter that yields one scripted split-review verdict, then accepts.
struct ScriptedPrompter(Mutex<Option<SplitReview>>);
impl ScriptedPrompter {
    fn new(v: SplitReview) -> Self {
        Self(Mutex::new(Some(v)))
    }
}
impl Prompter for ScriptedPrompter {
    fn new_pr(&self, _b: &str, _base: &str, d: PrDraft) -> jjk::error::Result<Option<PrDraft>> {
        Ok(Some(d))
    }
    fn review_split(&self, _p: &SplitPlan, _c: &[String]) -> jjk::error::Result<SplitReview> {
        Ok(self.0.lock().unwrap().take().unwrap_or(SplitReview::Accept))
    }
}

fn layer(slug: &str, atoms: &[&str], title: &str) -> LayerSpec {
    LayerSpec {
        slug: slug.into(),
        atoms: atoms.iter().map(|s| s.to_string()).collect(),
        title: title.into(),
        body: format!("{title} body"),
        rationale: String::new(),
        backward_compatible: true,
        compat_notes: String::new(),
    }
}

async fn resolve_tip(e: &Engine, revset: &str) -> ChangeId {
    e.vcs()
        .resolve(revset)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("revset {revset} resolved to nothing"))
        .change_id
}

async fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into()))
        .await
        .unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

/// Build a monolith branch `feature` touching three independent files.
async fn monolith(e: &mut Engine, root: &std::path::Path) {
    e.branch_create("feature", true).await.unwrap();
    write(root, "auth.rs", "fn login() {}\n");
    write(root, "db.rs", "fn query() {}\n");
    write(root, "ui.rs", "fn render() {}\n");
    e.commit("feature: initial work").await.unwrap();
}

#[tokio::test]
async fn preview_covers_every_changed_file() {
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await;

    e.domain_activate(Mode::Change, None, None).await.unwrap();
    // Force the offline splitter so the test never reaches the network.
    e.set_splitter(Box::new(FakeSplitter::deterministic()));

    let report = e.domain_expand(/*preview=*/ true).await.unwrap();
    let text = report.notes.join("\n");

    assert!(text.contains("proposed split"), "got:\n{text}");
    assert!(text.contains("preview only"));
    // Completeness: every changed file must appear somewhere in the plan.
    for f in ["auth.rs", "db.rs", "ui.rs"] {
        assert!(text.contains(f), "{f} missing from preview:\n{text}");
    }
}

#[tokio::test]
async fn engine_respects_the_splitter_grouping_without_reclustering() {
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await;
    e.domain_activate(Mode::Feature, None, None).await.unwrap();

    // Three added files → three atoms (labels a0..a2). Script a single-layer plan; the engine must
    // honor it (not split by connected components, which would give three layers).
    let plan = SplitPlan {
        layers: vec![LayerSpec {
            slug: "everything".into(),
            atoms: vec!["a0".into(), "a1".into(), "a2".into()],
            title: "All the work".into(),
            body: "single PR".into(),
            rationale: "one cohesive feature".into(),
            backward_compatible: true,
            compat_notes: String::new(),
        }],
    };
    e.set_splitter(Box::new(FakeSplitter::scripted(plan)));

    let report = e.domain_expand(true).await.unwrap();
    let text = report.notes.join("\n");
    assert!(text.contains("1 layer (bottom→top)"), "got:\n{text}");
    assert!(text.contains("All the work"));
}

#[tokio::test]
async fn status_reflects_activation_and_collapse_clears_it() {
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await;

    // Not active yet.
    let before = e.domain_status().await.unwrap().notes.join("\n");
    assert!(before.contains("not active"));

    e.domain_activate(Mode::Slice, Some("group by API surface".into()), None)
        .await
        .unwrap();
    let active = e.domain_status().await.unwrap().notes.join("\n");
    assert!(active.contains("monolith: feature"));
    assert!(active.contains("slice"));
    assert!(active.contains("group by API surface"));

    e.domain_collapse().await.unwrap();
    let after = e.domain_status().await.unwrap().notes.join("\n");
    assert!(after.contains("not active"));
}

#[tokio::test]
async fn reconstruct_builds_a_tree_equivalent_stack() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    monolith(&mut e, &root).await;
    e.domain_activate(Mode::Change, None, None).await.unwrap();

    // 3 added files → atoms a0..a2. Script two layers; the engine must build a stack whose top tree
    // equals the monolith and whose bottom layer is a strict subset.
    let plan = SplitPlan {
        layers: vec![
            layer("base", &["a0", "a1"], "Base layer"),
            layer("top", &["a2"], "Top layer"),
        ],
    };
    e.set_splitter(Box::new(FakeSplitter::scripted(plan)));
    e.domain_expand(/*preview=*/ false).await.unwrap();

    // Two layer bookmarks exist.
    let bms = e.vcs().bookmarks().await.unwrap();
    assert!(bms.iter().any(|b| b.name == "jjk/layer/base"));
    assert!(bms.iter().any(|b| b.name == "jjk/layer/top"));

    let st = ExpansionState::load(&root).unwrap().unwrap();
    assert_eq!(st.layers.len(), 2);

    let monolith_tip = resolve_tip(&e, "feature").await;
    let top = resolve_tip(&e, "jjk/layer/top").await;
    let base = resolve_tip(&e, "jjk/layer/base").await;

    // Hard invariant: top of stack ≡ monolith (no changes lost).
    assert!(
        e.vcs().trees_equal(&top, &monolith_tip).await.unwrap(),
        "top layer must equal the monolith"
    );
    // The bottom layer is a real subset (missing the top layer's file).
    assert!(
        !e.vcs().trees_equal(&base, &monolith_tip).await.unwrap(),
        "base layer should not already equal the monolith"
    );

    // The reconstructed stack derives, anchored at the top layer, as base→top.
    let stack = e.derive_stack_at(Some(&top)).await.unwrap();
    let names: Vec<&str> = stack.branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["jjk/layer/base", "jjk/layer/top"]);
}

#[tokio::test]
async fn submit_in_domain_mode_opens_a_pr_per_layer_with_correct_bases() {
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feature", true).await.unwrap();
    write(&root, "auth.rs", "fn login() {}\n");
    write(&root, "db.rs", "fn query() {}\n");
    write(&root, "ui.rs", "fn render() {}\n");
    h.engine.commit("feature: work").await.unwrap();

    h.engine
        .domain_activate(Mode::Change, None, None)
        .await
        .unwrap();
    let plan = SplitPlan {
        layers: vec![
            layer("base", &["a0", "a1"], "Base layer"),
            layer("top", &["a2"], "Top layer"),
        ],
    };
    h.engine.set_splitter(Box::new(FakeSplitter::scripted(plan)));
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // submit expands the monolith, then opens one PR per layer with stacked bases + nav comments.
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    assert_eq!(fake.count(), 2, "one PR per layer");
    assert_eq!(
        fake.pr_for("jjk/layer/base").unwrap().base,
        "main",
        "bottom layer based on trunk"
    );
    assert_eq!(
        fake.pr_for("jjk/layer/top").unwrap().base,
        "jjk/layer/base",
        "top layer based on the layer below it"
    );
    // Nav comment posted on the stack.
    let base_pr = fake.pr_for("jjk/layer/base").unwrap().number;
    assert!(
        fake.comments_on(base_pr).iter().any(|c| c.contains("stack")),
        "stack nav comment posted"
    );

    // Idempotent: re-submitting the unchanged monolith creates no duplicates.
    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.count(), 2, "no duplicate PRs on re-submit");
}

#[tokio::test]
async fn review_abort_creates_nothing() {
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feature", true).await.unwrap();
    write(&root, "a.rs", "fn a() {}\n");
    write(&root, "b.rs", "fn b() {}\n");
    h.engine.commit("feature: work").await.unwrap();
    h.engine
        .domain_activate(Mode::Change, None, None)
        .await
        .unwrap();
    h.engine.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0"], "Base"), layer("top", &["a1"], "Top")],
    })));
    h.engine.set_prompter(Box::new(ScriptedPrompter::new(SplitReview::Abort)));
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    let err = h.engine.submit(SubmitScope::Stack).await;
    assert!(err.is_err(), "abort should error out of submit");
    assert_eq!(fake.count(), 0, "no PRs created on abort");
    let bms = h.engine.vcs().bookmarks().await.unwrap();
    assert!(
        !bms.iter().any(|b| b.name.starts_with("jjk/layer/")),
        "no layer bookmarks created on abort"
    );
}

#[tokio::test]
async fn review_edit_substitutes_the_plan() {
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feature", true).await.unwrap();
    write(&root, "a.rs", "fn a() {}\n");
    write(&root, "b.rs", "fn b() {}\n");
    h.engine.commit("feature: work").await.unwrap();
    h.engine
        .domain_activate(Mode::Change, None, None)
        .await
        .unwrap();
    // Splitter proposes two layers; the reviewer edits it down to one.
    h.engine.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0"], "Base"), layer("top", &["a1"], "Top")],
    })));
    let edited = SplitPlan {
        layers: vec![layer("merged", &["a0", "a1"], "Merged")],
    };
    h.engine
        .set_prompter(Box::new(ScriptedPrompter::new(SplitReview::Edit(edited))));
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(SubmitScope::Stack).await.unwrap();
    assert_eq!(fake.count(), 1, "edited plan collapses to a single PR");
    assert!(fake.pr_for("jjk/layer/merged").is_some());
}

#[tokio::test]
async fn re_submit_after_editing_monolith_keeps_pr_numbers() {
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feature", true).await.unwrap();
    write(&root, "auth.rs", "fn login() {}\n");
    write(&root, "db.rs", "fn query() {}\n");
    write(&root, "ui.rs", "fn render() {}\n");
    h.engine.commit("feature: work").await.unwrap();
    h.engine
        .domain_activate(Mode::Change, None, None)
        .await
        .unwrap();
    h.engine.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0", "a1"], "Base"), layer("top", &["a2"], "Top")],
    })));
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(SubmitScope::Stack).await.unwrap();
    let base_pr = fake.pr_for("jjk/layer/base").unwrap().number;
    let top_pr = fake.pr_for("jjk/layer/top").unwrap().number;

    // Address "feedback" in the monolith and re-submit.
    write(&root, "auth.rs", "fn login() {}\n// reviewed\n");
    h.engine.commit("feature: address feedback").await.unwrap();
    h.engine.submit(SubmitScope::Stack).await.unwrap();

    assert_eq!(fake.count(), 2, "still two PRs after re-split");
    assert_eq!(fake.pr_for("jjk/layer/base").unwrap().number, base_pr, "base PR stable");
    assert_eq!(fake.pr_for("jjk/layer/top").unwrap().number, top_pr, "top PR stable");
}

#[tokio::test]
async fn domain_sync_keeps_the_stack_stable() {
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();
    h.engine.branch_create("feature", true).await.unwrap();
    write(&root, "auth.rs", "fn login() {}\n");
    write(&root, "db.rs", "fn query() {}\n");
    write(&root, "ui.rs", "fn render() {}\n");
    h.engine.commit("feature: work").await.unwrap();
    h.engine
        .domain_activate(Mode::Change, None, None)
        .await
        .unwrap();
    h.engine.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0", "a1"], "Base"), layer("top", &["a2"], "Top")],
    })));
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    h.engine.submit(SubmitScope::Stack).await.unwrap();
    let base_pr = fake.pr_for("jjk/layer/base").unwrap().number;

    // Edit the monolith, then sync: fetch + rebase monolith + re-expand + re-submit, stack stable.
    write(&root, "db.rs", "fn query() {}\n// tweak\n");
    h.engine.commit("feature: tweak").await.unwrap();
    h.engine.sync(/*push=*/ true).await.unwrap();

    assert_eq!(fake.count(), 2, "sync keeps two PRs");
    assert_eq!(
        fake.pr_for("jjk/layer/base").unwrap().number,
        base_pr,
        "base PR stable across sync"
    );
}

#[tokio::test]
async fn verify_command_runs_and_reports() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    monolith(&mut e, &root).await;
    // `true` passes for every layer.
    e.domain_activate(Mode::Change, None, Some("true".into()))
        .await
        .unwrap();
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0", "a1"], "Base"), layer("top", &["a2"], "Top")],
    })));
    let pass = e.domain_expand(false).await.unwrap().notes.join("\n");
    assert!(pass.contains("verified all layers"), "got:\n{pass}");

    // `false` fails for every layer → surfaced as a warning.
    e.domain_activate(Mode::Change, None, Some("false".into()))
        .await
        .unwrap();
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0", "a1"], "Base"), layer("top", &["a2"], "Top")],
    })));
    let fail = e.domain_expand(false).await.unwrap().notes.join("\n");
    assert!(fail.contains("verify"), "got:\n{fail}");
    assert!(fail.contains("failed"), "got:\n{fail}");
}
