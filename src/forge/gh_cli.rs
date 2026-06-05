//! GitHub forge adapter shelling out to the `gh` CLI. Targets the repo explicitly via `-R
//! <owner/repo>` (derived from the remote URL) so it never depends on cwd detection. Reuses the
//! user's existing `gh auth` token. All `gh` calls are centralized here.

use crate::error::Result;
use crate::forge::Forge;
use crate::model::{PrRef, PrState};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

/// Forge adapter backed by the `gh` binary.
pub struct GhCli {
    /// `owner/repo` slug, or None if no remote is configured (forge ops then error clearly).
    slug: Option<String>,
}

impl GhCli {
    pub fn new(slug: Option<String>) -> Self {
        Self { slug }
    }

    /// Parse `owner/repo` out of a GitHub remote URL (https/ssh, with optional `user:pass@` creds).
    pub fn slug_from_url(url: &str) -> Option<String> {
        let s = url.trim().trim_end_matches('/');
        // ssh shorthand: git@github.com:owner/repo(.git)
        let rest = if let Some(r) = s.strip_prefix("git@github.com:") {
            r
        } else {
            // https://[user[:pass]@]github.com/owner/repo or ssh://git@github.com/owner/repo
            s.split_once("github.com/")?.1
        };
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let (owner, repo) = rest.split_once('/')?;
        let repo = repo.split('/').next().unwrap_or(repo);
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(format!("{owner}/{repo}"))
    }

    fn slug(&self) -> Result<&str> {
        self.slug
            .as_deref()
            .ok_or_else(|| anyhow!("no GitHub remote configured; cannot run forge operations"))
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("gh")
            .args(args)
            .output()
            .await
            .context("failed to spawn `gh` — is it installed and authenticated (`gh auth login`)?")?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh {} failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Subset of `gh pr` JSON we parse.
#[derive(Debug, Deserialize)]
struct GhPr {
    number: u64,
    #[serde(rename = "headRefName", default)]
    head: String,
    #[serde(rename = "baseRefName", default)]
    base: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
}

fn map_state(s: &str) -> PrState {
    match s.to_ascii_uppercase().as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

impl From<GhPr> for PrRef {
    fn from(p: GhPr) -> Self {
        PrRef {
            number: p.number,
            head: p.head,
            base: p.base,
            state: map_state(&p.state),
            url: p.url,
            title: p.title,
        }
    }
}

const PR_FIELDS: &str = "number,headRefName,baseRefName,state,url,title";

#[async_trait]
impl Forge for GhCli {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>> {
        let slug = self.slug()?.to_string();
        // List PRs (any state) whose head is `branch`; newest first.
        let out = self
            .run(&[
                "pr", "list", "-R", &slug, "--head", branch, "--state", "all", "--json",
                PR_FIELDS, "--limit", "1",
            ])
            .await?;
        let prs: Vec<GhPr> = serde_json::from_str(&out).context("parsing gh pr list JSON")?;
        Ok(prs.into_iter().next().map(Into::into))
    }

    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrRef> {
        let slug = self.slug()?.to_string();
        // `gh pr create` prints the PR URL; fetch the structured record afterwards.
        let mut args = vec![
            "pr", "create", "-R", &slug, "--head", head, "--base", base, "--title", title,
            "--body", body,
        ];
        if draft {
            args.push("--draft");
        }
        let url = self.run(&args).await?;
        let url = url.trim();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("could not parse PR number from `gh pr create` output: {url:?}"))?;
        Ok(PrRef {
            number,
            head: head.to_string(),
            base: base.to_string(),
            state: PrState::Open,
            url: url.to_string(),
            title: title.to_string(),
        })
    }

    async fn update_pr(&self, pr: u64, base: Option<&str>) -> Result<()> {
        let slug = self.slug()?.to_string();
        let num = pr.to_string();
        let mut args: Vec<&str> = vec!["pr", "edit", &num, "-R", &slug];
        if let Some(b) = base {
            args.push("--base");
            args.push(b);
        }
        if args.len() <= 5 {
            return Ok(()); // nothing to change — never touches the body/description
        }
        self.run(&args).await?;
        Ok(())
    }

    async fn is_merged(&self, pr: u64) -> Result<bool> {
        Ok(self.pr_state(pr).await? == PrState::Merged)
    }

    async fn pr_state(&self, pr: u64) -> Result<PrState> {
        let slug = self.slug()?.to_string();
        let num = pr.to_string();
        let out = self
            .run(&["pr", "view", &num, "-R", &slug, "--json", "state"])
            .await?;
        #[derive(Deserialize)]
        struct S {
            #[serde(default)]
            state: String,
        }
        let s: S = serde_json::from_str(&out).context("parsing gh pr view JSON")?;
        Ok(map_state(&s.state))
    }

    async fn view_pr(&self, pr: u64, web: bool) -> Result<Option<String>> {
        let slug = self.slug()?.to_string();
        let num = pr.to_string();
        if web {
            // `gh pr view --web` resolves the right host (incl. Enterprise) and opens the browser,
            // printing its own "Opening …" line.
            self.run(&["pr", "view", &num, "-R", &slug, "--web"]).await?;
            Ok(None)
        } else {
            let url = self
                .run(&["pr", "view", &num, "-R", &slug, "--json", "url", "--jq", ".url"])
                .await?;
            Ok(Some(url.trim().to_string()))
        }
    }

    async fn find_comment(&self, pr: u64, marker: &str) -> Result<Option<u64>> {
        let slug = self.slug()?.to_string();
        // PR comments are issue comments. Page through and pick the first carrying our marker.
        let out = self
            .run(&[
                "api",
                "--paginate",
                &format!("repos/{slug}/issues/{pr}/comments"),
                "--jq",
                &format!(".[] | select(.body | contains(\"{marker}\")) | .id"),
            ])
            .await?;
        Ok(out.lines().next().and_then(|l| l.trim().parse::<u64>().ok()))
    }

    async fn create_comment(&self, pr: u64, body: &str) -> Result<u64> {
        let slug = self.slug()?.to_string();
        let out = self
            .run(&[
                "api",
                "-X",
                "POST",
                &format!("repos/{slug}/issues/{pr}/comments"),
                "-f",
                &format!("body={body}"),
                "--jq",
                ".id",
            ])
            .await?;
        out.trim()
            .parse::<u64>()
            .map_err(|_| anyhow!("could not parse comment id from gh output: {out:?}"))
    }

    async fn update_comment(&self, comment_id: u64, body: &str) -> Result<()> {
        let slug = self.slug()?.to_string();
        self.run(&[
            "api",
            "-X",
            "PATCH",
            &format!("repos/{slug}/issues/comments/{comment_id}"),
            "-f",
            &format!("body={body}"),
        ])
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::GhCli;

    #[test]
    fn parses_https_ssh_and_trailing_git() {
        assert_eq!(
            GhCli::slug_from_url("https://github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            GhCli::slug_from_url("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            GhCli::slug_from_url("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            GhCli::slug_from_url("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        // Token-authenticated URL (used for pushes).
        assert_eq!(
            GhCli::slug_from_url("https://x-access-token:ghp_abc@github.com/owner/repo.git")
                .as_deref(),
            Some("owner/repo")
        );
        assert_eq!(GhCli::slug_from_url("/tmp/local/bare.git"), None);
    }
}
