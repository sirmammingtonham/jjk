//! Phase 4 **live** smoke test: full stacked-PR sync against the throwaway GitHub repo, exercising
//! real `gh` merge-detection. Ignored by default:
//!
//! ```sh
//! cargo test --test phase4_live -- --ignored --nocapture
//! ```

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
const SLUG: &str = "sirmammingtonham/jjk-smoke-tests";

fn gh_token() -> String {
    let out = Command::new("gh").args(["auth", "token"]).output().unwrap();
    assert!(out.status.success(), "gh auth token failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn close_pr(number: u64) {
    let _ = Command::new("gh")
        .args(["pr", "close", &number.to_string(), "-R", SLUG, "--delete-branch"])
        .output();
}

fn delete_remote_branch(branch: &str) {
    let _ = Command::new("gh")
        .args([
            "api",
            "-X",
            "DELETE",
            &format!("repos/{SLUG}/git/refs/heads/{branch}"),
        ])
        .output();
}

#[tokio::test]
#[ignore = "live: hits GitHub; run explicitly with --ignored"]
async fn live_sync_after_squash_merge() {
    init_identity();
    let repo_url = std::env::var("JJK_SMOKE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let authed = repo_url.replace("https://", &format!("https://x-access-token:{}@", gh_token()));

    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let a = format!("sync-{ts}-a");
    let b = format!("sync-{ts}-b");

    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into())).unwrap();
    let mut engine = Engine::open(tmp.path()).unwrap();
    engine.vcs().add_remote("origin", &authed).unwrap();
    engine.set_forge(Box::new(GhCli::new(Some(SLUG.to_string()))));

    let result = run_body(&mut engine, tmp.path(), &a, &b).await;

    // cleanup (best effort)
    let gh = GhCli::new(Some(SLUG.to_string()));
    for branch in [&a, &b] {
        if let Ok(Some(pr)) = gh.get_pr(branch).await {
            close_pr(pr.number);
        }
        delete_remote_branch(branch);
    }
    result.expect("live sync flow failed");
}

async fn run_body(engine: &mut Engine, root: &Path, a: &str, b: &str) -> anyhow::Result<()> {
    // Ensure main exists (reuse if a prior run seeded it).
    engine.vcs().fetch("origin").ok();
    let has_main = engine
        .vcs()
        .resolve("main@origin")
        .ok()
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !has_main {
        write(root, "README.md", "# jjk smoke\n");
        engine.vcs().transaction(&mut |tx| {
            let id = tx.finalize_working_copy("chore: seed trunk")?;
            tx.create_bookmark("main", &id)?;
            Ok(())
        })?;
        engine.vcs().push("origin", "main", PushOpts::default())?;
    } else if !engine.vcs().bookmarks()?.iter().any(|bm| bm.name == "main") {
        let id = engine.vcs().resolve("main@origin")?[0].change_id.clone();
        engine.vcs().transaction(&mut |tx| tx.set_bookmark("main", &id))?;
    }

    // Two-branch stack.
    engine.checkout("main")?;
    engine.branch_create(a, true)?;
    write(root, &format!("{a}.txt"), "alpha\n");
    engine.commit(&format!("{a}: alpha"))?;
    engine.branch_create(b, true)?;
    write(root, &format!("{b}.txt"), "bravo\n");
    engine.commit(&format!("{b}: bravo"))?;

    engine.submit(jjk::engine::SubmitScope::Stack).await?;
    let gh = GhCli::new(Some(SLUG.to_string()));
    let pa = gh.get_pr(a).await?.expect("PR a");
    let pb = gh.get_pr(b).await?.expect("PR b");
    eprintln!("created #{} ({a}) base {}, #{} ({b}) base {}", pa.number, pa.base, pb.number, pb.base);

    // SQUASH-merge the bottom PR on GitHub (keep the branch so GitHub auto-retargets the dependent
    // PR to main rather than closing it; jjk sync deletes the merged branch afterwards).
    let out = Command::new("gh")
        .args([
            "pr",
            "merge",
            &pa.number.to_string(),
            "-R",
            SLUG,
            "--squash",
        ])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "gh pr merge failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("squash-merged #{}", pa.number);

    // Sync: reconcile the merged bottom, rebase + retarget the survivor.
    let report = engine.sync(true).await?;
    eprintln!("sync:\n{}", report.notes.join("\n"));
    anyhow::ensure!(report.conflicts.is_empty(), "unexpected conflicts: {:?}", report.conflicts);

    // feat-a gone locally; feat-b survives on trunk.
    let stack = engine.derive_stack()?;
    let names: Vec<_> = stack.branches.iter().map(|b| b.name.clone()).collect();
    anyhow::ensure!(names == vec![b.to_string()], "expected only {b}, got {names:?}");

    // The survivor's PR is open and retargeted onto main.
    let pb2 = gh.get_pr(b).await?.expect("PR b still open");
    anyhow::ensure!(pb2.number == pb.number, "no duplicate PR for b");
    anyhow::ensure!(pb2.base == "main", "b retargeted to main, got {}", pb2.base);
    anyhow::ensure!(pb2.state == PrState::Open, "b still open");
    Ok(())
}
