//! Parser for `jj diff --git` (git-format unified diff) → structured per-file hunks.

use crate::model::{DiffLine, FileChangeKind, FileDiff, Hunk};

pub(crate) fn parse_git_diff(text: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let (mut path, mut old_path) = parse_diff_git_paths(rest);
        let mut change = FileChangeKind::Modified;
        let mut is_binary = false;

        // Metadata lines (index/mode/---/+++/rename/binary) up to the first hunk or next file.
        while let Some(peek) = lines.peek() {
            if peek.starts_with("@@") || peek.starts_with("diff --git ") {
                break;
            }
            let meta = lines.next().unwrap();
            if meta.starts_with("new file") {
                change = FileChangeKind::Added;
            } else if meta.starts_with("deleted file") {
                change = FileChangeKind::Deleted;
            } else if let Some(src) = meta.strip_prefix("rename from ") {
                old_path = Some(src.to_string());
                change = FileChangeKind::Renamed;
            } else if let Some(dst) = meta.strip_prefix("rename to ") {
                path = dst.to_string();
            } else if meta.starts_with("Binary files") || meta.starts_with("GIT binary patch") {
                is_binary = true;
            }
        }

        let mut hunks = Vec::new();
        while let Some(peek) = lines.peek() {
            if !peek.starts_with("@@") {
                break;
            }
            let header = lines.next().unwrap();
            let (old_start, old_len, new_start, new_len) = parse_hunk_header(header);
            let mut hlines = Vec::new();
            while let Some(peek2) = lines.peek() {
                if peek2.starts_with("@@") || peek2.starts_with("diff --git ") {
                    break;
                }
                let l = lines.next().unwrap();
                if l.starts_with('\\') {
                    continue; // "\ No newline at end of file"
                }
                match l.as_bytes().first() {
                    Some(b'+') => hlines.push(DiffLine::Added(l[1..].to_string())),
                    Some(b'-') => hlines.push(DiffLine::Removed(l[1..].to_string())),
                    Some(b' ') => hlines.push(DiffLine::Context(l[1..].to_string())),
                    _ => hlines.push(DiffLine::Context(l.to_string())),
                }
            }
            hunks.push(Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
                lines: hlines,
            });
        }
        if is_binary {
            change = FileChangeKind::Binary;
        }
        files.push(FileDiff {
            path,
            old_path,
            change,
            hunks,
        });
    }
    files
}

/// Parse the `a/<old> b/<new>` tail of a `diff --git` line. Renames are corrected later from the
/// `rename from`/`rename to` lines, so for the common modify case (a == b) `old_path` is `None`.
fn parse_diff_git_paths(rest: &str) -> (String, Option<String>) {
    if let Some(idx) = rest.find(" b/") {
        let a = rest[..idx].trim_start_matches("a/").to_string();
        let b = rest[idx + 1..].trim_start_matches("b/").to_string();
        let old = (a != b).then_some(a);
        return (b, old);
    }
    (rest.trim_start_matches("a/").to_string(), None)
}

/// Parse `@@ -old_start,old_len +new_start,new_len @@ ...` (lengths default to 1 when omitted).
fn parse_hunk_header(h: &str) -> (u32, u32, u32, u32) {
    let core = h.split("@@").nth(1).unwrap_or("").trim();
    let mut parts = core.split_whitespace();
    let (os, ol) = parse_hunk_range(parts.next().unwrap_or("-0,0").trim_start_matches('-'));
    let (ns, nl) = parse_hunk_range(parts.next().unwrap_or("+0,0").trim_start_matches('+'));
    (os, ol, ns, nl)
}

fn parse_hunk_range(s: &str) -> (u32, u32) {
    let mut it = s.split(',');
    let start = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let len = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    (start, len)
}

#[cfg(test)]
mod diff_parse_tests {
    use super::*;

    #[test]
    fn parses_modify_add_delete_and_binary() {
        let text = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 keep
-old
+new
+extra
 tail
diff --git a/new.txt b/new.txt
new file mode 100644
index 000..333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 444..000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/logo.png b/logo.png
index 555..666 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = parse_git_diff(text);
        assert_eq!(files.len(), 4);

        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].change, FileChangeKind::Modified);
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (1, 3, 1, 4));
        assert_eq!(
            h.lines,
            vec![
                DiffLine::Context("keep".into()),
                DiffLine::Removed("old".into()),
                DiffLine::Added("new".into()),
                DiffLine::Added("extra".into()),
                DiffLine::Context("tail".into()),
            ]
        );

        assert_eq!(files[1].change, FileChangeKind::Added);
        assert_eq!(files[1].hunks[0].lines.len(), 2);
        assert_eq!(files[2].change, FileChangeKind::Deleted);
        assert_eq!(files[3].change, FileChangeKind::Binary);
        assert!(files[3].hunks.is_empty());
    }

    #[test]
    fn parses_rename() {
        let text = "\
diff --git a/old/path.rs b/new/path.rs
similarity index 90%
rename from old/path.rs
rename to new/path.rs
index 111..222 100644
--- a/old/path.rs
+++ b/new/path.rs
@@ -1 +1 @@
-a
+b
";
        let files = parse_git_diff(text);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].change, FileChangeKind::Renamed);
        assert_eq!(files[0].path, "new/path.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("old/path.rs"));
    }

    #[test]
    fn single_line_hunk_header_defaults_len_to_one() {
        assert_eq!(parse_hunk_header("@@ -5 +6 @@"), (5, 1, 6, 1));
        assert_eq!(parse_hunk_header("@@ -1,0 +1,3 @@ fn foo()"), (1, 0, 1, 3));
    }
}

