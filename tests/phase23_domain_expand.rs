//! Domain Expansion: the splitter pipeline (diff → atoms → split → preview) against real jj.
//! Uses the deterministic / scripted `FakeSplitter` so no network is touched.

mod common;

use common::{init_identity, setup_with_remote, write, FakeForge, SharedForge};
use jjk::engine::expansion::{ExpansionState, Mode};
use jjk::engine::{Engine, SubmitScope};
use jjk::llm::{FakeSplitter, LayerSpec, SplitPlan};
use jjk::model::ChangeId;
use jjk::prompt::{PrDraft, Prompter, SplitReview};
use jjk::vcs::PushOpts;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git").current_dir(dir).args(args).status().unwrap().success(),
        "git {args:?} failed"
    );
}

/// A splitter that puts every atom of whatever it's given into a single layer (adapts to the input,
/// so it works per-bucket in the hierarchical path — unlike a fixed scripted plan).
struct OneLayerSplitter;
#[async_trait::async_trait]
impl jjk::llm::Splitter for OneLayerSplitter {
    async fn split(&self, input: &jjk::llm::SplitInput) -> jjk::error::Result<SplitPlan> {
        Ok(SplitPlan {
            layers: vec![LayerSpec {
                slug: "all".into(),
                atoms: input.atoms.iter().map(|a| a.label.clone()).collect(),
                title: "All".into(),
                body: String::new(),
                rationale: String::new(),
                backward_compatible: true,
                compat_notes: String::new(),
            }],
        })
    }
}

/// A splitter that splits granularly (one layer per atom) but, when asked to `revise`, coarsens to
/// exactly two layers — modelling the conversational "combine these into two PRs" feedback.
struct RevisingSplitter;
#[async_trait::async_trait]
impl jjk::llm::Splitter for RevisingSplitter {
    async fn split(&self, input: &jjk::llm::SplitInput) -> jjk::error::Result<SplitPlan> {
        Ok(SplitPlan {
            layers: input
                .atoms
                .iter()
                .enumerate()
                .map(|(i, a)| layer(&format!("l{i}"), &[a.label.as_str()], &format!("layer {i}")))
                .collect(),
        })
    }
    async fn revise(
        &self,
        input: &jjk::llm::SplitInput,
        _previous: &SplitPlan,
        _feedback: &str,
    ) -> jjk::error::Result<SplitPlan> {
        let labels: Vec<&str> = input.atoms.iter().map(|a| a.label.as_str()).collect();
        let mid = labels.len() / 2;
        Ok(SplitPlan {
            layers: vec![
                layer("first", &labels[..mid], "First half"),
                layer("second", &labels[mid..], "Second half"),
            ],
        })
    }
}

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

    e.domain_activate(Mode::Change, None, None, None, None).await.unwrap();
    // Force the offline splitter so the test never reaches the network.
    e.set_splitter(Box::new(FakeSplitter::deterministic()));

    let report = e.domain_split(/*preview=*/ true, false).await.unwrap();
    let text = report.notes.join("\n");

    assert!(text.contains("proposed split"), "got:\n{text}");
    assert!(text.contains("preview only"));
    // Completeness: every changed file must appear somewhere in the plan.
    for f in ["auth.rs", "db.rs", "ui.rs"] {
        assert!(text.contains(f), "{f} missing from preview:\n{text}");
    }
}

#[tokio::test]
async fn split_diffs_against_merge_base_not_trunk_tip() {
    // The monolith forks from main, then main advances with an UNRELATED file (without the monolith
    // being rebased). A tip-to-tip diff would scatter the inverse of that file through the split;
    // diffing against the merge-base must include only the monolith's own work.
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();

    // Monolith forks here and adds feature.rs.
    e.branch_create("feature", true).await.unwrap();
    write(&root, "feature.rs", "fn feature() {}\n");
    e.commit("feature: add feature.rs").await.unwrap();

    // Advance main past the fork with an unrelated file (sibling of feature off the same base),
    // then point the trunk bookmark at it — main is now ahead and feature was never rebased.
    e.checkout("main").await.unwrap();
    e.branch_create("mainwork", true).await.unwrap();
    write(&root, "unrelated.rs", "fn unrelated() {}\n");
    e.commit("main: unrelated work").await.unwrap();
    let advanced = resolve_tip(&e, "bookmarks(exact:\"mainwork\")").await;
    e.vcs()
        .transaction(&mut |tx| tx.set_bookmark("main", &advanced))
        .unwrap();

    // Back on the monolith, split it.
    e.checkout("feature").await.unwrap();
    e.domain_activate(Mode::Change, None, None, None, None).await.unwrap();
    e.set_splitter(Box::new(FakeSplitter::deterministic()));

    let text = e.domain_split(/*preview=*/ true, false).await.unwrap().notes.join("\n");
    assert!(text.contains("feature.rs"), "monolith's own file missing:\n{text}");
    assert!(
        !text.contains("unrelated.rs"),
        "trunk's later file leaked into the split (diffed vs tip, not merge-base):\n{text}"
    );
}

#[tokio::test]
async fn refine_loop_re_splits_from_feedback_before_building() {
    // The model first proposes a granular split (one layer per atom). The user refines ("combine
    // these…"); the splitter re-proposes two layers; the user accepts. The built stack must be the
    // refined two-layer split, not the granular original.
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await; // 3 files → 3 atoms → granular = 3 layers

    e.domain_activate(Mode::Change, None, None, None, None).await.unwrap();
    e.set_splitter(Box::new(RevisingSplitter));
    // One Revise verdict, then the prompter accepts the re-proposed plan.
    e.set_prompter(Box::new(ScriptedPrompter::new(SplitReview::Revise(
        "combine these into two PRs".into(),
    ))));

    e.domain_split(/*preview=*/ false, /*no_review=*/ false).await.unwrap();

    let layers = e
        .vcs()
        .bookmarks()
        .await
        .unwrap()
        .into_iter()
        .filter(|b| b.name.starts_with("jjk/layer/"))
        .count();
    assert_eq!(layers, 2, "granular split was refined to two layers before building");
}

#[tokio::test]
async fn engine_respects_the_splitter_grouping_without_reclustering() {
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await;
    e.domain_activate(Mode::Feature, None, None, None, None).await.unwrap();

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

    let report = e.domain_split(true, false).await.unwrap();
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

    e.domain_activate(Mode::Slice, Some("group by API surface".into()), None, None, None)
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
    e.domain_activate(Mode::Change, None, None, None, None).await.unwrap();

    // 3 added files → atoms a0..a2. Script two layers; the engine must build a stack whose top tree
    // equals the monolith and whose bottom layer is a strict subset.
    let plan = SplitPlan {
        layers: vec![
            layer("base", &["a0", "a1"], "Base layer"),
            layer("top", &["a2"], "Top layer"),
        ],
    };
    e.set_splitter(Box::new(FakeSplitter::scripted(plan)));
    e.domain_split(/*preview=*/ false, /*no_review=*/ true).await.unwrap();

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
        .domain_activate(Mode::Change, None, None, None, None)
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
    // Nav comment posted on the stack, and it subtly notes the stack was auto-split.
    let base_pr = fake.pr_for("jjk/layer/base").unwrap().number;
    let nav = &fake.comments_on(base_pr)[0];
    assert!(nav.contains("stack"), "stack nav comment posted");
    assert!(
        nav.contains("Domain Expansion"),
        "nav comment notes auto-split:\n{nav}"
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
        .domain_activate(Mode::Change, None, None, None, None)
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
        .domain_activate(Mode::Change, None, None, None, None)
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
        .domain_activate(Mode::Change, None, None, None, None)
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
        .domain_activate(Mode::Change, None, None, None, None)
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
async fn domain_sync_surfaces_monolith_rebase_conflicts() {
    let mut h = setup_with_remote().await;
    let root = h.repo.path().to_path_buf();

    // Seed + push trunk with a shared file.
    write(&root, "shared.rs", "base\n");
    h.engine
        .vcs()
        .transaction(&mut |tx| {
            let id = tx.finalize_working_copy("chore: seed trunk")?;
            tx.create_bookmark("main", &id)?;
            Ok(())
        })
        .unwrap();
    h.engine.vcs().push("origin", "main", PushOpts::default()).await.unwrap();

    // Monolith edits the shared file.
    h.engine.branch_create("feature", true).await.unwrap();
    write(&root, "shared.rs", "feature change\n");
    h.engine.commit("feature: edit shared").await.unwrap();
    h.engine
        .domain_activate(Mode::Change, None, None, None, None)
        .await
        .unwrap();
    h.engine.set_splitter(Box::new(FakeSplitter::deterministic()));
    let fake = Arc::new(FakeForge::new());
    h.engine.set_forge(Box::new(SharedForge(fake.clone())));

    // Someone else lands a CONFLICTING edit to the same file on trunk.
    let work = tempfile::tempdir().unwrap();
    let bare = h.remote.path().join("origin.git");
    git(work.path(), &["clone", bare.to_str().unwrap(), "."]);
    git(work.path(), &["config", "user.email", "x@e.com"]);
    git(work.path(), &["config", "user.name", "x"]);
    std::fs::write(work.path().join("shared.rs"), "other change\n").unwrap();
    git(work.path(), &["add", "shared.rs"]);
    git(work.path(), &["commit", "-m", "trunk: conflicting edit"]);
    git(work.path(), &["push", "origin", "main"]);

    // Sync rebases the monolith onto the new trunk → conflict → surfaced, not silently expanded.
    let report = h.engine.sync(/*push=*/ true).await.unwrap();
    assert!(
        !report.conflicts.is_empty(),
        "rebase conflict should surface; notes: {:?}",
        report.notes
    );
    assert_eq!(fake.count(), 0, "no PRs created from a conflicted monolith");
}

#[tokio::test]
async fn hierarchical_split_for_large_changesets() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feature", true).await.unwrap();
    // 201 independent files → 201 atoms, over the 200-atom threshold → two buckets (≤200 each).
    for i in 0..201 {
        write(&root, &format!("f{i}.rs"), &format!("fn f{i}() {{}}\n"));
    }
    e.commit("feature: big change").await.unwrap();
    e.domain_activate(Mode::Change, None, None, None, None).await.unwrap();
    e.set_splitter(Box::new(OneLayerSplitter));

    e.domain_split(/*preview=*/ false, /*no_review=*/ true).await.unwrap();

    let st = ExpansionState::load(&root).unwrap().unwrap();
    assert_eq!(st.layers.len(), 2, "two buckets → two layers (one per bucket)");
    assert!(st.layers[0].slug.starts_with("b0-"));
    assert!(st.layers[1].slug.starts_with("b1-"));

    // Hierarchical reconstruction still satisfies the equivalence invariant.
    let monolith_tip = resolve_tip(&e, "feature").await;
    let top = resolve_tip(&e, &format!("jjk/layer/{}", st.layers.last().unwrap().slug)).await;
    assert!(
        e.vcs().trees_equal(&top, &monolith_tip).await.unwrap(),
        "hierarchical stack top must equal the monolith"
    );
}

#[tokio::test]
async fn verify_passing_keeps_all_layers() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    monolith(&mut e, &root).await;
    e.domain_activate(Mode::Change, None, Some("true".into()), None, None) // `true` passes for every layer
        .await
        .unwrap();
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0", "a1"], "Base"), layer("top", &["a2"], "Top")],
    })));
    let report = e.domain_split(false, true).await.unwrap().notes.join("\n");
    assert!(report.contains("verified all layers"), "got:\n{report}");
    let st = ExpansionState::load(&root).unwrap().unwrap();
    assert_eq!(st.layers.len(), 2, "both layers kept when each builds");
}

#[tokio::test]
async fn verify_failure_folds_layers_forward() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    monolith(&mut e, &root).await;
    e.domain_activate(Mode::Change, None, Some("false".into()), None, None) // `false` fails every layer
        .await
        .unwrap();
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0", "a1"], "Base"), layer("top", &["a2"], "Top")],
    })));
    let report = e.domain_split(false, true).await.unwrap().notes.join("\n");
    assert!(report.contains("merging into the next layer"), "got:\n{report}");

    // The failing bottom layer is folded into the last → a single sealed layer covering everything.
    let st = ExpansionState::load(&root).unwrap().unwrap();
    assert_eq!(st.layers.len(), 1, "all layers folded forward into one");
    assert_eq!(st.layers[0].slug, "base", "keeps the first layer's identity");

    // Equivalence still holds: the single layer reproduces the monolith.
    let monolith_tip = resolve_tip(&e, "feature").await;
    let top = resolve_tip(&e, &format!("jjk/layer/{}", st.layers[0].slug)).await;
    assert!(e.vcs().trees_equal(&top, &monolith_tip).await.unwrap());
}

#[tokio::test]
async fn undo_reverts_domain_activation() {
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await;
    // main records a checkpoint before each mutating command; mirror that here.
    e.checkpoint().await.unwrap();
    e.domain_activate(Mode::Change, None, None, None, None)
        .await
        .unwrap();
    assert!(ExpansionState::load(tmp.path()).unwrap().is_some());

    e.undo().await.unwrap();
    assert!(
        ExpansionState::load(tmp.path()).unwrap().is_none(),
        "undo of activation removes the expansion.json sidecar"
    );
}

#[tokio::test]
async fn status_and_explain_surface_compat_and_rationale() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    monolith(&mut e, &root).await;
    e.domain_activate(Mode::Change, None, None, None, None)
        .await
        .unwrap();
    let plan = SplitPlan {
        layers: vec![LayerSpec {
            slug: "risky".into(),
            atoms: vec!["a0".into(), "a1".into(), "a2".into()],
            title: "Risky layer".into(),
            body: "body".into(),
            rationale: "groups the whole API surface".into(),
            backward_compatible: false,
            compat_notes: "calls helper() before it is defined".into(),
        }],
    };
    e.set_splitter(Box::new(FakeSplitter::scripted(plan)));
    e.domain_split(false, true).await.unwrap();

    let status = e.domain_status().await.unwrap().notes.join("\n");
    assert!(status.contains('⚠'), "status flags the risky layer: {status}");
    assert!(status.contains("calls helper() before it is defined"));

    let explain = e.domain_explain(Some("risky".into())).await.unwrap().notes.join("\n");
    assert!(explain.contains("groups the whole API surface"), "explain shows rationale: {explain}");
}

#[tokio::test]
async fn model_and_effort_persist_for_the_monolith() {
    let (tmp, mut e) = setup().await;
    monolith(&mut e, tmp.path()).await;
    e.domain_activate(Mode::Change, None, None, Some("opus".into()), Some("max".into()))
        .await
        .unwrap();
    let st = ExpansionState::load(tmp.path()).unwrap().unwrap();
    assert_eq!(st.model.as_deref(), Some("opus"));
    assert_eq!(st.thinking.as_deref(), Some("max"));
}

#[tokio::test]
async fn layer_branches_follow_the_monolith_prefix() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    // A monolith with a `user/` prefix → layers reuse that prefix, not the jjk/layer/ namespace.
    e.branch_create("ethan/monolith", true).await.unwrap();
    write(&root, "a.rs", "fn a() {}\n");
    write(&root, "b.rs", "fn b() {}\n");
    e.commit("ethan/monolith: work").await.unwrap();
    e.domain_activate(Mode::Change, None, None, None, None)
        .await
        .unwrap();
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("base", &["a0"], "Base"), layer("top", &["a1"], "Top")],
    })));
    e.domain_split(false, true).await.unwrap();

    let bms = e.vcs().bookmarks().await.unwrap();
    assert!(bms.iter().any(|b| b.name == "ethan/base"), "got: {bms:?}");
    assert!(bms.iter().any(|b| b.name == "ethan/top"));
    assert!(
        !bms.iter().any(|b| b.name.starts_with("jjk/layer/")),
        "prefixed monolith should not use the jjk/layer/ fallback"
    );
}

#[tokio::test]
async fn same_file_can_split_across_layers() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    // Seed a 20-line file on trunk, then edit the first and last lines on the monolith. They're far
    // enough apart (> the diff's hunk-merge window) to produce two separate hunks → two atoms.
    let lines: Vec<String> = (1..=20).map(|n| format!("line{n}")).collect();
    write(&root, "shared.rs", &format!("{}\n", lines.join("\n")));
    e.vcs()
        .transaction(&mut |tx| {
            let id = tx.finalize_working_copy("seed trunk")?;
            tx.create_bookmark("main", &id)?;
            Ok(())
        })
        .unwrap();
    e.branch_create("feature", true).await.unwrap();
    let mut edited = lines.clone();
    edited[0] = "LINE1".into();
    edited[19] = "LINE20".into();
    write(&root, "shared.rs", &format!("{}\n", edited.join("\n")));
    e.commit("feature: edit two regions").await.unwrap();
    e.domain_activate(Mode::Change, None, None, None, None)
        .await
        .unwrap();
    // Put each hunk of the same file in a different layer.
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("first", &["a0"], "First edit"), layer("second", &["a1"], "Second edit")],
    })));
    e.domain_split(false, true).await.unwrap();

    let st = ExpansionState::load(&root).unwrap().unwrap();
    assert_eq!(st.layers.len(), 2, "no remainder — the file split cleanly across two layers");
    let monolith_tip = resolve_tip(&e, "feature").await;
    let top = resolve_tip(&e, "jjk/layer/second").await;
    let base = resolve_tip(&e, "jjk/layer/first").await;
    assert!(e.vcs().trees_equal(&top, &monolith_tip).await.unwrap(), "top ≡ monolith");
    assert!(
        !e.vcs().trees_equal(&base, &monolith_tip).await.unwrap(),
        "the lower layer has only its hunk of the shared file"
    );
}

#[tokio::test]
async fn binary_file_lands_in_its_assigned_layer_not_the_remainder() {
    let (tmp, mut e) = setup().await;
    let root = tmp.path().to_path_buf();
    e.branch_create("feature", true).await.unwrap();
    // A binary file (null bytes) + a text file.
    std::fs::write(root.join("logo.bin"), [0u8, 1, 2, 3, 0, 255, 7]).unwrap();
    write(&root, "x.rs", "fn x() {}\n");
    e.commit("feature: add binary + code").await.unwrap();
    e.domain_activate(Mode::Change, None, None, None, None)
        .await
        .unwrap();
    // logo.bin sorts before x.rs → a0 = binary; assign it to the lower layer.
    e.set_splitter(Box::new(FakeSplitter::scripted(SplitPlan {
        layers: vec![layer("assets", &["a0"], "Binary asset"), layer("code", &["a1"], "Code")],
    })));
    e.domain_split(false, true).await.unwrap();

    let st = ExpansionState::load(&root).unwrap().unwrap();
    assert_eq!(
        st.layers.len(),
        2,
        "binary placed in its layer — no forced remainder layer: {:?}",
        st.layers.iter().map(|l| &l.slug).collect::<Vec<_>>()
    );
    let monolith_tip = resolve_tip(&e, "feature").await;
    let top = resolve_tip(&e, "jjk/layer/code").await;
    assert!(
        e.vcs().trees_equal(&top, &monolith_tip).await.unwrap(),
        "binary reconstructed exactly — top ≡ monolith"
    );
}
