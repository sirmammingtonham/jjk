//! `jjk submit` (stack / upstack / downstack / branch scopes), `pr view`, and the
//! stack-navigation comments. Shared helpers live in the parent `engine` module.

use super::*;
use crate::color;

impl Engine {
    /// `jjk pr view` — the current branch's PR. With `print`, return its URL (don't open a browser);
    /// otherwise open it in the browser and return `None`. Errors if not on a branch, or the branch
    /// has no submitted PR.
    pub async fn pr_view(&self, print: bool) -> Result<Option<String>> {
        let branch = self.current_branch().await?.ok_or(JjkError::NotOnBranch)?;
        let pr = self.state.pr_of(&branch).ok_or_else(|| {
            JjkError::Msg(format!("no PR for '{branch}' yet — run `jjk submit` first"))
        })?;
        self.forge().await?.view_pr(pr, !print).await
    }

    /// `jjk submit` — push tracked branches bottom-up and create/update their PRs with correct
    /// bases (downstack tracked branch, or trunk for the bottom). Idempotent. Uses the default
    /// (non-interactive) submit options; see [`submit_with`](Engine::submit_with).
    pub async fn submit(&mut self, scope: SubmitScope) -> Result<Report> {
        self.submit_with(scope, SubmitOptions::default()).await
    }

    /// Like [`submit`](Engine::submit) but with explicit options. For each **new** branch (no PR
    /// yet) the installed [`Prompter`] gathers the title/body/draft; existing PRs are just updated.
    pub async fn submit_with(&mut self, scope: SubmitScope, opts: SubmitOptions) -> Result<Report> {
        let mut report = Report::default();
        // Domain expansion (if active): (re)build the layer stack from the monolith first, then
        // submit operates on that stack anchored at its top layer. `None` = ordinary jjk.
        let anchor = self.maybe_expand_monolith(opts.no_review, &mut report).await?;
        if anchor.is_none() {
            self.ensure_fresh(&mut report).await?;
        }
        let stack = self.derive_stack_at(anchor.as_ref()).await?;
        let remote = self.state.config.remote.clone();
        let trunk_name = stack.trunk_name.clone();
        // Domain expansion: use the splitter's per-layer PR body (keyed by layer bookmark) instead of
        // the commit-subject default, so each PR explains its slice (plan §7).
        let layer_bodies: std::collections::HashMap<String, String> = ExpansionState::load(&self.root)?
            .map(|st| {
                st.layers
                    .into_iter()
                    .filter(|l| !l.body.trim().is_empty())
                    .map(|l| (l.bookmark, l.body))
                    .collect()
            })
            .unwrap_or_default();
        // Easter egg: a user can opt a PR-body flourish in via their jj config (undocumented).
        let yuji = self.vcs.config_get(YUJI_KEY).await?.as_deref() == Some(YUJI_VALUE);

        // Full tracked list (bottom→top) with correct bases. Bases come from the *whole* stack —
        // a subset submit still bases each PR on its real downstack branch, not the subset.
        struct Item {
            name: String,
            base: String,
            title: String,
            body: String,
            /// Whether the remote already has this branch at its tip (push would be a no-op).
            on_remote: bool,
        }
        let tracked: Vec<&Branch> = stack.branches.iter().filter(|b| b.tracked).collect();
        let mut plan: Vec<Item> = Vec::new();
        let mut prev_tracked: Option<String> = None;
        for b in &tracked {
            let base = prev_tracked.clone().unwrap_or_else(|| trunk_name.clone());
            let title = b
                .commits
                .first()
                .map(|c| c.subject().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| b.name.clone());
            let body = layer_bodies
                .get(&b.name)
                .cloned()
                .unwrap_or_else(|| pr_body(b, &base));
            plan.push(Item {
                name: b.name.clone(),
                base: base.clone(),
                title,
                body,
                on_remote: tip_on_remote(b, &remote),
            });
            prev_tracked = Some(b.name.clone());
        }

        if plan.is_empty() {
            report.note("no tracked branches to submit");
            return Ok(report);
        }

        // Which indices to submit, per scope (relative to the current branch).
        let cur_idx = stack
            .current
            .as_ref()
            .and_then(|c| plan.iter().position(|i| &i.name == c));
        let to_submit: Vec<usize> = match scope {
            SubmitScope::Stack => (0..plan.len()).collect(),
            SubmitScope::Branch => vec![cur_idx.ok_or(JjkError::NotOnBranch)?],
            SubmitScope::Upstack => (cur_idx.ok_or(JjkError::NotOnBranch)?..plan.len()).collect(),
            SubmitScope::Downstack => (0..=cur_idx.ok_or(JjkError::NotOnBranch)?).collect(),
        };
        drop(stack);

        // Look up the existing PR (if any) for every in-scope branch *concurrently*: these are
        // independent read-only `gh pr list` calls, so one round-trip's latency instead of N. The
        // per-call query (by head, newest, any state) is unchanged. Doing them up front also fails
        // before any push if the forge is unreachable. Results stay aligned with `to_submit`.
        let existing: Vec<Option<PrRef>> = {
            let forge = self.forge().await?;
            futures::future::try_join_all(to_submit.iter().map(|&i| forge.get_pr(&plan[i].name)))
                .await?
        };

        // Phase 1 — open the in-scope branches bottom→top. Sequential: pushes can't run
        // concurrently (one jj op/repo), create needs its base ref pushed first, and the prompt for
        // a new PR is interactive (one at a time). No comments yet — like git-spice, we defer every
        // navigation comment to phase 2, once all PR numbers in the stack are known, so each comment
        // is written correct the first time (no placeholder/renumber step).
        for (slot, &i) in to_submit.iter().enumerate() {
            let item = &plan[i];
            // Only push when the branch actually moved — don't re-push an unchanged branch.
            if !item.on_remote {
                self.vcs.push(&remote, &item.name, PushOpts::default()).await?;
            }
            let number = match &existing[slot] {
                Some(pr) => {
                    let pr_num = pr.number;
                    // Retarget the base only when it actually changed — never the body, so we don't
                    // clobber the author's description, and we skip a no-op API call on re-submit.
                    if pr.base != item.base {
                        self.forge().await?.update_pr(pr_num, Some(&item.base)).await?;
                        report.note(format!(
                            "updated #{pr_num} {} (base {})",
                            item.name, item.base
                        ));
                    }
                    pr_num
                }
                None => {
                    let defaults = PrDraft {
                        title: item.title.clone(),
                        body: item.body.clone(),
                        draft: opts.draft,
                    };
                    let Some(d) = self.prompter.new_pr(&item.name, &item.base, defaults)? else {
                        report.note(format!("skipped {} (no PR created)", item.name));
                        continue;
                    };
                    let pr = self
                        .forge()
                        .await?
                        .create_pr(&item.name, &item.base, &d.title, &d.body, d.draft)
                        .await?;
                    let kind = if d.draft { "draft " } else { "" };
                    report.note(color::green(&format!(
                        "created {}#{} {} (base {})",
                        kind, pr.number, item.name, item.base
                    )));
                    pr.number
                }
            };
            self.state.branch_mut(&item.name).pr = Some(number);
        }
        self.state.save(&self.root)?;

        // Phase 2 — now that every PR number is known, upsert the navigation comment across the
        // whole stack (all tracked branches that have a PR) in one pass, parallelized across PRs.
        let stack_prs: Vec<(String, u64)> = plan
            .iter()
            .filter_map(|it| self.state.pr_of(&it.name).map(|pr| (it.name.clone(), pr)))
            .collect();
        self.refresh_nav_comments(&stack_prs, yuji).await?;
        Ok(report)
    }

    /// Refresh the stack-navigation comment for every PR in `stack_prs` (`(branch, pr)`, bottom→top)
    /// and persist each comment's forge id in state. Seeding from the cached ids lets later runs
    /// edit comments in place — skipping the `find_comment` lookup (which paginates all of a PR's
    /// comments), like git-spice. Saves state and returns the number of comments touched.
    pub(in crate::engine) async fn refresh_nav_comments(&mut self, stack_prs: &[(String, u64)], yuji: bool) -> Result<usize> {
        // Seed the per-PR comment ids we already know (pr → comment id) from state.
        let known: std::collections::HashMap<u64, u64> = stack_prs
            .iter()
            .filter_map(|(name, pr)| self.state.nav_comment_of(name).map(|cid| (*pr, cid)))
            .collect();
        // When expansion is active this stack was auto-split from a monolith — note it subtly.
        let domain = ExpansionState::load(&self.root)?.is_some();
        let touched = self.upsert_nav_comments(stack_prs, yuji, domain, &known).await?;
        if touched.is_empty() {
            return Ok(0);
        }
        // Persist the (possibly newly created) comment ids back to state, keyed by branch.
        let name_of: std::collections::HashMap<u64, &str> =
            stack_prs.iter().map(|(n, pr)| (*pr, n.as_str())).collect();
        for (pr, cid) in &touched {
            if let Some(name) = name_of.get(pr) {
                self.state.branch_mut(name).nav_comment_id = Some(*cid);
            }
        }
        self.state.save(&self.root)?;
        Ok(touched.len())
    }

    /// Upsert the stack-navigation comment on each PR in `prs` (bottom→top order). Runs across PRs
    /// concurrently (they target different PRs), while each PR's update/find/create stays ordered,
    /// so it's idempotent — one comment per PR, never duplicated. `known` supplies comment ids
    /// already known (cached in state) to skip the `find_comment` lookup; if that cached id is stale
    /// (comment deleted), it self-heals by rediscovering or recreating the comment — like git-spice.
    /// Returns `(pr, comment_id)` for every PR touched so callers can persist them. No-op (`[]`) for
    /// fewer than 2 PRs — a lone PR has no stack to navigate.
    async fn upsert_nav_comments(
        &self,
        prs: &[(String, u64)],
        yuji: bool,
        domain: bool,
        known: &std::collections::HashMap<u64, u64>,
    ) -> Result<Vec<(u64, u64)>> {
        if prs.len() < 2 {
            return Ok(Vec::new());
        }
        let forge = self.forge().await?;
        let tasks = prs.iter().enumerate().map(|(idx, (_, pr))| {
            let pr = *pr;
            let body = nav_comment_body(prs, idx, yuji, domain);
            let known_id = known.get(&pr).copied();
            async move {
                // Fast path: edit the comment we already know about. If that fails (e.g. the author
                // deleted it), fall back to discovering or recreating it.
                if let Some(id) = known_id {
                    if forge.update_comment(id, &body).await.is_ok() {
                        return Ok((pr, id));
                    }
                }
                let id = match forge.find_comment(pr, NAV_MARKER).await? {
                    Some(id) => {
                        forge.update_comment(id, &body).await?;
                        id
                    }
                    None => forge.create_comment(pr, &body).await?,
                };
                Ok::<(u64, u64), anyhow::Error>((pr, id))
            }
        });
        futures::future::try_join_all(tasks).await
    }
}

/// Hidden marker used to find & update the navigation comment idempotently.
const NAV_MARKER: &str = "<!-- jjk:nav -->";

/// Build the stack-navigation comment for the PR at `current_idx` in `prs` (bottom→top). The PR
/// numbers expand into GitHub's rich previews on their own, so we list just `#N`; a prominent
/// footer shows this PR's position (`x/N`) and links jjk. When `domain`, the footer also notes the
/// stack was auto-split from one branch with Domain Expansion (subtle, just a trailing clause).
fn nav_comment_body(prs: &[(String, u64)], current_idx: usize, yuji: bool, domain: bool) -> String {
    let n = prs.len();
    let mut s = format!("**🥞 This change is part of the following stack · PR {}/{}**\n\n", current_idx + 1, n);
    for (i, (_branch, pr)) in prs.iter().enumerate() {
        let indent = "    ".repeat(i);
        let marker = if i == current_idx { " ◀" } else { "" };
        s.push_str(&format!("{indent}- #{pr}{marker}\n"));
    }
    s.push_str("\nManaged by [jjk](https://github.com/sirmammingtonham/jjk)");
    if domain {
        s.push_str(" · split automatically with [Domain Expansion](https://ethan.website/jjk/domain-expansion)");
    }
    s.push_str(".\n");
    if yuji {
        s.push('\n');
        s.push_str(YUJI_FLOURISH);
        s.push('\n');
    }
    s.push_str(NAV_MARKER);
    s.push('\n');
    s
}
