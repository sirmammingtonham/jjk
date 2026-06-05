//! Worktrees (jj workspaces) and stash park/pop.

use super::*;

impl Engine {
    // ---------------------------------------------------------------- worktrees (jj workspaces)

    /// `jjk worktree add <path> [name] [--branch B]` — create a jj workspace. The new workspace's
    /// `@` starts as an empty child of `B`'s tip (or trunk). Great for parallel agents per branch.
    pub async fn worktree_add(
        &self,
        path: &Path,
        name: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Report> {
        let mut report = Report::default();
        let name = match name {
            Some(n) => n.to_string(),
            None => path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| JjkError::Msg("could not derive workspace name from path".into()))?
                .to_string(),
        };
        let at = match branch {
            Some(b) if b != self.state.config.trunk => self.branch_tip(b).await?,
            _ => self.trunk_anchor().await?.1,
        };
        self.vcs.add_workspace(path, &name, &at).await?;
        report.note(format!("added workspace '{name}' at {}", path.display()));
        if let Some(b) = branch {
            report.note(format!("starting on '{b}'"));
        }
        Ok(report)
    }

    /// `jjk worktree list` — workspaces with their per-workspace current branch.
    pub async fn worktree_list(&self) -> Result<Vec<WorktreeRow>> {
        let workspaces = self.vcs.workspaces().await?;
        // Each workspace's "current branch" is an independent `heads(::<ws>@ & STACK)` read —
        // resolve them all concurrently (one round-trip instead of one per workspace).
        let revsets: Vec<String> = workspaces
            .iter()
            .map(|ws| format!("heads(::{}@ & {STACK_BOOKMARKS})", ws.name))
            .collect();
        let revset_refs: Vec<&str> = revsets.iter().map(String::as_str).collect();
        let resolved = self.vcs.resolve_many(&revset_refs).await?;
        let mut rows = Vec::with_capacity(workspaces.len());
        for (ws, commits) in workspaces.into_iter().zip(resolved) {
            let current = commits
                .into_iter()
                .next()
                .and_then(|c| c.local_bookmarks.into_iter().find(|b| self.is_stack_bookmark(b)));
            rows.push(WorktreeRow {
                name: ws.name,
                working_copy: ws.working_copy,
                current_branch: current,
                is_stale: ws.is_stale,
            });
        }
        Ok(rows)
    }

    /// `jjk worktree remove <name>` — stop tracking a workspace (files are left on disk).
    pub async fn worktree_remove(&self, name: &str) -> Result<Report> {
        let mut report = Report::default();
        self.vcs.forget_workspace(name).await?;
        report.note(format!("removed workspace '{name}' (files left on disk)"));
        Ok(report)
    }

    // ---------------------------------------------------------------- stash (muscle memory; D-§5)

    /// `jjk stash` — park the working-copy changes aside on a `jjk/stash/N` bookmark and leave a
    /// clean empty `@` in place. (Mostly unnecessary in jj since switching is safe.)
    pub async fn stash(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        // Snapshot so on-disk edits are seen before deciding whether there's anything to stash.
        let wc = self.vcs.snapshot().await?;
        if wc.is_empty {
            report.note("nothing to stash (working copy is clean)");
            return Ok(report);
        }
        let parent = wc
            .parents
            .first()
            .cloned()
            .ok_or_else(|| JjkError::Msg("working copy has no parent to stash onto".into()))?;
        let n = self.next_stash_number().await?;
        let name = format!("jjk/stash/{n}");
        let wc_id = wc.change_id.clone();
        let name_cl = name.clone();
        self.vcs.transaction(&mut |tx| {
            tx.create_bookmark(&name_cl, &wc_id)?; // park the changes
            tx.new_child(&parent)?; // clean empty @ on the same parent
            Ok(())
        })?;
        report.note(format!("stashed working copy as {name}"));
        Ok(report)
    }

    /// `jjk stash pop` — restore the most recent stash into the current working copy.
    pub async fn stash_pop(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stashes = self.list_stashes().await?;
        let (name, from) = stashes
            .into_iter()
            .max_by_key(|(_, _, n)| *n)
            .map(|(name, id, _)| (name, id))
            .ok_or_else(|| JjkError::Msg("no stash to pop".into()))?;
        let into = self.vcs.working_copy().await?.change_id;
        let name_cl = name.clone();
        self.vcs.transaction(&mut |tx| {
            tx.squash(&from, &into)?; // restore changes into @ (abandons the now-empty stash)
            tx.forget_bookmark(&name_cl)?; // bookmark slid to the parent; drop it (local-only)
            Ok(())
        })?;
        report.note(format!("popped {name}"));
        Ok(report)
    }

    async fn list_stashes(&self) -> Result<Vec<(String, ChangeId, u64)>> {
        let mut out = Vec::new();
        for b in self.vcs.bookmarks().await? {
            if let Some(rest) = b.name.strip_prefix("jjk/stash/") {
                if let Ok(n) = rest.parse::<u64>() {
                    out.push((b.name.clone(), b.target, n));
                }
            }
        }
        Ok(out)
    }

    async fn next_stash_number(&self) -> Result<u64> {
        Ok(self
            .list_stashes()
            .await?
            .into_iter()
            .map(|(_, _, n)| n)
            .max()
            .unwrap_or(0)
            + 1)
    }

}
