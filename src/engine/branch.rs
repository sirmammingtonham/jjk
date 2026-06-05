//! Branch + restacking verbs: create/checkout/onto/rename/squash/fold/split, nav, track.

use super::*;

impl Engine {
    /// `jjk branch create [NAME]` (tracked) / `jjk checkout -b NAME` (untracked).
    pub async fn branch_create(&mut self, name: &str, tracked: bool) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        if self.vcs.bookmarks().await?.iter().any(|b| b.name == name) {
            return Err(JjkError::Msg(format!("branch '{name}' already exists")).into());
        }
        // Stack on the current branch tip, or trunk if on trunk.
        let base = match self.current_branch().await? {
            Some(b) => self.branch_tip(&b).await?,
            None => self.trunk_anchor().await?.1,
        };
        let name_cl = name.to_string();
        self.vcs.transaction(&mut |tx| {
            let at = tx.new_child(&base)?; // fresh empty @ child of base
            tx.create_bookmark(&name_cl, &at)?; // bookmark on the empty @ (rides real commits later)
            Ok(())
        })?;
        let entry = self.state.branch_mut(name);
        entry.tracked = tracked;
        self.state.save(&self.root)?;
        report.note(format!(
            "created {} branch '{name}'",
            if tracked { "tracked" } else { "untracked" }
        ));
        Ok(report)
    }

    /// `jjk checkout NAME` — switch to an existing branch, carrying any uncommitted changes along.
    pub async fn checkout(&mut self, name: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let tip = if name == self.state.config.trunk {
            self.trunk_anchor().await?.1
        } else {
            self.branch_tip(name).await?
        };
        self.switch_onto(&tip, &mut report).await?;
        report.note(format!("switched to {name}"));
        Ok(report)
    }

    /// Reposition the working copy onto `tip` (a branch tip / trunk anchor), git-style.
    ///
    /// With a **clean** working copy this is a fresh empty `@` (`jj new`) — the cheap, common switch.
    /// With **uncommitted changes** it *carries* them: it rebases the working-copy commit onto `tip`
    /// (`jj rebase -s @`), so the edits stay uncommitted but now sit on the target — exactly like
    /// `git checkout` keeping a dirty tree. The old behavior (`jj new`) silently stranded those
    /// changes as a nameless, off-disk commit on the previous branch, which read as data loss.
    ///
    /// If the carry conflicts with the target, jj writes conflict markers into `@` (it never drops
    /// the changes); we record that in `report.conflicts` so the caller opens the resolve flow, and
    /// `jjk undo` still reverts the whole switch (a checkpoint was taken before the command).
    async fn switch_onto(&mut self, tip: &ChangeId, report: &mut Report) -> Result<()> {
        // Snapshot so on-disk edits are visible before we decide clean-vs-dirty.
        let wc = self.vcs.snapshot().await?;
        // A bookmark riding `@` means an empty branch just created (its bookmark sits on the working
        // copy); rebasing `@` would drag that bookmark onto the target and tangle the stack. That
        // window only holds a clean `@`, so `is_empty` already routes it to the safe `jj new` path —
        // but guard explicitly in case changes were made before the first commit.
        let carrying = !wc.is_empty && !wc.local_bookmarks.iter().any(|b| self.is_stack_bookmark(b));
        let wc_id = wc.change_id.clone();
        let tip = tip.clone();
        self.vcs.transaction(&mut |tx| {
            if carrying {
                tx.rebase(&wc_id, &tip)?; // `@` is a leaf, so `-s @` moves only the working copy
            } else {
                tx.new_child(&tip)?; // clean switch (or empty branch riding `@`): fresh empty child
            }
            Ok(())
        })?;
        if carrying {
            let conflicted = self.vcs.conflicted_paths(&wc_id).await.unwrap_or_default();
            if conflicted.is_empty() {
                report.note("brought your uncommitted changes along");
            } else {
                let n = conflicted.len();
                report.note(format!(
                    "brought your uncommitted changes along — {n} {} with the target",
                    plural(n, "file now conflicts", "files now conflict")
                ));
                report.conflicts.push(format!(
                    "working copy: {n} {}",
                    plural(n, "file needs resolution", "files need resolution")
                ));
            }
        }
        Ok(())
    }

    /// Navigation: reposition `@` onto a target branch's tip.
    pub async fn navigate(&mut self, dir: NavDir) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;
        let target: String = match dir {
            NavDir::Up => {
                let cur = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
                stack
                    .upstack(&cur)
                    .map(|b| b.name.clone())
                    .ok_or(JjkError::StackEnd("top"))?
            }
            NavDir::Down => match &stack.current {
                Some(cur) => match stack.downstack(cur) {
                    Some(b) => b.name.clone(),
                    None => stack.trunk_name.clone(),
                },
                None => return Err(JjkError::StackEnd("bottom").into()),
            },
            NavDir::Top => stack
                .top()
                .map(|b| b.name.clone())
                .ok_or(JjkError::StackEnd("top"))?,
            NavDir::Bottom => stack
                .bottom()
                .map(|b| b.name.clone())
                .ok_or(JjkError::StackEnd("bottom"))?,
        };
        let tip = if target == stack.trunk_name {
            stack.trunk.clone()
        } else {
            self.branch_tip(&target).await?
        };
        self.switch_onto(&tip, &mut report).await?;
        report.note(format!("moved to {target}"));
        Ok(report)
    }

    /// `jjk restack` — ensure each upstack branch's first commit is parented on its downstack
    /// branch's tip. Usually a **no-op** (jj already auto-rebased on every rewrite); this repairs
    /// any drift and reports what moved.
    pub async fn restack(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;

        // (first_commit_to_rebase, destination). Computed against stable change ids.
        let mut actions: Vec<(ChangeId, ChangeId)> = Vec::new();
        let mut expected_parent = stack.trunk.clone();
        for b in &stack.branches {
            if let Some(first) = b.commits.first() {
                if !first.parents.contains(&expected_parent) {
                    actions.push((first.change_id.clone(), expected_parent.clone()));
                }
            }
            expected_parent = b.tip.clone();
        }

        if actions.is_empty() {
            report.note("stack already up to date (jj auto-rebases; nothing to do)");
        } else {
            self.vcs.transaction(&mut |tx| {
                for (src, dest) in &actions {
                    tx.rebase(src, dest)?;
                }
                Ok(())
            })?;
            let n = actions.len();
            report.note(format!("restacked {n} {}", plural(n, "branch", "branches")));
        }
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk track [NAME]` / `jjk untrack [NAME]` — toggle stack-tracking. Defaults to the current
    /// branch. Tracking governs PR intent (only tracked branches are submitted in Phase 3).
    pub async fn set_tracked(&mut self, name: Option<&str>, tracked: bool) -> Result<Report> {
        let mut report = Report::default();
        let name = match name {
            Some(n) => n.to_string(),
            None => self.current_branch().await?.ok_or(JjkError::NotOnBranch)?,
        };
        if name == self.state.config.trunk {
            return Err(JjkError::IsTrunk(name).into());
        }
        if !self.vcs.bookmarks().await?.iter().any(|b| b.name == name) {
            return Err(JjkError::UnknownBranch(name).into());
        }
        self.state.branch_mut(&name).tracked = tracked;
        self.state.save(&self.root)?;
        report.note(format!(
            "{} '{name}'",
            if tracked { "tracking" } else { "untracking" }
        ));
        Ok(report)
    }

    /// `jjk branch delete NAME` — drop the branch and heal the gap: abandon its commit range so the
    /// upstack auto-reconnects to NAME's parent (downstack branch tip or trunk). (PR close: Phase 3.)
    pub async fn branch_delete(&mut self, name: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        if name == self.state.config.trunk {
            return Err(JjkError::IsTrunk(name.to_string()).into());
        }
        let stack = self.derive_stack().await?;
        let branch = stack
            .branch(name)
            .ok_or_else(|| JjkError::UnknownBranch(name.to_string()))?;
        let range: Vec<ChangeId> = branch.commits.iter().map(|c| c.change_id.clone()).collect();
        let had_upstack = stack.upstack(name).is_some();

        self.vcs.transaction(&mut |tx| {
            tx.abandon(&range)?; // abandons the range (deletes the bookmark) + auto-rebases upstack
            Ok(())
        })?;

        // Defensive: if a bookmark somehow survived (e.g. it wasn't on the abandoned tip), drop it.
        if self.vcs.bookmarks().await?.iter().any(|b| b.name == name) {
            let n = name.to_string();
            self.vcs.transaction(&mut |tx| tx.delete_bookmark(&n))?;
        }

        self.state.branches.remove(name);
        self.state.save(&self.root)?;
        report.note(format!("deleted branch '{name}'"));
        if had_upstack {
            report.note("upstack reconnected to its parent");
        }
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    // ---------------------------------------------------------------- branch restructuring

    /// `jjk trunk` — switch to the trunk branch.
    pub async fn trunk_checkout(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let (_, id) = self.trunk_anchor().await?;
        self.switch_onto(&id, &mut report).await?;
        report.note(format!("switched to trunk '{}'", self.state.config.trunk));
        Ok(report)
    }

    /// `jjk branch onto <target>` — move the current branch and everything stacked above it onto a
    /// new base (`target` branch's tip, or trunk). The upstack rides along (jj auto-rebases).
    pub async fn branch_onto(&mut self, target: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;
        let branch = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
        if target == branch {
            return Err(JjkError::Msg("cannot move a branch onto itself".into()).into());
        }
        let first = stack
            .branch(&branch)
            .and_then(|b| b.commits.first())
            .map(|c| c.change_id.clone())
            .ok_or_else(|| JjkError::UnknownBranch(branch.clone()))?;
        let dest = if target == stack.trunk_name {
            stack.trunk.clone()
        } else {
            self.branch_tip(target).await?
        };
        self.vcs.transaction(&mut |tx| {
            tx.rebase(&first, &dest)?;
            Ok(())
        })?;
        report.note(format!("moved '{branch}' onto '{target}'"));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk branch rename [old] <new>` — rename a branch (default: the current one), preserving its
    /// PR mapping in state.
    pub async fn branch_rename(&mut self, old: Option<&str>, new: &str) -> Result<Report> {
        let mut report = Report::default();
        let old = match old {
            Some(o) => o.to_string(),
            None => self.current_branch().await?.ok_or(JjkError::NotOnBranch)?,
        };
        if old == self.state.config.trunk {
            return Err(JjkError::IsTrunk(old).into());
        }
        if self.resolve_bookmark(&old).await?.is_none() {
            return Err(JjkError::UnknownBranch(old).into());
        }
        if self.resolve_bookmark(new).await?.is_some() {
            return Err(JjkError::Msg(format!("branch '{new}' already exists")).into());
        }
        let (o, n) = (old.clone(), new.to_string());
        self.vcs.transaction(&mut |tx| tx.rename_bookmark(&o, &n))?;
        if let Some(entry) = self.state.branches.remove(&old) {
            self.state.branches.insert(new.to_string(), entry);
        }
        self.state.save(&self.root)?;
        report.note(format!("renamed '{old}' to '{new}'"));
        Ok(report)
    }

    /// `jjk branch diff` — show the current branch's diff against its base (downstack tip / trunk).
    pub async fn branch_diff(&self) -> Result<String> {
        let stack = self.derive_stack().await?;
        let branch = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
        let tip = stack
            .branch(&branch)
            .map(|b| b.tip.clone())
            .ok_or_else(|| JjkError::UnknownBranch(branch.clone()))?;
        // Use change ids as revset endpoints (always valid, even when the trunk bookmark is absent).
        let base = stack
            .downstack(&branch)
            .map(|d| d.tip.clone())
            .unwrap_or_else(|| stack.trunk.clone());
        self.vcs
            .diff(&format!("{}..{}", base.as_str(), tip.as_str()))
            .await
    }

    /// `jjk branch squash [-m M]` — collapse all of the current branch's commits into one.
    pub async fn branch_squash(&mut self, message: Option<&str>) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;
        let branch = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
        let b = stack
            .branch(&branch)
            .ok_or_else(|| JjkError::UnknownBranch(branch.clone()))?;
        if b.commit_count() <= 1 {
            report.note(format!("'{branch}' already has a single commit"));
            return Ok(report);
        }
        let first = b.commits[0].change_id.clone();
        // Squash every commit above the first (first..tip, by change id) into the first.
        let range = format!("{}..{}", first.as_str(), b.tip.as_str());
        let msg = message.map(|s| s.to_string());
        self.vcs.transaction(&mut |tx| {
            tx.squash_revset(&range, &first)?;
            if let Some(m) = &msg {
                tx.describe(&first, m)?;
            }
            Ok(())
        })?;
        report.note(format!("squashed '{branch}' into one commit"));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk branch fold` — fold the current branch into its downstack base: the base's bookmark
    /// advances over the current branch's commits and the current bookmark is dropped (one fewer
    /// PR; the upstack reconnects to the base).
    pub async fn branch_fold(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;
        let branch = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
        let tip = stack
            .branch(&branch)
            .map(|b| b.tip.clone())
            .ok_or_else(|| JjkError::UnknownBranch(branch.clone()))?;
        let base = stack.downstack(&branch).map(|d| d.name.clone()).ok_or_else(|| {
            JjkError::Msg(format!(
                "'{branch}' sits on trunk; nothing to fold into (folding into trunk isn't allowed)"
            ))
        })?;
        let (bname, brn) = (base.clone(), branch.clone());
        self.vcs.transaction(&mut |tx| {
            tx.set_bookmark(&bname, &tip)?; // base absorbs the branch's commits
            tx.delete_bookmark(&brn)?;
            Ok(())
        })?;
        self.state.branches.remove(&branch);
        self.state.save(&self.root)?;
        report.note(format!("folded '{branch}' into '{base}'"));
        Ok(report)
    }

    /// `jjk commit --fixup <target>` — fold the working-copy changes into `target` branch's tip (an
    /// older commit downstack); descendants auto-rebase. (`git commit --fixup` + autosquash.)
    pub async fn commit_fixup(&mut self, target: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        if target == self.state.config.trunk {
            return Err(JjkError::IsTrunk(target.to_string()).into());
        }
        let target_tip = self.branch_tip(target).await?;
        // Snapshot so on-disk edits are captured into @ before folding them down.
        let wc = self.vcs.snapshot().await?;
        if wc.is_empty {
            report.note("nothing to fix up (working copy is clean)");
            return Ok(report);
        }
        self.vcs
            .transaction(&mut |tx| tx.squash_working_into(&target_tip))?;
        report.note(format!("fixed up '{target}' with working-copy changes"));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk commit --split` — split the current branch's tip into two commits via an interactive
    /// diff editor (`jj split`); descendants auto-rebase.
    pub async fn commit_split(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let (branch, tip) = self.current_branch_tip().await?.ok_or(JjkError::NotOnBranch)?;
        self.vcs.split_interactive(&tip).await?;
        report.note(format!("split the tip of '{branch}'"));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk commit --pick <rev>` — copy a commit (e.g. from an upstack branch) onto the current
    /// branch's tip; the upstack rides along.
    pub async fn commit_pick(&mut self, rev: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let (branch, tip) = self.current_branch_tip().await?.ok_or(JjkError::NotOnBranch)?;
        let src = self
            .vcs
            .resolve(rev)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| JjkError::Msg(format!("no commit matches '{rev}'")))?
            .change_id;
        let tip_cl = tip.clone();
        self.vcs
            .transaction(&mut |tx| tx.duplicate_after(&src, &tip_cl))?;
        // After --insert-after, the copy is the sole new child of the old tip.
        let dup = self
            .vcs
            .resolve(&format!("children({})", tip.as_str()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| JjkError::Msg("could not locate the picked commit".into()))?
            .change_id;
        let bname = branch.clone();
        self.vcs
            .transaction(&mut |tx| tx.set_bookmark(&bname, &dup))?;
        report.note(format!("picked {} onto '{branch}'", src.short()));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk branch split <new> <commit>` — split the current branch at `commit`: a new tracked
    /// branch `<new>` takes the commits up to and including `commit`; the current branch keeps the
    /// rest. (`commit` must be within the branch and below its tip.)
    pub async fn branch_split(&mut self, new_name: &str, at: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;
        let branch = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
        let b = stack
            .branch(&branch)
            .ok_or_else(|| JjkError::UnknownBranch(branch.clone()))?;
        if self.resolve_bookmark(new_name).await?.is_some() {
            return Err(JjkError::Msg(format!("branch '{new_name}' already exists")).into());
        }
        let at_id = self
            .vcs
            .resolve(at)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| JjkError::Msg(format!("no commit matches '{at}'")))?
            .change_id;
        if at_id == b.tip || !b.commits.iter().any(|c| c.change_id == at_id) {
            return Err(JjkError::Msg(format!(
                "split point must be a commit within '{branch}', below its tip"
            ))
            .into());
        }
        let name = new_name.to_string();
        self.vcs
            .transaction(&mut |tx| tx.create_bookmark(&name, &at_id))?;
        self.state.branch_mut(new_name).tracked = true;
        self.state.save(&self.root)?;
        report.note(format!(
            "split '{branch}' at {}: '{new_name}' holds the lower commits",
            at_id.short()
        ));
        Ok(report)
    }
}
