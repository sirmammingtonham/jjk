//! Domain-expansion change-atoms + dependency hints — the deterministic "sketch" the
//! splitter reasons over (gists + advisory edges, never raw code).

use crate::model::{DiffLine, FileChangeKind, FileDiff, Hunk};

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
