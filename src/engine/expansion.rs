//! Domain Expansion — persisted mode state (the monolith↔layer-stack mapping).
//!
//! Stored in `.jj/jjk/expansion.json`, a sidecar like `undo.json`/`resolve.json` (kept out of
//! `state.toml` because it's mode-specific and richer than the branch↔PR map). Its **presence**
//! means the current repo has a monolith branch being auto-stacked; its absence means normal jjk.
//! PR numbers / nav-comment ids still live in `state.toml`'s `BranchEntry`, keyed by each layer
//! bookmark, so the existing submit/sync machinery reuses unchanged.

use crate::error::{JjkError, Result};
use crate::llm::{AtomGist, EdgeHint, LayerSpec, SplitInput, SplitPlan};
use crate::model::{ChangeId, DiffLine, FileChangeKind, FileDiff, Hunk};
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

// ======================================================================================
// The deterministic "sketch": atoms, dependency hints, and the patch-subset applier.
//
// These produce the cheap, high-signal representation the LLM reasons over (atoms + hints) and the
// mechanical means to rebuild a layer's file content (the applier). The LLM decides the boundaries;
// nothing here pre-decides the partition.
// ======================================================================================

/// One atomic change unit fed to the splitter. A modified text file contributes **one atom per
/// hunk** (sub-file granularity); add/delete/rename/binary files contribute a single whole-file
/// atom (they can't be sub-split). Carries cheap, advisory `defs`/`uses` for dependency hints and a
/// one-line `gist` — never raw code.
#[derive(Debug, Clone)]
pub struct Atom {
    /// Stable content hash (hex) — identity for incremental re-split matching.
    pub id: String,
    pub path: String,
    pub change: FileChangeKind,
    /// Index into the `Vec<FileDiff>` this atom came from.
    pub file_idx: usize,
    /// Hunk indices within that file this atom covers (one for a per-hunk atom; all for whole-file).
    pub hunks: Vec<usize>,
    /// Identifiers this change appears to define (heuristic).
    pub defs: Vec<String>,
    /// Identifiers this change references (heuristic).
    pub uses: Vec<String>,
    pub added: u32,
    pub removed: u32,
    /// One-line human summary for the LLM payload.
    pub gist: String,
}

/// A dependency/affinity hint between two atoms (by index). Advisory input to the LLM, not a
/// constraint — the LLM may group however it judges best.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// `from` references a symbol `to` defines (suggests `to` in `from`'s layer or below).
    DefUse,
    /// Same file (affinity).
    SameFile,
}

/// Extract atoms from a structured diff (`Vcs::diff_hunks` output).
pub fn extract_atoms(files: &[FileDiff]) -> Vec<Atom> {
    let mut atoms = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let per_hunk = matches!(f.change, FileChangeKind::Modified) && f.hunks.len() > 1;
        if per_hunk {
            for (hi, h) in f.hunks.iter().enumerate() {
                atoms.push(atom_for(f, fi, vec![hi], &[h]));
            }
        } else {
            let idxs: Vec<usize> = (0..f.hunks.len()).collect();
            let hrefs: Vec<&Hunk> = f.hunks.iter().collect();
            atoms.push(atom_for(f, fi, idxs, &hrefs));
        }
    }
    atoms
}

fn atom_for(f: &FileDiff, file_idx: usize, hunk_idxs: Vec<usize>, hunks: &[&Hunk]) -> Atom {
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut defs: Vec<String> = Vec::new();
    let mut uses: Vec<String> = Vec::new();
    for h in hunks {
        for line in &h.lines {
            match line {
                DiffLine::Added(s) => {
                    added += 1;
                    collect_defs(s, &mut defs);
                    collect_idents(s, &mut uses);
                }
                DiffLine::Removed(s) => {
                    removed += 1;
                    collect_idents(s, &mut uses);
                }
                DiffLine::Context(s) => collect_idents(s, &mut uses),
            }
        }
    }
    defs.sort();
    defs.dedup();
    uses.sort();
    uses.dedup();
    // A symbol an atom defines isn't a "use" of an external symbol.
    uses.retain(|u| !defs.contains(u));

    let id = atom_hash(&f.path, hunks);
    let kind = change_word(f.change);
    let head = defs
        .first()
        .cloned()
        .unwrap_or_else(|| f.path.rsplit('/').next().unwrap_or(&f.path).to_string());
    let gist = format!("{kind} {} (+{added}/-{removed}) {head}", f.path);
    Atom {
        id,
        path: f.path.clone(),
        change: f.change,
        file_idx,
        hunks: hunk_idxs,
        defs,
        uses,
        added,
        removed,
        gist,
    }
}

pub(crate) fn change_word(c: FileChangeKind) -> &'static str {
    match c {
        FileChangeKind::Added => "add",
        FileChangeKind::Modified => "modify",
        FileChangeKind::Deleted => "delete",
        FileChangeKind::Renamed => "rename",
        FileChangeKind::Binary => "binary",
    }
}

/// Stable content hash of an atom: FNV-1a over `path` + the +/- line bodies (ignoring line numbers,
/// so an atom keeps its identity when unrelated edits shift it). Hex string.
fn atom_hash(path: &str, hunks: &[&Hunk]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(path.as_bytes());
    feed(b"\0");
    for hk in hunks {
        for line in &hk.lines {
            let (tag, s) = match line {
                DiffLine::Added(s) => (b'+', s),
                DiffLine::Removed(s) => (b'-', s),
                DiffLine::Context(s) => (b' ', s),
            };
            feed(&[tag]);
            feed(s.as_bytes());
            feed(b"\n");
        }
    }
    format!("{h:016x}")
}

/// Build advisory dependency/affinity edges over the atoms. `O(n²)`; fine for review-sized diffs.
pub fn build_edges(atoms: &[Atom]) -> Vec<Edge> {
    let mut edges = Vec::new();
    for (ai, a) in atoms.iter().enumerate() {
        for (bi, b) in atoms.iter().enumerate() {
            if ai == bi {
                continue;
            }
            // `a` uses a symbol `b` defines ⇒ a depends on b.
            if b.defs.iter().any(|d| a.uses.contains(d)) {
                edges.push(Edge {
                    from: ai,
                    to: bi,
                    kind: EdgeKind::DefUse,
                });
            }
        }
    }
    for ai in 0..atoms.len() {
        for bi in (ai + 1)..atoms.len() {
            if atoms[ai].path == atoms[bi].path {
                edges.push(Edge {
                    from: ai,
                    to: bi,
                    kind: EdgeKind::SameFile,
                });
            }
        }
    }
    edges
}

/// Keywords whose following identifier is treated as a definition (cross-language, heuristic).
const DEF_KEYWORDS: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "trait",
    "type",
    "const",
    "static",
    "mod",
    "class",
    "def",
    "function",
    "interface",
    "impl",
];

/// Identifiers worth ignoring as "uses" (common keywords/noise); kept short on purpose.
fn is_noise_ident(s: &str) -> bool {
    s.len() <= 2
        || matches!(
            s,
            "let" | "mut" | "pub" | "use" | "the" | "for" | "and" | "not" | "return" | "if" | "else"
        )
}

/// Push identifiers that follow a def keyword in `line` into `out`.
fn collect_defs(line: &str, out: &mut Vec<String>) {
    let toks = tokenize_idents(line);
    for w in toks.windows(2) {
        if DEF_KEYWORDS.contains(&w[0].as_str()) {
            out.push(w[1].clone());
        }
    }
}

/// Push all non-noise identifiers in `line` into `out`.
fn collect_idents(line: &str, out: &mut Vec<String>) {
    for t in tokenize_idents(line) {
        if !is_noise_ident(&t) && !DEF_KEYWORDS.contains(&t.as_str()) {
            out.push(t);
        }
    }
}

/// Split a line into identifier tokens (`[A-Za-z_][A-Za-z0-9_]*`).
fn tokenize_idents(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in line.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            if cur.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if !cur.is_empty() && cur.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
        out.push(cur);
    }
    out
}

// ---- patch-subset applier ----------------------------------------------------------------------

/// Apply a subset of a file's hunks onto `base` (the original/trunk content) and return the
/// resulting file content. Within one squashed diff hunks are non-overlapping ranges of `base`, so
/// any subset applies cleanly. Returns `Err` if a hunk's context/removed lines don't match `base`
/// (which would indicate the hunks weren't computed against this `base`); callers route such files
/// to the remainder commit so nothing is ever lost.
///
/// Added files pass `base = ""`. Final-newline handling is best-effort (assume newline-terminated);
/// any mismatch is caught by the `trees_equal` gate and absorbed by the remainder.
pub fn apply_hunks(base: &str, hunks: &[&Hunk]) -> Result<String> {
    let (base_lines, _trailing) = split_lines(base);
    // Apply in ascending original-line order.
    let mut order: Vec<&&Hunk> = hunks.iter().collect();
    order.sort_by_key(|h| h.old_start);

    let mut out: Vec<&str> = Vec::new();
    let mut cursor: usize = 0; // 0-based index into base_lines
    for h in order {
        let start0 = if h.old_len == 0 {
            h.old_start as usize
        } else {
            (h.old_start as usize).saturating_sub(1)
        };
        if start0 < cursor || start0 > base_lines.len() {
            return Err(JjkError::Msg(format!(
                "hunk at line {} does not apply cleanly (overlap or out of range)",
                h.old_start
            ))
            .into());
        }
        // Copy untouched base lines before the hunk.
        out.extend_from_slice(&base_lines[cursor..start0]);

        // Validate + consume the base lines this hunk covers, and emit the new content.
        let mut consumed = 0usize;
        for line in &h.lines {
            match line {
                DiffLine::Context(s) => {
                    let bi = start0 + consumed;
                    if base_lines.get(bi).copied() != Some(s.as_str()) {
                        return Err(JjkError::Msg(
                            "hunk context does not match base content".into(),
                        )
                        .into());
                    }
                    out.push(s);
                    consumed += 1;
                }
                DiffLine::Removed(s) => {
                    let bi = start0 + consumed;
                    if base_lines.get(bi).copied() != Some(s.as_str()) {
                        return Err(JjkError::Msg(
                            "hunk removed line does not match base content".into(),
                        )
                        .into());
                    }
                    consumed += 1;
                }
                DiffLine::Added(s) => out.push(s),
            }
        }
        if consumed != h.old_len as usize {
            return Err(JjkError::Msg(format!(
                "hunk length mismatch: header says {} old lines, body consumed {consumed}",
                h.old_len
            ))
            .into());
        }
        cursor = start0 + consumed;
    }
    out.extend_from_slice(&base_lines[cursor..]);

    let mut result = out.join("\n");
    if !out.is_empty() {
        result.push('\n'); // assume newline-terminated; remainder catches the rare exception
    }
    Ok(result)
}

/// Split into lines (without terminators) plus whether the content ended with a newline.
fn split_lines(s: &str) -> (Vec<&str>, bool) {
    if s.is_empty() {
        return (Vec::new(), false);
    }
    let trailing = s.ends_with('\n');
    let body = if trailing { &s[..s.len() - 1] } else { s };
    (body.split('\n').collect(), trailing)
}

// ---- LLM payload + plan resolution -------------------------------------------------------------

/// Everything `compute_split` produces: the source diff + atoms and the (completeness-repaired)
/// plan, plus the trunk/monolith anchors needed for reconstruction. Atom labels are `a{index}`.
pub struct ResolvedSplit {
    pub atoms: Vec<Atom>,
    pub files: Vec<FileDiff>,
    pub trunk: ChangeId,
    pub monolith_tip: ChangeId,
    pub plan: SplitPlan,
}

/// Atom label used in the LLM payload / plan: `a{index}`.
pub fn atom_label(index: usize) -> String {
    format!("a{index}")
}

/// Parse an `a{index}` label back to an atom index.
pub fn label_index(label: &str) -> Option<usize> {
    label.strip_prefix('a').and_then(|s| s.parse().ok())
}

/// Build the compressed, code-free LLM payload from the sketch. Full hunk text is stashed in
/// `details` (served on demand via the `get_atom_detail` tool), never in the catalog.
pub fn build_split_input(
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
pub fn detail_text(files: &[FileDiff], atom: &Atom) -> String {
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
pub fn repair_completeness(plan: &mut SplitPlan, atom_count: usize) -> Vec<String> {
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

// ---- reconstruction support -------------------------------------------------------------------

/// What to do with one file when materializing a layer's cumulative content.
pub enum Materialized {
    /// Write this exact content at the file's (new) path.
    Write(String),
    /// Remove the file.
    Delete,
    /// Can't reconstruct from text (binary) — leave it; the remainder commit will capture it.
    Skip,
}

/// Compute a file's content at a layer, given its trunk `base` (or `None` for an added file) and the
/// cumulative set of included hunk indices for it. Add/modify → apply the included hunks; delete →
/// remove; binary → skip (the remainder safety net handles it).
pub fn materialize_file(base: Option<&str>, f: &FileDiff, included: &[usize]) -> Result<Materialized> {
    match f.change {
        FileChangeKind::Binary => Ok(Materialized::Skip),
        FileChangeKind::Deleted => Ok(Materialized::Delete),
        _ => {
            let hunks: Vec<&Hunk> = included.iter().filter_map(|&i| f.hunks.get(i)).collect();
            let content = apply_hunks(base.unwrap_or(""), &hunks)?;
            Ok(Materialized::Write(content))
        }
    }
}

/// Stabilize a freshly-computed plan's layer slugs by matching each layer to an existing persisted
/// layer with the greatest atom-hash (Jaccard) overlap, so a surviving layer keeps its
/// bookmark→PR→nav-comment across an incremental re-split. Mutates `plan` layer slugs in place.
pub fn match_layers(plan: &mut SplitPlan, existing: &[PersistedLayer], atoms: &[Atom]) {
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

/// Slugs of plan layers that differ from the previous expansion (new layer, or same slug but a
/// changed atom set) — used to highlight what moved in a re-split's review.
pub fn changed_layers(plan: &SplitPlan, existing: &[PersistedLayer], atoms: &[Atom]) -> Vec<String> {
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
