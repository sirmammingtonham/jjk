//! The `Splitter` **port**: decides how a monolith's atoms group into an ordered stack of layers.
//!
//! This is the part of Domain Expansion that makes it work — the LLM (not a heuristic) draws the
//! boundaries, following the mode and the user's instructions. The engine feeds it a cheap,
//! code-free sketch ([`SplitInput`]) and gets back an assignment + naming ([`SplitPlan`]); the
//! engine then validates completeness and reconstructs. Adapters: [`anthropic::AnthropicLlm`] (real)
//! and [`FakeSplitter`] (tests + offline fallback). Mirrors the `Forge`/`Prompter` ports.

pub mod anthropic;

use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A code-free, one-line description of one atom for the LLM. `label` (e.g. `a0`) is a short,
/// per-call handle the model uses in its output — kept tiny to save tokens.
#[derive(Debug, Clone, Serialize)]
pub struct AtomGist {
    pub label: String,
    pub path: String,
    /// add / modify / delete / rename / binary.
    pub kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub defs: Vec<String>,
    pub gist: String,
    /// Lines added by this atom (a rough size signal alongside `removed`).
    pub added: u32,
    /// Lines removed by this atom.
    pub removed: u32,
}

/// An advisory dependency/affinity hint between two atoms (by label).
#[derive(Debug, Clone, Serialize)]
pub struct EdgeHint {
    pub from: String,
    pub to: String,
    /// "defuse" (from references a symbol to defines) or "samefile".
    pub kind: String,
}

/// Everything the splitter sees. Compressed by construction: gists + hints, never raw code.
#[derive(Debug, Clone, Serialize)]
pub struct SplitInput {
    /// "feature" / "change" / "slice".
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    pub atoms: Vec<AtomGist>,
    pub edges: Vec<EdgeHint>,
    /// The monolith's own commit subjects — free grouping signal.
    pub commit_subjects: Vec<String>,
    /// Full hunk text per atom label, served **on demand** via the `get_atom_detail` tool — never
    /// sent up front. Not part of the prompt payload.
    #[serde(skip)]
    pub details: HashMap<String, String>,
}

/// The splitter's decision: an ordered list of layers (bottom→top = stack order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitPlan {
    pub layers: Vec<LayerSpec>,
}

/// One layer the splitter proposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerSpec {
    /// Short kebab-case identity (also the layer bookmark suffix).
    pub slug: String,
    /// Atom labels assigned to this layer.
    pub atoms: Vec<String>,
    /// PR title.
    pub title: String,
    /// PR body / description.
    #[serde(default)]
    pub body: String,
    /// Why these atoms belong together (shown by `domain explain`).
    #[serde(default)]
    pub rationale: String,
    /// The LLM's judgement that this layer is safe to merge on its own.
    #[serde(default = "default_true")]
    pub backward_compatible: bool,
    /// Any compatibility risk the LLM flags.
    #[serde(default)]
    pub compat_notes: String,
}

fn default_true() -> bool {
    true
}

/// The port. One call turns a sketch into an ordered, named split.
#[async_trait]
pub trait Splitter: Send + Sync {
    async fn split(&self, input: &SplitInput) -> Result<SplitPlan>;

    /// Revise a previously proposed plan per the user's natural-language `feedback` (e.g. "combine
    /// these into two PRs"). The default folds the feedback into the instruction and re-splits; the
    /// real adapter shows the model its prior plan + the feedback and asks for an adjusted split.
    async fn revise(
        &self,
        input: &SplitInput,
        _previous: &SplitPlan,
        feedback: &str,
    ) -> Result<SplitPlan> {
        let mut input = input.clone();
        input.instruction = Some(match input.instruction.take() {
            Some(prev) => format!("{prev}\n\nRevision requested: {feedback}"),
            None => format!("Revision requested: {feedback}"),
        });
        self.split(&input).await
    }
}

/// Test + offline fallback splitter. With a scripted plan it returns it verbatim (so tests pin the
/// LLM's *decision*); otherwise it produces a valid-but-unrefined split: one layer per connected
/// component of the dependency/affinity edges. Never makes a network call.
pub struct FakeSplitter {
    scripted: Option<SplitPlan>,
}

impl FakeSplitter {
    /// Returns `plan` verbatim from [`Splitter::split`].
    pub fn scripted(plan: SplitPlan) -> Self {
        Self {
            scripted: Some(plan),
        }
    }

    /// Deterministic connected-component split (the offline fallback).
    pub fn deterministic() -> Self {
        Self { scripted: None }
    }
}

#[async_trait]
impl Splitter for FakeSplitter {
    async fn split(&self, input: &SplitInput) -> Result<SplitPlan> {
        if let Some(plan) = &self.scripted {
            return Ok(plan.clone());
        }
        Ok(deterministic_split(input))
    }
}

/// A valid-but-unrefined split: union-find connected components over the edges, one layer each,
/// ordered by first appearance. Used offline / when no API key is present. The point of the feature
/// is the LLM split; this only guarantees a working stack exists without one.
pub(crate) fn deterministic_split(input: &SplitInput) -> SplitPlan {
    let n = input.atoms.len();
    let index: HashMap<&str, usize> = input
        .atoms
        .iter()
        .enumerate()
        .map(|(i, a)| (a.label.as_str(), i))
        .collect();

    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        // path-compress
        let mut cur = x;
        while parent[cur] != r {
            let next = parent[cur];
            parent[cur] = r;
            cur = next;
        }
        r
    }
    for e in &input.edges {
        if let (Some(&a), Some(&b)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }

    // Group atom indices by component root, preserving first-seen order of roots.
    let mut order: Vec<usize> = Vec::new();
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        if !groups.contains_key(&r) {
            order.push(r);
        }
        groups.entry(r).or_default().push(i);
    }

    let mut layers = Vec::new();
    for (li, root) in order.iter().enumerate() {
        let members = &groups[root];
        let atoms: Vec<String> = members.iter().map(|&i| input.atoms[i].label.clone()).collect();
        let title = input
            .atoms
            .get(members[0])
            .map(|a| a.gist.clone())
            .unwrap_or_else(|| format!("layer {}", li + 1));
        layers.push(LayerSpec {
            slug: format!("layer-{}", li + 1),
            atoms,
            title,
            body: String::new(),
            rationale: "deterministic connected-component split (no LLM)".into(),
            backward_compatible: true,
            compat_notes: String::new(),
        });
    }
    SplitPlan { layers }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gist(label: &str, path: &str) -> AtomGist {
        AtomGist {
            label: label.into(),
            path: path.into(),
            kind: "modify".into(),
            defs: vec![],
            gist: format!("modify {path}"),
            added: 1,
            removed: 0,
        }
    }

    #[test]
    fn deterministic_split_groups_connected_atoms() {
        let input = SplitInput {
            mode: "change".into(),
            instruction: None,
            atoms: vec![
                gist("a0", "x.rs"),
                gist("a1", "x.rs"),
                gist("a2", "y.rs"),
            ],
            // a0—a1 connected (same file); a2 isolated.
            edges: vec![EdgeHint {
                from: "a0".into(),
                to: "a1".into(),
                kind: "samefile".into(),
            }],
            commit_subjects: vec![],
            details: HashMap::new(),
        };
        let plan = deterministic_split(&input);
        assert_eq!(plan.layers.len(), 2);
        assert_eq!(plan.layers[0].atoms, vec!["a0", "a1"]);
        assert_eq!(plan.layers[1].atoms, vec!["a2"]);
    }
}
