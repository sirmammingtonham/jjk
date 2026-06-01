//! Rendering for `jjk ls`/`ll` (git-spice-style stack tree) and `jjk status`.

use crate::engine::stack::{Branch, Stack};
use crate::engine::{Report, WorktreeRow};
use std::io::IsTerminal;

// ---- minimal ANSI styling (disabled when stdout isn't a terminal) ----

fn colors_on() -> bool {
    std::io::stdout().is_terminal()
}

fn paint(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

const BOLD: &str = "1";
const DIM: &str = "90"; // bright black / gray
const GREEN: &str = "32";
const YELLOW: &str = "33";
const RED: &str = "31";

/// `jjk ls` — the stack tree (branches only).
pub fn render_ls(stack: &Stack) -> String {
    render_tree(stack, false, colors_on())
}

/// `jjk ll` — the stack tree with each branch's commits.
pub fn render_ll(stack: &Stack) -> String {
    render_tree(stack, true, colors_on())
}

// Heavy box-drawing glyphs, matching git-spice's fliptree renderer.
const G_DOWN_RIGHT: &str = "┏";
const G_HORIZONTAL: &str = "━";
const G_HORIZONTAL_UP: &str = "┻";
const G_VERTICAL: &str = "┃";

/// git-spice-style tree (heavy inline pipes), top → bottom, trunk last. For a linear stack each
/// branch is one level deeper than the one below; its single `┏` corner drops a `┃` pipe (as long
/// as its commit list) into the `┻` of the branch beneath it — one bend per branch, no trailing
/// joint.
fn render_tree(stack: &Stack, show_commits: bool, color: bool) -> String {
    let n = stack.branches.len();
    let mut out = String::new();
    let current = stack.current.as_deref();

    for i in (0..n).rev() {
        let b = &stack.branches[i];
        let is_cur = Some(b.name.as_str()) == current;
        let has_child = i + 1 < n; // a branch is stacked directly above this one
        let indent = "  ".repeat(i);

        // Title line: indent + dim("┏━[┻]") + box + " " + name + suffix [+ " ◀"]
        let joint = format!(
            "{G_DOWN_RIGHT}{G_HORIZONTAL}{}",
            if has_child { G_HORIZONTAL_UP } else { "" }
        );
        let box_str = if is_cur {
            paint(color, GREEN, "■")
        } else {
            "□".to_string()
        };
        let mut line = String::new();
        line.push_str(&indent);
        line.push_str(&paint(color, DIM, &joint));
        line.push_str(&box_str);
        line.push(' ');
        line.push_str(&paint(color, BOLD, &b.name));
        line.push_str(&branch_suffix(b, color));
        if is_cur {
            line.push_str(&paint(color, GREEN, " ◀"));
        }
        out.push_str(&line);
        out.push('\n');

        if show_commits {
            // Body lines continue the pipe: indent + dim("┃") + spaces to align under the name.
            let marker_width = if has_child { 1 } else { 0 } + 2; // width of "[┻]□ "
            let prefix = format!(
                "{indent}{} {}",
                paint(color, DIM, G_VERTICAL),
                " ".repeat(marker_width)
            );
            for c in b.commits.iter().rev() {
                let sha: String = c.commit_id.0.chars().take(10).collect();
                let subject = if c.subject().is_empty() {
                    "(no description)".to_string()
                } else {
                    c.subject().to_string()
                };
                out.push_str(&format!(
                    "{prefix}{} {} {}\n",
                    paint(color, YELLOW, &sha),
                    subject,
                    paint(color, DIM, &format!("({})", c.time_ago)),
                ));
            }
        }
    }

    // Trunk line (root): name only, no box/pipe, with the current marker if the working copy is on it.
    let mut tline = paint(color, &format!("{BOLD};{GREEN}"), &stack.trunk_name);
    if current.is_none() {
        tline.push_str(&paint(color, GREEN, " ◀"));
    }
    out.push_str(&tline);
    out.push('\n');
    out
}

/// Suffix after a branch name: PR number, or untracked/no-PR hint, plus a conflict flag.
fn branch_suffix(b: &Branch, color: bool) -> String {
    let mut s = String::new();
    if let Some(pr) = b.pr {
        s.push_str(&paint(color, DIM, &format!(" (#{pr})")));
    } else if !b.tracked {
        s.push_str(&paint(color, DIM, " (untracked)"));
    }
    if b.has_conflict() {
        s.push_str(&paint(color, RED, " ⚠ conflict"));
    }
    s
}

/// One-line stack-position hint for `status`.
pub fn render_position(stack: &Stack) -> String {
    match &stack.current {
        Some(cur) => {
            let idx = stack.index_of(cur).map(|i| i + 1).unwrap_or(0);
            let total = stack.branches.len();
            format!("on branch '{cur}' ({idx}/{total} in stack)")
        }
        None => format!("on trunk '{}'", stack.trunk_name),
    }
}

/// Render `jjk worktree list`.
pub fn render_worktrees(rows: &[WorktreeRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let branch = r.current_branch.as_deref().unwrap_or("(trunk)");
        let stale = if r.is_stale { "  ⚠ stale" } else { "" };
        out.push_str(&format!(
            "{name}   on {branch}   @{wc}{stale}\n",
            name = r.name,
            wc = r.working_copy.short(),
        ));
    }
    out
}

/// Print a [`Report`]'s notes and conflict summary to stdout/stderr.
pub fn print_report(report: &Report) {
    // Notes were already streamed as they were produced (see `Report::note`); only the conflict
    // summary is rendered here, at the end.
    if !report.conflicts.is_empty() {
        eprintln!();
        eprintln!(
            "CONFLICT: {} change(s) need resolution:",
            report.conflicts.len()
        );
        for c in &report.conflicts {
            eprintln!("  {c}");
        }
        eprintln!("Run `jjk resolve` to fix them; the resolution propagates upstack.");
    }
}
