//! Domain Expansion **live** smoke test: a real Anthropic split + real stacked PRs against the
//! throwaway GitHub repo. Ignored by default; run with:
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo test --test phase24_domain_live -- --ignored --nocapture
//! ```
//!
//! Requires `ANTHROPIC_API_KEY` (the splitter is built from env when no fake is injected), plus
//! `gh auth` + write access to the smoke repo. Uses a unique monolith name per run and cleans up
//! the generated layer PRs/branches afterwards.

mod common;

use common::{init_identity, write};
use jjk::engine::expansion::{ExpansionState, Mode};
use jjk::engine::{Engine, SubmitScope};
use jjk::forge::gh_cli::GhCli;
use jjk::forge::Forge;
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
#[ignore = "live: hits the Anthropic API + GitHub; run explicitly with --ignored"]
async fn live_domain_expansion_splits_and_submits() {
    init_identity();
    assert!(
        std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.trim().is_empty()),
        "set ANTHROPIC_API_KEY to run the live domain test"
    );
    let repo_url = std::env::var("JJK_SMOKE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let token = gh_token();
    let authed = repo_url.replace("https://", &format!("https://x-access-token:{token}@"));

    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let monolith = format!("smk-de-{ts}");

    let tmp = tempfile::tempdir().unwrap();
    Engine::repo_init(tmp.path(), Some("main".into()), Some("origin".into()))
        .await
        .unwrap();
    let mut engine = Engine::open(tmp.path()).unwrap();
    engine.vcs().add_remote("origin", &authed).await.unwrap();
    engine.set_forge(Box::new(GhCli::new(Some(SLUG.to_string()))));
    // No set_splitter → the engine builds the real AnthropicLlm from ANTHROPIC_API_KEY.

    let result = run_body(&mut engine, tmp.path(), &monolith).await;

    // ---- cleanup (best effort): close every generated layer PR + delete its remote branch ----
    if let Ok(Some(st)) = ExpansionState::load(tmp.path()) {
        let gh = GhCli::new(Some(SLUG.to_string()));
        for layer in &st.layers {
            if let Ok(Some(pr)) = gh.get_pr(&layer.bookmark).await {
                close_pr(pr.number);
            }
            delete_remote_branch(&layer.bookmark);
        }
    }

    result.expect("live domain flow failed");
}

async fn run_body(engine: &mut Engine, root: &Path, monolith: &str) -> anyhow::Result<()> {
    // Ensure a local `main` bookmark tracking the remote trunk (PR bases need it).
    engine.vcs().fetch("origin", None).await.ok();
    match engine
        .vcs()
        .resolve("main@origin")
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
    {
        Some(c) => {
            if !engine.vcs().bookmarks().await?.iter().any(|b| b.name == "main") {
                let id = c.change_id.clone();
                engine.vcs().transaction(&mut |tx| tx.set_bookmark("main", &id))?;
            }
        }
        None => {
            write(root, "README.md", "# jjk smoke\n");
            engine.vcs().transaction(&mut |tx| {
                let id = tx.finalize_working_copy("chore: seed trunk")?;
                tx.create_bookmark("main", &id)?;
                Ok(())
            })?;
            engine.vcs().push("origin", "main", PushOpts::default()).await?;
        }
    }

    // Build a monolith: two independent capabilities (auth, cache) plus a wiring file that uses
    // both — a realistic split target (the wiring depends on the other two).
    engine.checkout("main").await?;
    engine.branch_create(monolith, true).await?;
    write(
        root,
        "auth.rs",
        "pub fn login(user: &str) -> bool {\n    !user.is_empty()\n}\n\npub fn logout(user: &str) {\n    let _ = user;\n}\n",
    );
    write(
        root,
        "cache.rs",
        "use std::collections::HashMap;\n\npub struct Cache {\n    map: HashMap<String, String>,\n}\n\nimpl Cache {\n    pub fn new() -> Self {\n        Cache { map: HashMap::new() }\n    }\n    pub fn get(&self, k: &str) -> Option<&String> {\n        self.map.get(k)\n    }\n    pub fn set(&mut self, k: String, v: String) {\n        self.map.insert(k, v);\n    }\n}\n",
    );
    write(
        root,
        "app.rs",
        "mod auth;\nmod cache;\n\npub fn handle(user: &str) {\n    if auth::login(user) {\n        let mut c = cache::Cache::new();\n        c.set(user.to_string(), \"online\".to_string());\n    }\n}\n",
    );
    engine.commit(&format!("{monolith}: auth + cache + app wiring")).await?;

    // Activate domain expansion and submit — real LLM split → reconstruction → real PRs.
    engine
        .domain_activate(Mode::Change, None, None, None, None)
        .await?;
    let report = engine.submit(SubmitScope::Stack).await?;
    eprintln!("--- submit report ---\n{}\n", report.notes.join("\n"));

    // Inspect the persisted split.
    let st = ExpansionState::load(root)?.expect("domain mode active after submit");
    eprintln!("--- {} layer(s) ---", st.layers.len());
    for (i, l) in st.layers.iter().enumerate() {
        eprintln!("  {}. {} [{}] compat={}", i + 1, l.title, l.slug, l.backward_compatible);
    }
    assert!(!st.layers.is_empty(), "splitter produced at least one layer");

    // Hard invariant, live: the top of the reconstructed stack equals the monolith.
    let monolith_tip = engine
        .vcs()
        .resolve(monolith)
        .await?
        .into_iter()
        .next()
        .expect("monolith resolves")
        .change_id;
    let top_bm = st.layers.last().unwrap().bookmark.clone();
    let top = engine
        .vcs()
        .resolve(&top_bm)
        .await?
        .into_iter()
        .next()
        .expect("top layer resolves")
        .change_id;
    assert!(
        engine.vcs().trees_equal(&top, &monolith_tip).await?,
        "top layer tree must equal the monolith (no changes lost)"
    );

    // Every layer should have a real PR, based bottom-up.
    let gh = GhCli::new(Some(SLUG.to_string()));
    let mut prev_base = "main".to_string();
    for layer in &st.layers {
        let pr = gh
            .get_pr(&layer.bookmark)
            .await?
            .unwrap_or_else(|| panic!("PR created for {}", layer.bookmark));
        eprintln!("PR #{} {} → base {}", pr.number, layer.bookmark, pr.base);
        assert_eq!(pr.base, prev_base, "layer based on the one below it");
        prev_base = layer.bookmark.clone();
    }

    Ok(())
}
