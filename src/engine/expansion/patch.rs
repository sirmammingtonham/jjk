//! Patch-subset applier + per-file materialization used to rebuild each layer's tree.

use crate::error::{JjkError, Result};
use crate::model::{DiffLine, FileChangeKind, FileDiff, Hunk};

// ---- patch-subset applier ----------------------------------------------------------------------

/// Apply a subset of a file's hunks onto `base` (the original/trunk content) and return the
/// resulting file content. Within one squashed diff hunks are non-overlapping ranges of `base`, so
/// any subset applies cleanly. Returns `Err` if a hunk's context/removed lines don't match `base`
/// (which would indicate the hunks weren't computed against this `base`); callers route such files
/// to the remainder commit so nothing is ever lost.
///
/// Added files pass `base = ""`. Final-newline handling is best-effort (assume newline-terminated);
/// any mismatch is caught by the `trees_equal` gate and absorbed by the remainder.
pub(crate) fn apply_hunks(base: &str, hunks: &[&Hunk]) -> Result<String> {
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

// ---- reconstruction support -------------------------------------------------------------------

/// What to do with one file when materializing a layer's cumulative content.
pub(crate) enum Materialized {
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
pub(crate) fn materialize_file(base: Option<&str>, f: &FileDiff, included: &[usize]) -> Result<Materialized> {
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
