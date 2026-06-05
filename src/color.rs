//! Tiny ANSI styling for richer CLI output. Auto-disabled when stdout isn't a terminal or when
//! `NO_COLOR` is set, so piped/CI/test output stays plain. Computed once.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether to emit ANSI codes. Honors `NO_COLOR` and requires stdout to be a tty.
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("90", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
/// Bold + green, for headings / the trunk line.
pub fn bold_green(s: &str) -> String {
    paint("1;32", s)
}
