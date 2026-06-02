//! GitHub forge adapter backed by the **octocrab** crate (native HTTP, in-process, async).
//!
//! This is the default forge backend. It reuses the user's existing GitHub auth by reading a
//! token from the environment (`GH_TOKEN`/`GITHUB_TOKEN`) or, failing that, from `gh auth token`
//! — so there is nothing new to configure for someone already using `gh`. All GitHub API calls in
//! the crate-backed path live here; the `gh` subprocess adapter ([`super::gh_cli`]) remains a
//! config-selectable fallback.

use crate::error::Result;
use crate::forge::Forge;
use crate::model::{PrRef, PrState};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use octocrab::models::{issues::Comment, pulls::PullRequest, CommentId, IssueState};
use octocrab::params::State as ListState;
use octocrab::Octocrab;

/// A GitHub repository coordinate parsed from a remote URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoCoord {
    /// Host (`github.com`, or an Enterprise host).
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl RepoCoord {
    /// `owner/repo`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Parse `host`, `owner`, and `repo` out of a git remote URL (https/ssh, with optional
    /// `user:pass@` creds). Returns `None` if it isn't a recognizable GitHub-style URL.
    pub fn parse(url: &str) -> Option<RepoCoord> {
        let s = url.trim().trim_end_matches('/');
        // ssh shorthand: [user@]host:owner/repo(.git)
        let (host, rest) = if let Some(after) = s.strip_prefix("git@") {
            // git@host:owner/repo
            let (host, path) = after.split_once(':')?;
            (host.to_string(), path)
        } else {
            // scheme://[user[:pass]@]host/owner/repo  (https or ssh://)
            let after_scheme = s.split_once("://").map(|(_, r)| r).unwrap_or(s);
            // strip any userinfo
            let after_auth = after_scheme.rsplit_once('@').map(|(_, r)| r).unwrap_or(after_scheme);
            let (host, path) = after_auth.split_once('/')?;
            (host.to_string(), path)
        };
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let (owner, repo) = rest.split_once('/')?;
        let repo = repo.split('/').next().unwrap_or(repo);
        if host.is_empty() || owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(RepoCoord {
            host,
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

/// Forge adapter backed by octocrab.
pub struct Octo {
    client: Octocrab,
    coord: RepoCoord,
}

impl Octo {
    /// Build a client for `coord`, sourcing a token from the environment or `gh`.
    pub fn new(coord: RepoCoord) -> Result<Self> {
        let token = github_token().ok_or_else(|| {
            anyhow!(
                "no GitHub token found; set GH_TOKEN/GITHUB_TOKEN or run `gh auth login` \
                 (jjk reads `gh auth token`)"
            )
        })?;
        let mut builder = Octocrab::builder().personal_token(token);
        // Non-github.com hosts are GitHub Enterprise: API lives under /api/v3.
        if coord.host != "github.com" {
            builder = builder
                .base_uri(format!("https://{}/api/v3", coord.host))
                .context("invalid GitHub Enterprise host")?;
        }
        let client = builder.build().context("building GitHub API client")?;
        Ok(Self { client, coord })
    }

    fn owner(&self) -> &str {
        &self.coord.owner
    }
    fn repo(&self) -> &str {
        &self.coord.repo
    }
}

/// Read a GitHub token, preferring explicit env vars and falling back to the `gh` CLI's stored
/// auth so existing `gh` users need no extra setup.
fn github_token() -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Map octocrab's open/closed state plus a merge timestamp into our tri-state.
fn pr_state(state: &IssueState, merged_at: bool) -> PrState {
    if merged_at {
        return PrState::Merged;
    }
    match state {
        IssueState::Open => PrState::Open,
        IssueState::Closed => PrState::Closed,
        _ => PrState::Open,
    }
}

impl From<PullRequest> for PrRef {
    fn from(p: PullRequest) -> Self {
        PrRef {
            number: p.number,
            head: p.head.ref_field,
            base: p.base.ref_field,
            state: pr_state(&p.state, p.merged || p.merged_at.is_some()),
            url: p.html_url.to_string(),
            title: p.title,
        }
    }
}

#[async_trait]
impl Forge for Octo {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>> {
        // `head` filter is `owner:branch`; query any state, newest first, just the top match.
        let page = self
            .client
            .pulls(self.owner(), self.repo())
            .list()
            .head(format!("{}:{}", self.owner(), branch))
            .state(ListState::All)
            .per_page(1)
            .send()
            .await
            .context("listing pull requests")?;
        let Some(pr) = page.items.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(PrRef {
            number: pr.number,
            head: pr.head.ref_field,
            base: pr.base.ref_field,
            state: pr_state(&pr.state, pr.merged_at.is_some()),
            url: pr.html_url.to_string(),
            title: pr.title,
        }))
    }

    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrRef> {
        let pr = self
            .client
            .pulls(self.owner(), self.repo())
            .create(title, head, base)
            .body(body)
            .draft(Some(draft))
            .send()
            .await
            .context("creating pull request")?;
        Ok(pr.into())
    }

    async fn update_pr(&self, pr: u64, base: Option<&str>) -> Result<()> {
        let Some(base) = base else {
            return Ok(()); // nothing to change — never touches the body/description
        };
        self.client
            .pulls(self.owner(), self.repo())
            .update(pr)
            .base(base.to_string())
            .send()
            .await
            .context("retargeting pull request base")?;
        Ok(())
    }

    async fn is_merged(&self, pr: u64) -> Result<bool> {
        self.client
            .pulls(self.owner(), self.repo())
            .is_merged(pr)
            .await
            .context("checking PR merge status")
    }

    async fn view_pr(&self, pr: u64, web: bool) -> Result<Option<String>> {
        // The canonical URL doesn't require a round-trip — it's deterministic from the coordinate.
        let url = format!(
            "https://{}/{}/{}/pull/{pr}",
            self.coord.host,
            self.owner(),
            self.repo()
        );
        if web {
            open_in_browser(&url)?;
            Ok(None)
        } else {
            Ok(Some(url))
        }
    }

    async fn find_comment(&self, pr: u64, marker: &str) -> Result<Option<u64>> {
        // PR comments are issue comments. Page through and pick the first carrying our marker.
        let first = self
            .client
            .issues(self.owner(), self.repo())
            .list_comments(pr)
            .per_page(100)
            .send()
            .await
            .context("listing PR comments")?;
        let all = self
            .client
            .all_pages(first)
            .await
            .context("paging PR comments")?;
        Ok(all
            .into_iter()
            .find(|c: &Comment| c.body.as_deref().is_some_and(|b| b.contains(marker)))
            .map(|c| c.id.0))
    }

    async fn create_comment(&self, pr: u64, body: &str) -> Result<u64> {
        let c = self
            .client
            .issues(self.owner(), self.repo())
            .create_comment(pr, body)
            .await
            .context("creating PR comment")?;
        Ok(c.id.0)
    }

    async fn update_comment(&self, comment_id: u64, body: &str) -> Result<()> {
        self.client
            .issues(self.owner(), self.repo())
            .update_comment(CommentId(comment_id), body)
            .await
            .context("updating PR comment")?;
        Ok(())
    }
}

/// Open `url` in the user's default browser via the platform opener. Not a GitHub call — purely an
/// OS action, so it stays a tiny subprocess.
fn open_in_browser(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .status()
        .with_context(|| format!("failed to open browser ({opener})"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RepoCoord;

    #[test]
    fn parses_https_ssh_enterprise_and_creds() {
        let c = RepoCoord::parse("https://github.com/owner/repo.git").unwrap();
        assert_eq!((c.host.as_str(), c.slug().as_str()), ("github.com", "owner/repo"));

        let c = RepoCoord::parse("git@github.com:owner/repo.git").unwrap();
        assert_eq!((c.host.as_str(), c.slug().as_str()), ("github.com", "owner/repo"));

        let c = RepoCoord::parse("https://github.com/owner/repo").unwrap();
        assert_eq!(c.slug(), "owner/repo");

        let c = RepoCoord::parse("ssh://git@github.com/owner/repo.git").unwrap();
        assert_eq!((c.host.as_str(), c.slug().as_str()), ("github.com", "owner/repo"));

        // token-authenticated URL (used for pushes)
        let c = RepoCoord::parse("https://x-access-token:ghp_abc@github.com/owner/repo.git").unwrap();
        assert_eq!((c.host.as_str(), c.slug().as_str()), ("github.com", "owner/repo"));

        // GitHub Enterprise host is preserved
        let c = RepoCoord::parse("https://ghe.corp.example/team/proj.git").unwrap();
        assert_eq!((c.host.as_str(), c.slug().as_str()), ("ghe.corp.example", "team/proj"));

        assert!(RepoCoord::parse("/tmp/local/bare.git").is_none());
    }
}
