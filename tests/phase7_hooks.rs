//! git pre-commit hook integration on `jjk commit`.

mod common;

use common::{init_identity, write};
use jjk::engine::Engine;
use std::path::Path;
use tempfile::TempDir;

fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    (tmp, engine)
}

fn write_hook(root: &Path, script: &str) {
    let dir = root.join(".git/hooks");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("pre-commit");
    std::fs::write(&p, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn failing_hook_blocks_commit() {
    let (tmp, e) = setup();
    write_hook(tmp.path(), "#!/bin/sh\nexit 1\n");
    write(tmp.path(), "a.txt", "x\n");
    assert!(e.run_pre_commit(&jjk::vcs::CommitScope::All).is_err(), "a non-zero pre-commit hook blocks");
}

#[test]
fn no_hook_is_a_noop() {
    let (tmp, e) = setup();
    write(tmp.path(), "a.txt", "x\n");
    assert!(e.run_pre_commit(&jjk::vcs::CommitScope::All).is_ok(), "no hook -> nothing to verify");
}

#[test]
fn non_executable_hook_is_skipped() {
    let (tmp, e) = setup();
    // Write but DON'T chmod +x; git (and jjk) ignore non-executable hooks.
    std::fs::create_dir_all(tmp.path().join(".git/hooks")).unwrap();
    std::fs::write(tmp.path().join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
    assert!(e.run_pre_commit(&jjk::vcs::CommitScope::All).is_ok(), "non-executable hook is skipped");
}

#[test]
fn passing_hook_sees_staged_changes_and_commit_proceeds() {
    let (tmp, mut e) = setup();
    // The hook records the staged files (proving git semantics) then passes.
    write_hook(
        tmp.path(),
        "#!/bin/sh\ngit diff --cached --name-only > hook-staged.txt\nexit 0\n",
    );
    e.branch_create("feat-a", true).unwrap();
    write(tmp.path(), "a.txt", "hello\n");

    e.run_pre_commit(&jjk::vcs::CommitScope::All).unwrap();
    let staged = std::fs::read_to_string(tmp.path().join("hook-staged.txt")).unwrap();
    assert!(staged.contains("a.txt"), "hook saw staged file: {staged:?}");

    // Commit still works after the hook ran.
    e.commit("a1").unwrap();
    assert_eq!(e.derive_stack().unwrap().branch("feat-a").unwrap().commit_count(), 1);
}
