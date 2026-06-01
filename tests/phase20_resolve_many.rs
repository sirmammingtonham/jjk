//! Guards the concurrent batch-read primitive (`Vcs::resolve_many`): the independent `jj` reads
//! now run in parallel, so correctness depends on results being mapped back **by input position**,
//! not by completion order. This builds a tiny real repo and asserts the i-th result is the
//! resolution of the i-th revset — the same position-mapping guarantee `derive_stack` /
//! `upstack_first_commits` / `worktree_list` rely on.

use jjk::engine::Engine;
use std::sync::Once;
use tempfile::TempDir;

static IDENTITY: Once = Once::new();

fn init_identity() {
    IDENTITY.call_once(|| {
        let dir = std::env::temp_dir().join("jjk-test-jjconfig");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            "[user]\nname = \"jjk test\"\nemail = \"jjk-test@example.com\"\n",
        )
        .unwrap();
        std::env::set_var("JJ_CONFIG", &cfg);
    });
}

async fn setup() -> (TempDir, Engine) {
    init_identity();
    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into()))
        .await
        .unwrap();
    let mut e = Engine::open(tmp.path()).unwrap();
    // A branch with a couple of commits so `@`, `@-`, and `root()` are all distinct commits.
    e.branch_create("feat", /*tracked=*/ true).await.unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    e.commit("first").await.unwrap();
    std::fs::write(tmp.path().join("b.txt"), "b").unwrap();
    e.commit("second").await.unwrap();
    (tmp, e)
}

#[tokio::test]
async fn resolve_many_preserves_input_order() {
    let (_tmp, e) = setup().await;
    let vcs = e.vcs();

    let revsets = ["@", "@-", "root()"];
    // Each batched result must equal resolving that same revset on its own, in the same slot.
    let batched = vcs.resolve_many(&revsets).await.unwrap();
    assert_eq!(batched.len(), revsets.len());

    for (i, r) in revsets.iter().enumerate() {
        let solo = vcs.resolve(r).await.unwrap();
        let batched_ids: Vec<_> = batched[i].iter().map(|c| c.change_id.clone()).collect();
        let solo_ids: Vec<_> = solo.iter().map(|c| c.change_id.clone()).collect();
        assert_eq!(
            batched_ids, solo_ids,
            "resolve_many slot {i} ({r:?}) did not match a solo resolve — ordering is mis-mapped"
        );
    }

    // Sanity: `@`, `@-`, `root()` resolved to three different commits, so a mismapping would show.
    let at = &batched[0][0].change_id;
    let parent = &batched[1][0].change_id;
    let root = &batched[2][0].change_id;
    assert_ne!(at, parent);
    assert_ne!(parent, root);
    assert_ne!(at, root);
}

#[tokio::test]
async fn resolve_many_empty_input_is_empty() {
    let (_tmp, e) = setup().await;
    let out = e.vcs().resolve_many(&[]).await.unwrap();
    assert!(out.is_empty());
}
