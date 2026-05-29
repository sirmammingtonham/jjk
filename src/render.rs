//! Rendering for `jjk ls` (stack diagram) and `jjk status`.

use crate::engine::stack::Stack;
use crate::engine::Report;

/// Render the stack top → bottom (top printed first, trunk last), git-spice style.
///
/// ```text
///   feat-c   #1203 (open)
/// ◉ feat-b   #1202 (open)   ← current
///   feat-a   #1201 (merged)
///   main  (trunk)
/// ```
pub fn render_ls(stack: &Stack) -> String {
    let mut out = String::new();
    let current = stack.current.as_deref();

    for b in stack.branches.iter().rev() {
        let is_cur = Some(b.name.as_str()) == current;
        let marker = if is_cur { "◉" } else { " " };

        let pr = match b.pr {
            Some(n) => format!("#{n}"),
            None => "(no PR)".to_string(),
        };
        let count = b.commit_count();
        let commits = if count == 1 {
            "1 commit".to_string()
        } else {
            format!("{count} commits")
        };
        let conflict = if b.has_conflict() { "  ⚠ conflict" } else { "" };
        let cur = if is_cur { "   ← current" } else { "" };
        let tracked = if b.tracked { "" } else { "  (untracked)" };

        out.push_str(&format!(
            "{marker} {name}   {pr}  {commits}{tracked}{conflict}{cur}\n",
            name = b.name,
        ));
    }

    // trunk line
    let trunk_cur = current.is_none();
    let marker = if trunk_cur { "◉" } else { " " };
    let cur = if trunk_cur { "   ← current" } else { "" };
    out.push_str(&format!(
        "{marker} {} (trunk){cur}\n",
        stack.trunk_name
    ));
    out
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

/// Print a [`Report`]'s notes and conflict summary to stdout/stderr.
pub fn print_report(report: &Report) {
    for n in &report.notes {
        println!("{n}");
    }
    if !report.conflicts.is_empty() {
        eprintln!();
        eprintln!("CONFLICT: {} change(s) need resolution:", report.conflicts.len());
        for c in &report.conflicts {
            eprintln!("  {c}");
        }
        eprintln!("Edit the conflicted files, then re-run to re-snapshot; resolution propagates.");
        eprintln!("Run `jjk status` to see details.");
    }
}
