//! LLM split payload, plan completeness/repair, layer matching, and hierarchical bucketing.

use crate::llm::{AtomGist, EdgeHint, LayerSpec, SplitInput, SplitPlan};
use crate::model::{ChangeId, DiffLine, FileDiff};
use super::atom::{change_word, Atom, Edge, EdgeKind};
use super::{Mode, PersistedLayer};

// ---- LLM payload + plan resolution -------------------------------------------------------------

/// Everything `compute_split` produces: the source diff + atoms and the (completeness-repaired)
/// plan, plus the trunk/monolith anchors needed for reconstruction. Atom labels are `a{index}`.
pub(crate) struct ResolvedSplit {
    pub atoms: Vec<Atom>,
    pub files: Vec<FileDiff>,
    pub trunk: ChangeId,
    pub monolith_tip: ChangeId,
    pub plan: SplitPlan,
}

/// Atom label used in the LLM payload / plan: `a{index}`.
pub(crate) fn atom_label(index: usize) -> String {
    format!("a{index}")
}

/// Parse an `a{index}` label back to an atom index.
pub(crate) fn label_index(label: &str) -> Option<usize> {
    label.strip_prefix('a').and_then(|s| s.parse().ok())
}

/// Build the compressed, code-free LLM payload from the sketch. Full hunk text is stashed in
/// `details` (served on demand via the `get_atom_detail` tool), never in the catalog.
pub(crate) fn build_split_input(
    mode: Mode,
    instruction: Option<String>,
    atoms: &[Atom],
    edges: &[Edge],
    files: &[FileDiff],
    commit_subjects: Vec<String>,
) -> SplitInput {
    let gists = atoms
        .iter()
        .enumerate()
        .map(|(i, a)| AtomGist {
            label: atom_label(i),
            path: a.path.clone(),
            kind: change_word(a.change).to_string(),
            defs: a.defs.clone(),
            gist: a.gist.clone(),
            size: a.added + a.removed,
        })
        .collect();
    let hints = edges
        .iter()
        .map(|e| EdgeHint {
            from: atom_label(e.from),
            to: atom_label(e.to),
            kind: match e.kind {
                EdgeKind::DefUse => "defuse",
                EdgeKind::SameFile => "samefile",
            }
            .to_string(),
        })
        .collect();
    let details = atoms
        .iter()
        .enumerate()
        .map(|(i, a)| (atom_label(i), detail_text(files, a)))
        .collect();
    SplitInput {
        mode: mode.to_string(),
        instruction,
        atoms: gists,
        edges: hints,
        commit_subjects,
        details,
    }
}

/// The full diff text for one atom (served on demand to the LLM).
pub(crate) fn detail_text(files: &[FileDiff], atom: &Atom) -> String {
    let f = &files[atom.file_idx];
    let mut s = format!("{} {}\n", change_word(atom.change), f.path);
    for &hi in &atom.hunks {
        let h = &f.hunks[hi];
        s.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_len, h.new_start, h.new_len
        ));
        for line in &h.lines {
            match line {
                DiffLine::Context(t) => s.push_str(&format!(" {t}\n")),
                DiffLine::Added(t) => s.push_str(&format!("+{t}\n")),
                DiffLine::Removed(t) => s.push_str(&format!("-{t}\n")),
            }
        }
    }
    s
}

/// Enforce the completeness invariant on an LLM plan **without** re-grouping: drop invalid/duplicate
/// labels (keeping the first occurrence), remove emptied layers, and sweep any atom the LLM left
/// unassigned into a trailing `remainder` layer. Returns the swept-up (previously unassigned)
/// labels for reporting. This is the only correctness gate at the plan level — the LLM's grouping
/// and ordering are otherwise accepted as-is.
pub(crate) fn repair_completeness(plan: &mut SplitPlan, atom_count: usize) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for layer in &mut plan.layers {
        layer.atoms.retain(|l| {
            label_index(l).is_some_and(|i| i < atom_count) && seen.insert(l.clone())
        });
    }
    plan.layers.retain(|l| !l.atoms.is_empty());

    let missing: Vec<String> = (0..atom_count)
        .map(atom_label)
        .filter(|l| !seen.contains(l))
        .collect();
    if !missing.is_empty() {
        plan.layers.push(LayerSpec {
            slug: "remainder".into(),
            atoms: missing.clone(),
            title: "Remaining changes".into(),
            body: "Changes not assigned to a specific layer.".into(),
            rationale: "completeness remainder (atoms the splitter left unassigned)".into(),
            backward_compatible: true,
            compat_notes: String::new(),
        });
    }
    missing
}

/// Stabilize a freshly-computed plan's layer slugs by matching each layer to an existing persisted
/// layer with the greatest atom-hash (Jaccard) overlap, so a surviving layer keeps its
/// bookmark→PR→nav-comment across an incremental re-split. Mutates `plan` layer slugs in place.
pub(crate) fn match_layers(plan: &mut SplitPlan, existing: &[PersistedLayer], atoms: &[Atom]) {
    if existing.is_empty() {
        return;
    }
    // Atom-hash set for each new layer.
    let new_sets: Vec<std::collections::HashSet<String>> = plan
        .layers
        .iter()
        .map(|l| {
            l.atoms
                .iter()
                .filter_map(|lab| label_index(lab))
                .filter_map(|i| atoms.get(i))
                .map(|a| a.id.clone())
                .collect()
        })
        .collect();
    let old_sets: Vec<std::collections::HashSet<String>> = existing
        .iter()
        .map(|l| l.atom_hashes.iter().cloned().collect())
        .collect();

    // All (new, old, jaccard) candidate pairs, best first.
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (ni, ns) in new_sets.iter().enumerate() {
        for (oi, os) in old_sets.iter().enumerate() {
            let j = jaccard(ns, os);
            if j > 0.5 {
                pairs.push((j, ni, oi));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut new_taken = vec![false; plan.layers.len()];
    let mut old_taken = vec![false; existing.len()];
    for (_, ni, oi) in pairs {
        if new_taken[ni] || old_taken[oi] {
            continue;
        }
        new_taken[ni] = true;
        old_taken[oi] = true;
        plan.layers[ni].slug = existing[oi].slug.clone();
    }
}

// ---- hierarchical split (huge diffs) -----------------------------------------------------------

/// Connected components of the atoms under the dependency/affinity edges (union-find). Each is a
/// list of atom indices; components are ordered by first appearance. Hard-connected atoms stay
/// together, so splitting these across buckets never separates a co-dependent group.
pub(crate) fn components(n: usize, edges: &[Edge]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while p[r] != r {
            r = p[r];
        }
        let mut c = x;
        while p[c] != r {
            let nx = p[c];
            p[c] = r;
            c = nx;
        }
        r
    }
    for e in edges {
        if e.from < n && e.to < n {
            let (a, b) = (find(&mut parent, e.from), find(&mut parent, e.to));
            if a != b {
                parent[a] = b;
            }
        }
    }
    let mut order = Vec::new();
    let mut groups: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        if !groups.contains_key(&r) {
            order.push(r);
        }
        groups.entry(r).or_default().push(i);
    }
    order
        .into_iter()
        .map(|r| {
            let mut v = groups.remove(&r).unwrap();
            v.sort_unstable();
            v
        })
        .collect()
}

/// Pack whole components into buckets of at most `max` atoms (a component larger than `max` becomes
/// its own bucket). Used to bound each LLM call's catalog for very large changesets.
pub(crate) fn bucket_components(n: usize, edges: &[Edge], max: usize) -> Vec<Vec<usize>> {
    let mut buckets: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for comp in components(n, edges) {
        if !cur.is_empty() && cur.len() + comp.len() > max {
            buckets.push(std::mem::take(&mut cur));
        }
        cur.extend(comp);
        if cur.len() >= max {
            buckets.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        buckets.push(cur);
    }
    buckets
}

/// The subset of `edges` internal to `bucket`, remapped to the bucket's local atom indices.
pub(crate) fn sub_edges(edges: &[Edge], bucket: &[usize]) -> Vec<Edge> {
    let local: std::collections::HashMap<usize, usize> =
        bucket.iter().enumerate().map(|(l, &g)| (g, l)).collect();
    edges
        .iter()
        .filter_map(|e| match (local.get(&e.from), local.get(&e.to)) {
            (Some(&f), Some(&t)) => Some(Edge {
                from: f,
                to: t,
                kind: e.kind,
            }),
            _ => None,
        })
        .collect()
}

/// Rewrite a per-bucket sub-plan's labels from bucket-local back to global atom labels, and prefix
/// each slug with the bucket index to keep slugs unique across buckets (incremental `match_layers`
/// re-attaches stable identity afterward, so the prefix doesn't churn PRs).
pub(crate) fn remap_and_prefix(plan: &mut SplitPlan, bucket: &[usize], bucket_idx: usize) {
    for layer in &mut plan.layers {
        layer.atoms = layer
            .atoms
            .iter()
            .filter_map(|l| label_index(l))
            .filter_map(|li| bucket.get(li))
            .map(|&g| atom_label(g))
            .collect();
        layer.slug = format!("b{bucket_idx}-{}", layer.slug);
    }
}

/// Slugs of plan layers that differ from the previous expansion (new layer, or same slug but a
/// changed atom set) — used to highlight what moved in a re-split's review.
pub(crate) fn changed_layers(plan: &SplitPlan, existing: &[PersistedLayer], atoms: &[Atom]) -> Vec<String> {
    let old: std::collections::HashMap<&str, std::collections::HashSet<&str>> = existing
        .iter()
        .map(|l| {
            (
                l.slug.as_str(),
                l.atom_hashes.iter().map(|s| s.as_str()).collect(),
            )
        })
        .collect();
    plan.layers
        .iter()
        .filter(|l| {
            let new_set: std::collections::HashSet<&str> = l
                .atoms
                .iter()
                .filter_map(|lab| label_index(lab))
                .filter_map(|i| atoms.get(i))
                .map(|a| a.id.as_str())
                .collect();
            match old.get(l.slug.as_str()) {
                Some(os) => &new_set != os,
                None => true,
            }
        })
        .map(|l| l.slug.clone())
        .collect()
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}
