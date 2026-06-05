//! `jjk sync` — fetch trunk, reconcile merged branches, rebase survivors, push & retarget.
//! (Domain-mode sync lives in `domain.rs`; this is the ordinary stacked-branch path.)

use super::*;

impl Engine {
    /// `jjk sync` — fetch trunk, reconcile merged branches, rebase the survivors, and (when `push`)
    /// force-push and retarget their PR bases (ARCHITECTURE §6). Landing-method agnostic (JJ_NOTES
    /// §9): squash and merge-commit are both handled by the `roots(trunk()..top)` rebase +
    /// empty/immutable handling. With `push = false`, only local state is reconciled (no force-push,
    /// no PR retarget, no remote deletions).
    pub async fn sync(&mut self, push: bool) -> Result<Report> {
        // Domain expansion: the stack is a derived artifact, so sync rebases the *monolith* onto the
        // advanced trunk and re-expands, rather than reconciling user-managed branches.
        if ExpansionState::load(&self.root)?.is_some() {
            return self.sync_domain(push).await;
        }
        let mut report = Report::default();

        // 1. Capture branch→PR BEFORE fetching: a merge-commit landing absorbs the merged branch
        // into trunk on fetch, after which it no longer appears in the derived stack (JJ_NOTES §9b).
        self.ensure_fresh(&mut report).await?;
        let pre = self.derive_stack().await?;
        let candidates: Vec<(String, u64)> = pre
            .branches
            .iter()
            .filter_map(|b| b.pr.map(|pr| (b.name.clone(), pr)))
            .collect();

        // 2. fetch, then bring the local trunk bookmark up to the fetched remote position. jj only
        // auto-advances *tracked* local bookmarks on fetch, so without this a non-tracking trunk
        // would stay behind and the stack wouldn't rebase onto the new trunk (it only moves the
        // remote-tracking `trunk@remote`). Then query merged-state.
        self.vcs
            .fetch(&self.state.config.remote, Some(&self.state.config.trunk))
            .await?;
        report.note(format!("fetched {}", self.state.config.remote));
        self.advance_trunk_to_remote(&mut report).await?;
        // Query merged-state for all candidate PRs concurrently (independent reads by PR number) —
        // one round-trip instead of N. Order is preserved by zipping back onto `candidates`.
        let merged_flags = {
            let forge = self.forge().await?;
            futures::future::try_join_all(candidates.iter().map(|(_, pr)| forge.is_merged(*pr)))
                .await?
        };
        let merged_names: Vec<String> = candidates
            .iter()
            .zip(merged_flags)
            .filter(|(_, merged)| *merged)
            .map(|((name, _), _)| name.clone())
            .collect();
        if merged_names.is_empty() {
            report.note("no merged PRs to reconcile");
        } else {
            report.note(format!("merged: {}", merged_names.join(", ")));
        }

        // 3. Rebase the whole stack onto the (advanced) trunk. In the squash case the merged
        // branch's commits become empty; in the merge-commit case they're already in trunk's
        // ancestry (immutable) and are left untouched.
        let moved = self.rebase_stack_onto_trunk().await?;
        if moved > 0 {
            report.note(format!(
                "rebased {moved} stack {} onto trunk",
                plural(moved, "root", "roots")
            ));
        }

        // 4. Reconcile each merged branch.
        let post = self.derive_stack().await?;
        for name in &merged_names {
            match post.branch(name) {
                // Squash landing: the branch is now empty & mutable above trunk → abandon it,
                // which deletes the bookmark and reconnects the upstack to its parent.
                Some(b) => {
                    let range: Vec<ChangeId> =
                        b.commits.iter().map(|c| c.change_id.clone()).collect();
                    self.vcs.transaction(&mut |tx| tx.abandon(&range))?;
                    report.note(format!("abandoned merged '{name}' (squash landing)"));
                }
                // Merge-commit landing: the branch's commit is an ancestor of trunk (immutable);
                // nothing to abandon — just drop the (now redundant) local bookmark.
                None => {
                    if self.vcs.bookmarks().await?.iter().any(|bm| bm.name == *name) {
                        let n = name.clone();
                        self.vcs.transaction(&mut |tx| tx.delete_bookmark(&n))?;
                    }
                    report.note(format!("dropped merged '{name}' (merge-commit landing)"));
                }
            }
            self.state.branches.remove(name);
        }

        // 5. Recover the current workspace if a rewrite left it stale (cross-workspace: Phase 5).
        if self.vcs.is_stale().await.unwrap_or(false) {
            self.vcs.update_stale().await?;
            report.note("recovered stale working copy");
        }

        // 6 + 7. Force-push survivors and retarget their PR bases bottom-up. Skipped with --no-push,
        // which reconciles local state only (no force-push, no PR retarget, no remote deletions).
        if !push {
            report.note("synced local state only (--no-push); run `jjk sync` to push & retarget");
        } else {
            let remote = self.state.config.remote.clone();
            let survivors = self.derive_stack().await?;
            let trunk_name = survivors.trunk_name.clone();

            // Prefetch each pushable branch's PR record concurrently (read-only) so the sequential
            // push/retarget pass below doesn't pay a `gh pr list` round-trip per branch. Same query
            // as before (by head); conflicted branches are skipped anyway, so don't fetch them.
            let pr_by_head: std::collections::HashMap<String, Option<PrRef>> = {
                let forge = self.forge().await?;
                let names: Vec<String> = survivors
                    .branches
                    .iter()
                    .filter(|b| b.tracked && b.pr.is_some() && !b.has_conflict())
                    .map(|b| b.name.clone())
                    .collect();
                let recs =
                    futures::future::try_join_all(names.iter().map(|n| forge.get_pr(n))).await?;
                names.into_iter().zip(recs).collect()
            };

            let mut prev_tracked: Option<String> = None;
            for b in survivors.branches.iter().filter(|b| b.tracked) {
                // A conflicted commit cannot be pushed; skip and report (D4 — don't abort).
                if b.has_conflict() {
                    report.note(format!(
                        "skipped '{}' — has conflicts; run `jjk resolve`",
                        b.name
                    ));
                    prev_tracked = Some(b.name.clone());
                    continue;
                }
                // Only push when the branch actually moved — re-pushing an unchanged branch on
                // every sync is wasteful (and a no-op force-push). After a rebase the tip is a new
                // commit, so this pushes; an untouched branch is skipped.
                if !tip_on_remote(b, &remote) {
                    self.vcs.push(&remote, &b.name, PushOpts::default()).await?;
                    report.note(format!("pushed {}", b.name));
                }
                let base = prev_tracked.clone().unwrap_or_else(|| trunk_name.clone());
                if let Some(pr) = b.pr {
                    // Retarget only when the base actually changed (best-effort: a dependent PR may
                    // have been closed by GitHub when its base branch was deleted on merge — you
                    // can't retarget a closed PR, so report).
                    match pr_by_head.get(&b.name).cloned().flatten() {
                        Some(p) if p.state == PrState::Open && p.base != base => {
                            self.forge().await?.update_pr(pr, Some(&base)).await?;
                            report.note(format!("#{pr} {} → base {base}", b.name));
                        }
                        Some(p) if p.state != PrState::Open => report.note(format!(
                            "#{pr} {} is {}; not retargeting (reopen it to restack the PR)",
                            b.name, p.state
                        )),
                        _ => {} // base already correct, or PR not found — nothing to do
                    }
                }
                prev_tracked = Some(b.name.clone());
            }
            // Best-effort: propagate merged-branch deletions to the remote.
            if !merged_names.is_empty() {
                let _ = self.vcs.push_deleted(&remote).await;
            }

            // Refresh the stack-navigation comments for the (now reconciled) surviving stack. sync
            // changes the stack — merged branches drop out, bases move — so the comments would
            // otherwise go stale; this also surfaces the opt-in flourish when newly configured.
            let stack_prs: Vec<(String, u64)> = survivors
                .branches
                .iter()
                .filter(|b| b.tracked)
                .filter_map(|b| b.pr.map(|pr| (b.name.clone(), pr)))
                .collect();
            let yuji = self.vcs.config_get(YUJI_KEY).await?.as_deref() == Some(YUJI_VALUE);
            self.refresh_nav_comments(&stack_prs, yuji).await?;
        }

        self.state.save(&self.root)?;
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }
}
