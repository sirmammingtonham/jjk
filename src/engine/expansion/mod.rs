//! Domain Expansion — persisted mode state + module aggregation.
//!
//! State lives in `.jj/jjk/expansion.json` (a sidecar like undo/resolve). The splitter sketch
//! and algorithms are in the [`atom`], [`patch`], and [`plan`] submodules, re-exported here so
//! callers keep using `expansion::*` paths. PR/nav-comment ids stay in `state.toml`'s
//! `BranchEntry`, keyed by each layer bookmark, so submit/sync reuse unchanged.

mod atom;
mod patch;
mod plan;
pub(crate) use atom::*;
pub(crate) use patch::*;
pub(crate) use plan::*;

use crate::error::{JjkError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How to slice the monolith. These are *instructions to the LLM* (it draws the boundaries), not
/// clustering algorithms — see the design notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Few PRs, each a coherent feature/capability.
    Feature,
    /// Self-contained changes — connected components merged toward a size target.
    #[default]
    Change,
    /// Many thin, minimal-diff slices.
    Slice,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Feature => "feature",
            Mode::Change => "change",
            Mode::Slice => "slice",
        })
    }
}

/// One reconstructed layer's durable identity + cached PR-facing metadata. `slug`/`atom_hashes`
/// give the layer a content-based identity so an incremental re-split can match it (by atom-hash
/// overlap) to the same bookmark→PR→nav-comment across monolith edits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedLayer {
    pub slug: String,
    /// Reserved-namespace bookmark, e.g. `jjk/layer/<slug>`.
    pub bookmark: String,
    /// Stable content hashes of this layer's atoms (for incremental matching).
    #[serde(default)]
    pub atom_hashes: Vec<String>,
    pub title: String,
    pub body: String,
    /// Why these atoms were grouped (the splitter's rationale) — shown by `domain explain`.
    #[serde(default)]
    pub rationale: String,
    /// Whether this layer is judged safe to merge on its own (LLM self-review, or `--verify` result).
    #[serde(default = "default_true")]
    pub backward_compatible: bool,
    /// Any flagged backward-compat risk for this layer.
    #[serde(default)]
    pub compat_notes: String,
}

fn default_true() -> bool {
    true
}

/// Domain-expansion mode state for a repo. Present ⇒ auto-stacking active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpansionState {
    /// The user's single working branch — the source of truth that is decomposed.
    pub monolith: String,
    pub mode: Mode,
    /// Extra natural-language splitting guidance for the LLM, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// Opt-in per-layer build/test command (the hard backward-compat gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_cmd: Option<String>,
    /// Per-monolith splitter model override (`auto`/id/`opus`/`sonnet`/`haiku`); env `JJK_LLM_MODEL`
    /// still wins. `None` ⇒ fall back to `state.config.llm_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-monolith thinking-effort override (`auto`/`off`/`low`/`high`/`max`/number); env
    /// `JJK_LLM_THINKING` still wins. `None` ⇒ fall back to `state.config.llm_thinking`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The reconstructed layers, bottom→top. Empty until the first expansion.
    #[serde(default)]
    pub layers: Vec<PersistedLayer>,
    /// Git commit id of the monolith tip at last expansion — lets a re-expand skip work when the
    /// monolith hasn't moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monolith_commit: Option<String>,
}

impl ExpansionState {
    /// `.jj/jjk/expansion.json` under a repo root.
    pub fn path_for(root: &Path) -> PathBuf {
        root.join(".jj").join("jjk").join("expansion.json")
    }

    pub fn new(monolith: impl Into<String>, mode: Mode) -> Self {
        ExpansionState {
            monolith: monolith.into(),
            mode,
            instruction: None,
            verify_cmd: None,
            model: None,
            thinking: None,
            layers: Vec::new(),
            monolith_commit: None,
        }
    }

    /// Load the mode state if active, else `None` (the common, non-domain case).
    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = Self::path_for(root);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(JjkError::Msg(format!("cannot read {}: {e}", path.display())).into())
            }
        };
        let st: ExpansionState = serde_json::from_str(&text)
            .map_err(|e| JjkError::Msg(format!("corrupt expansion.json: {e}")))?;
        Ok(Some(st))
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path_for(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| JjkError::Msg(format!("failed to serialize expansion state: {e}")))?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Remove the sidecar (deactivate domain mode). No-op if absent.
    pub fn remove(root: &Path) -> Result<()> {
        let path = Self::path_for(root);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(JjkError::Msg(format!("cannot remove {}: {e}", path.display())).into()),
        }
    }

    /// Reserved bookmark namespace for generated layers.
    pub const LAYER_PREFIX: &'static str = "jjk/layer/";

    /// The bookmark name for a layer slug.
    pub fn layer_bookmark(slug: &str) -> String {
        format!("{}{slug}", Self::LAYER_PREFIX)
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::llm::{LayerSpec, SplitPlan};

    fn layer(slug: &str, atoms: &[&str]) -> LayerSpec {
        LayerSpec {
            slug: slug.into(),
            atoms: atoms.iter().map(|s| s.to_string()).collect(),
            title: slug.into(),
            body: String::new(),
            rationale: String::new(),
            backward_compatible: true,
            compat_notes: String::new(),
        }
    }

    #[test]
    fn repair_sweeps_unassigned_into_remainder() {
        // 4 atoms; plan only covers a0,a2 → a1,a3 swept into remainder.
        let mut plan = SplitPlan {
            layers: vec![layer("base", &["a0", "a2"])],
        };
        let missing = repair_completeness(&mut plan, 4);
        assert_eq!(missing, vec!["a1", "a3"]);
        assert_eq!(plan.layers.len(), 2);
        assert_eq!(plan.layers[1].slug, "remainder");
        assert_eq!(plan.layers[1].atoms, vec!["a1", "a3"]);
    }

    #[test]
    fn buckets_pack_components_and_labels_remap() {
        // 5 atoms; edges link {0,1} and {2,3}; 4 is alone → 3 components.
        let edges = vec![
            Edge {
                from: 0,
                to: 1,
                kind: EdgeKind::SameFile,
            },
            Edge {
                from: 2,
                to: 3,
                kind: EdgeKind::SameFile,
            },
        ];
        assert_eq!(components(5, &edges).len(), 3);

        let buckets = bucket_components(5, &edges, 2);
        assert_eq!(buckets, vec![vec![0, 1], vec![2, 3], vec![4]]);

        // Edges internal to bucket {2,3} remap to local indices (0,1); the {0,1} edge is dropped.
        let se = sub_edges(&edges, &buckets[1]);
        assert_eq!(se.len(), 1);
        assert_eq!((se[0].from, se[0].to), (0, 1));

        // A bucket-local plan remaps back to global labels and gets a bucket-prefixed slug.
        let mut plan = SplitPlan {
            layers: vec![layer("x", &["a0", "a1"])],
        };
        remap_and_prefix(&mut plan, &buckets[1], 1);
        assert_eq!(plan.layers[0].atoms, vec!["a2", "a3"]);
        assert_eq!(plan.layers[0].slug, "b1-x");
    }

    #[test]
    fn repair_dedups_and_drops_invalid_and_empty() {
        let mut plan = SplitPlan {
            layers: vec![
                layer("a", &["a0", "a1"]),
                layer("b", &["a1", "a9", "bogus"]), // a1 dup, a9 out-of-range, bogus invalid
                layer("c", &[]),                    // empty
            ],
        };
        let missing = repair_completeness(&mut plan, 2);
        assert!(missing.is_empty());
        assert_eq!(plan.layers.len(), 1);
        assert_eq!(plan.layers[0].atoms, vec!["a0", "a1"]);
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::model::{DiffLine, FileChangeKind, FileDiff, Hunk};

    fn hunk(os: u32, ol: u32, ns: u32, nl: u32, lines: Vec<DiffLine>) -> Hunk {
        Hunk {
            old_start: os,
            old_len: ol,
            new_start: ns,
            new_len: nl,
            lines,
        }
    }

    #[test]
    fn applies_single_hunk_modify() {
        let base = "keep\nold\ntail\n";
        let h = hunk(
            1,
            3,
            1,
            4,
            vec![
                DiffLine::Context("keep".into()),
                DiffLine::Removed("old".into()),
                DiffLine::Added("new".into()),
                DiffLine::Added("extra".into()),
                DiffLine::Context("tail".into()),
            ],
        );
        let out = apply_hunks(base, &[&h]).unwrap();
        assert_eq!(out, "keep\nnew\nextra\ntail\n");
    }

    #[test]
    fn applies_subset_of_two_independent_hunks() {
        // base has two edit regions; including only the second leaves the first untouched.
        let base = "a\nb\nc\nd\ne\n";
        let h1 = hunk(
            1,
            1,
            1,
            1,
            vec![DiffLine::Removed("a".into()), DiffLine::Added("A".into())],
        );
        let h2 = hunk(
            5,
            1,
            5,
            1,
            vec![DiffLine::Removed("e".into()), DiffLine::Added("E".into())],
        );
        // only h2
        assert_eq!(apply_hunks(base, &[&h2]).unwrap(), "a\nb\nc\nd\nE\n");
        // only h1
        assert_eq!(apply_hunks(base, &[&h1]).unwrap(), "A\nb\nc\nd\ne\n");
        // both → full monolith
        assert_eq!(apply_hunks(base, &[&h1, &h2]).unwrap(), "A\nb\nc\nd\nE\n");
    }

    #[test]
    fn applies_added_file_from_empty_base() {
        let h = hunk(
            0,
            0,
            1,
            2,
            vec![DiffLine::Added("hello".into()), DiffLine::Added("world".into())],
        );
        assert_eq!(apply_hunks("", &[&h]).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn deletes_all_lines_to_empty() {
        let base = "x\ny\n";
        let h = hunk(
            1,
            2,
            0,
            0,
            vec![DiffLine::Removed("x".into()), DiffLine::Removed("y".into())],
        );
        assert_eq!(apply_hunks(base, &[&h]).unwrap(), "");
    }

    #[test]
    fn rejects_mismatched_context() {
        let base = "a\nb\n";
        let h = hunk(
            1,
            1,
            1,
            1,
            vec![DiffLine::Removed("WRONG".into()), DiffLine::Added("c".into())],
        );
        assert!(apply_hunks(base, &[&h]).is_err());
    }

    fn file(path: &str, change: FileChangeKind, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff {
            path: path.into(),
            old_path: None,
            change,
            hunks,
        }
    }

    #[test]
    fn modified_multi_hunk_file_yields_one_atom_per_hunk() {
        let f = file(
            "src/a.rs",
            FileChangeKind::Modified,
            vec![
                hunk(1, 1, 1, 1, vec![DiffLine::Added("x".into())]),
                hunk(9, 1, 9, 1, vec![DiffLine::Added("y".into())]),
            ],
        );
        let atoms = extract_atoms(&[f]);
        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].hunks, vec![0]);
        assert_eq!(atoms[1].hunks, vec![1]);
        // distinct identities
        assert_ne!(atoms[0].id, atoms[1].id);
    }

    #[test]
    fn added_file_is_one_atom_and_hash_is_stable() {
        let f = file(
            "new.rs",
            FileChangeKind::Added,
            vec![hunk(
                0,
                0,
                1,
                1,
                vec![DiffLine::Added("fn foo() {}".into())],
            )],
        );
        let a1 = extract_atoms(std::slice::from_ref(&f));
        let a2 = extract_atoms(&[f]);
        assert_eq!(a1.len(), 1);
        assert_eq!(a1[0].id, a2[0].id); // deterministic
        assert!(a1[0].defs.contains(&"foo".to_string()));
    }

    #[test]
    fn defuse_edge_links_user_to_definer() {
        let definer = file(
            "lib.rs",
            FileChangeKind::Added,
            vec![hunk(
                0,
                0,
                1,
                1,
                vec![DiffLine::Added("fn helper() {}".into())],
            )],
        );
        let user = file(
            "main.rs",
            FileChangeKind::Added,
            vec![hunk(
                0,
                0,
                1,
                1,
                vec![DiffLine::Added("helper();".into())],
            )],
        );
        let atoms = extract_atoms(&[definer, user]);
        let edges = build_edges(&atoms);
        // atom 1 (user) uses `helper` defined by atom 0 (definer)
        assert!(edges
            .iter()
            .any(|e| e.kind == EdgeKind::DefUse && e.from == 1 && e.to == 0));
    }
}
