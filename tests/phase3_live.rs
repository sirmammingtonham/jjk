//! Phase 3 **live** smoke test against a real throwaway GitHub repo. Ignored by default; run with:
//!
//! ```sh
//! cargo test --test phase3_live -- --ignored --nocapture
//! ```
//!
//! Requires `gh auth` and write access to the smoke repo. Uses unique branch names per run and
//! cleans up (closes PRs, deletes remote branches) afterwards.

mod common;

use common::{init_identity, write};
use jjk::engine::Engine;
use jjk::forge::gh_cli::GhCli;
use jjk::forge::Forge;
use jjk::model::PrState;
use jjk::vcs::PushOpts;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_REPO: &str = "https://github.com/sirmammingtonham/jjk-smoke-tests.git";

fn slug() -> String {
    "sirmammingtonham/jjk-smoke-tests".to_string()
}

fn gh_token() -> String {
    let out = Command::new("gh").args(["auth", "token"]).output().unwrap();
    assert!(out.status.success(), "gh auth token failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Delete a remote branch ref (best effort).
fn delete_remote_branch(branch: &str) {
    let _ = Command::new("gh")
        .args([
            "api",
            "-X",
            "DELETE",
            &format!("repos/{}/git/refs/heads/{branch}", slug()),
        ])
        .output();
}

fn close_pr(number: u64) {
    let _ = Command::new("gh")
        .args(["pr", "close", &number.to_string(), "-R", &slug(), "--delete-branch"])
        .output();
}

#[tokio::test]
#[ignore = "live: hits GitHub; run explicitly with --ignored"]
async fn live_submit_three_pr_stack_and_reconcile() {
    init_identity();
    let repo_url = std::env::var("JJK_SMOKE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let token = gh_token();
    // Token-authenticated URL so `jj git push` works over https without prompting.
    let authed = repo_url.replace("https://", &format!("https://x-access-token:{token}@"));

    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let a = format!("smk-{ts}-a");
    let b = format!("smk-{ts}-b");
    let c = format!("smk-{ts}-c");

    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).unwrap();
    let mut engine = Engine::open(tmp.path()).unwrap();
    engine.vcs().add_remote("origin", &authed).unwrap();
    // Remote was added after open(), so refresh the forge with the now-known slug.
    engine.set_forge(Box::new(GhCli::new(Some(slug()))));

    // Run the body, capturing the result so cleanup always runs.
    let result = run_body(&mut engine, tmp.path(), &a, &b, &c).await;

    // ---- cleanup (best effort) ----
    let gh = GhCli::new(Some(slug()));
    for branch in [&a, &b, &c] {
        if let Ok(Some(pr)) = gh.get_pr(branch).await {
            close_pr(pr.number);
        }
        delete_remote_branch(branch);
    }

    result.expect("live smoke flow failed");
}

async fn run_body(
    engine: &mut Engine,
    root: &Path,
    a: &str,
    b: &str,
    c: &str,
) -> anyhow::Result<()> {
    // Ensure `main` exists (PR bases need it). Reuse the remote's main if a prior run created it,
    // else seed and push it.
    engine.vcs().fetch("origin", None).ok();
    let remote_main = engine
        .vcs()
        .resolve("main@origin")
        .ok()
        .and_then(|v| v.into_iter().next());
    match remote_main {
        Some(c) => {
            // Make sure a local `main` bookmark points at the fetched trunk.
            if !engine.vcs().bookmarks()?.iter().any(|b| b.name == "main") {
                let id = c.change_id.clone();
                engine
                    .vcs()
                    .transaction(&mut |tx| tx.set_bookmark("main", &id))?;
            }
        }
        None => {
            write(root, "README.md", "# jjk smoke\n");
            engine.vcs().transaction(&mut |tx| {
                let id = tx.finalize_working_copy("chore: seed trunk")?;
                tx.create_bookmark("main", &id)?;
                Ok(())
            })?;
            engine.vcs().push("origin", "main", PushOpts::default())?;
        }
    }

    // Build a 3-branch stack on top of main.
    engine.checkout("main")?;
    engine.branch_create(a, true)?;
    write(root, "a.txt", "alpha\n");
    engine.commit(&format!("{a}: alpha"))?;
    engine.branch_create(b, true)?;
    write(root, "b.txt", "bravo\n");
    engine.commit(&format!("{b}: bravo"))?;
    engine.branch_create(c, true)?;
    write(root, "c.txt", "charlie\n");
    engine.commit(&format!("{c}: charlie"))?;

    // Submit: 3 PRs, based bottom-up.
    let r1 = engine.submit(jjk::engine::SubmitScope::Stack).await?;
    eprintln!("submit #1:\n{}", r1.notes.join("\n"));

    let gh = GhCli::new(Some(slug()));
    let pa = gh.get_pr(a).await?.expect("PR for a");
    let pb = gh.get_pr(b).await?.expect("PR for b");
    let pc = gh.get_pr(c).await?.expect("PR for c");
    assert_eq!(pa.base, "main", "bottom PR based on trunk");
    assert_eq!(pb.base, a, "middle PR based on a");
    assert_eq!(pc.base, b, "top PR based on b");
    assert_eq!(pa.state, PrState::Open);

    // Edit + re-submit: idempotent (no new PRs, same numbers, bases intact).
    write(root, "a.txt", "alpha edited\n");
    engine.commit(&format!("{a}: edit"))?;
    let r2 = engine.submit(jjk::engine::SubmitScope::Stack).await?;
    eprintln!("submit #2:\n{}", r2.notes.join("\n"));

    let pa2 = gh.get_pr(a).await?.expect("PR for a still there");
    let pb2 = gh.get_pr(b).await?.expect("PR for b still there");
    assert_eq!(pa2.number, pa.number, "no duplicate PR for a");
    assert_eq!(pb2.number, pb.number, "no duplicate PR for b");
    assert_eq!(pb2.base, a, "base preserved on re-submit");

    Ok(())
}
