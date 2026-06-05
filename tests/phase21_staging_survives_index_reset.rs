//! Regression: a plain `jjk commit` must commit only the git-staged files even when jj rewrites
//! the colocated git index during its pre-command snapshots.
//!
//! Some jj builds reset `.git/index` to match the working-copy commit on every snapshot (so a
//! colocated `git add` is silently unstaged the moment any jj command snapshots). jjk's dispatcher
//! runs `reconcile_git_head` and `checkpoint` — both of which snapshot — *before* it commits. If it
//! reads the staging after those, it sees an empty index and falls back to committing the whole
//! working copy. The fix reads the staged paths up front, before any jj invocation.
//!
//! We don't depend on the local jj build's index behavior: a `jj` shim on PATH emulates the
//! index-resetting build by unstaging after every snapshot (every non `--ignore-working-copy` call).

use std::path::Path;
use std::process::Command;

/// Write an executable `jj` shim into `dir` that runs real jj, then resets the colocated git index
/// (unstaging) after any working-copy snapshot — emulating jj builds that sync `.git/index` to `@`.
fn write_jj_shim(dir: &Path, real_jj: &str) {
    let shim = dir.join("jj");
    let script = format!(
        r#"#!/bin/sh
REAL_JJ="{real_jj}"
# Find the repo root passed as `-R <root>` (jjk always passes it for repo-scoped commands).
root=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-R" ]; then root="$a"; fi
  prev="$a"
done
"$REAL_JJ" "$@"
rc=$?
# Reads pass --ignore-working-copy and don't snapshot; everything else snapshots, so emulate a jj
# build that rewrites the git index to match @ by unstaging (reset index to HEAD, keep worktree).
case " $* " in
  *" --ignore-working-copy "*) : ;;
  *) [ -n "$root" ] && git -C "$root" reset -q >/dev/null 2>&1 ;;
esac
exit $rc
"#
    );
    std::fs::write(&shim, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Run the built `jjk` binary in `repo`, with the shim dir prepended to PATH so its `jj` is used.
fn jjk(repo: &Path, shim_dir: &Path, args: &[&str]) -> std::process::Output {
    let real_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{}", shim_dir.display(), real_path);
    Command::new(env!("CARGO_BIN_EXE_jjk"))
        .current_dir(repo)
        .args(args)
        .env("PATH", path)
        .output()
        .expect("failed to run jjk")
}

fn git(repo: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

/// Real jj, used only for assertions (read-only, with --ignore-working-copy so it doesn't snapshot).
fn jj_names(repo: &Path, revset: &str) -> String {
    let out = Command::new("jj")
        .current_dir(repo)
        .args([
            "diff",
            "--ignore-working-copy",
            "--name-only",
            "--color=never",
            "-r",
            revset,
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn plain_commit_honors_staging_even_when_jj_resets_the_index() {
    // jj identity for the subprocess (mirrors tests/common::init_identity).
    let cfgdir = std::env::temp_dir().join("jjk-test-jjconfig-p21");
    std::fs::create_dir_all(&cfgdir).unwrap();
    let cfg = cfgdir.join("config.toml");
    std::fs::write(&cfg, "[user]\nname = \"jjk test\"\nemail = \"jjk-test@example.com\"\n").unwrap();
    std::env::set_var("JJ_CONFIG", &cfg);

    let real_jj = "/opt/homebrew/bin/jj"; // resolved below if missing
    let real_jj = if Path::new(real_jj).exists() {
        real_jj.to_string()
    } else {
        String::from_utf8_lossy(
            &Command::new("sh").args(["-c", "command -v jj"]).output().unwrap().stdout,
        )
        .trim()
        .to_string()
    };

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().to_path_buf();
    let shim = tempfile::tempdir().unwrap();
    write_jj_shim(shim.path(), &real_jj);

    // Build a stack-tracked branch with a baseline commit (so HEAD/@- are well-defined).
    let out = jjk(&repo, shim.path(), &["repo", "init", "--trunk", "main"]);
    assert!(out.status.success(), "repo init: {}", String::from_utf8_lossy(&out.stderr));
    let out = jjk(&repo, shim.path(), &["branch", "create", "feat"]);
    assert!(out.status.success(), "branch create: {}", String::from_utf8_lossy(&out.stderr));

    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();

    // Stage only a.txt (as an editor would). The shim will unstage it on jjk's first snapshot.
    git(&repo, &["add", "a.txt"]);

    let out = jjk(&repo, shim.path(), &["commit", "-n", "-m", "only a"]);
    assert!(out.status.success(), "commit: {}", String::from_utf8_lossy(&out.stderr));

    // The commit must contain a.txt only; b.txt must remain an uncommitted working-copy change.
    let committed = jj_names(&repo, "@-");
    let remaining = jj_names(&repo, "@");
    assert!(committed.contains("a.txt"), "committed a.txt; got: {committed:?}");
    assert!(
        !committed.contains("b.txt"),
        "must NOT commit unstaged b.txt (index reset must not widen the scope); got: {committed:?}"
    );
    assert!(remaining.contains("b.txt"), "b.txt stays uncommitted; got: {remaining:?}");
}
