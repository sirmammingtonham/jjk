//! Conflict resolution sessions (resolve --continue/--abort) and undo checkpoints.

use super::*;

impl Engine {
    /// `jjk resolve` — enter conflict resolution: jump to the lowest conflicted change so you can
    /// fix the markers in place. With `interactive`, drive jj's merge tool on it instead (focused,
    /// one file at a time) and fold the result straight into the advance step. Pairs with
    /// `resolve --continue`/`--abort`.
    pub async fn resolve(&mut self, interactive: bool) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let stack = self.derive_stack().await?;
        let Some(target) = lowest_conflict(&stack) else {
            report.note("no conflicts to resolve");
            self.clear_resolve_session();
            return Ok(report);
        };
        // Standalone entry (conflicts that didn't come from a tracked command, e.g. a raw jj edit):
        // open a session so we can still return you home and offer `--abort`. A session left by the
        // originating command (e.g. sync) is kept as-is — it carries the resume action.
        if !self.has_resolve_session() {
            let home = self.current_branch().await?;
            let presync = self.load_checkpoints().last().cloned();
            self.save_resolve_session(&ResolveSession {
                home,
                resume_cmd: None,
                presync,
                pushed: Vec::new(),
            })?;
        }
        self.vcs.transaction(&mut |tx| tx.edit(&target))?;
        if interactive {
            self.vcs.resolve_with_merge_tool(&target).await?;
            self.advance_or_finish(&mut report).await?;
        } else {
            self.announce_conflict(&stack, &target, &mut report).await;
        }
        Ok(report)
    }

    /// `jjk resolve --continue` — capture the fix to the current conflict, then advance to the next
    /// one or finish (return home and resume the originating command) when the stack is clean.
    pub async fn resolve_continue(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.advance_or_finish(&mut report).await?;
        Ok(report)
    }

    /// Snapshot the fix to the change under `@`, then advance to the next conflict or finish.
    /// Shared by `resolve --continue` and the interactive resolver.
    async fn advance_or_finish(&mut self, report: &mut Report) -> Result<()> {
        let Some(session) = self.load_resolve_session() else {
            report.note("no conflict resolution in progress — run `jjk resolve` to start");
            return Ok(());
        };
        // Capture the on-disk edits into the change under `@` (ensure_fresh doesn't snapshot with a
        // single workspace, so do it explicitly); jj then propagates the fix to descendants.
        let wc = self.vcs.snapshot().await?;
        if wc.has_conflict {
            report.note(format!("{} still has conflicts:", wc.change_id.short()));
            for p in self.vcs.conflicted_paths(&wc.change_id).await.unwrap_or_default() {
                report.note(format!("    {p}"));
            }
            report.note("remove all conflict markers, then run `jjk resolve --continue`");
            return Ok(());
        }
        let stack = self.derive_stack().await?;
        if let Some(next) = lowest_conflict(&stack) {
            self.vcs.transaction(&mut |tx| tx.edit(&next))?;
            report.note("✓ resolved");
            self.announce_conflict(&stack, &next, report).await;
            return Ok(());
        }
        self.finish_resolve(session, report).await
    }

    /// `jjk resolve --abort` — discard the resolution and roll back to the pre-sync state via an
    /// op-log rewind, the way `git rebase --abort` would.
    pub async fn resolve_abort(&mut self) -> Result<Report> {
        let mut report = Report::default();
        let Some(session) = self.load_resolve_session() else {
            report.note("no conflict resolution in progress");
            return Ok(report);
        };
        // Restore the pre-command checkpoint (op + state.toml), exactly like `undo`; fall back to a
        // single `jj undo` if no checkpoint was recorded.
        let msg = match &session.presync {
            Some(ckpt) => {
                let msg = self.vcs.restore_op(&ckpt.op_id).await?;
                if !ckpt.state.is_empty() {
                    std::fs::write(State::path_for(&self.root), &ckpt.state)?;
                    self.state = State::load(&self.root)?;
                }
                msg
            }
            None => self.vcs.undo().await?,
        };
        for line in msg.lines() {
            report.note(line.to_string());
        }
        self.clear_resolve_session();
        report.note("aborted — restored the state from before the sync");
        if !session.pushed.is_empty() {
            report.note(format!(
                "note: {} already pushed and still on the remote (run `jjk sync` to reconcile)",
                session.pushed.join(", ")
            ));
        }
        Ok(report)
    }

    /// Finish a resolve session: return to the branch you started on (falling back to trunk if it
    /// was merged away during the sync), clear the session, and resume the originating command.
    async fn finish_resolve(&mut self, session: ResolveSession, report: &mut Report) -> Result<()> {
        report.note("✓ all conflicts resolved");
        let home_ok = match &session.home {
            Some(b) => self.resolve_bookmark(b).await?.is_some(),
            None => false,
        };
        let target = if home_ok {
            session.home.clone().expect("home present when home_ok")
        } else {
            self.state.config.trunk.clone()
        };
        let tip = if home_ok {
            self.branch_tip(&target).await?
        } else {
            self.trunk_anchor().await?.1
        };
        self.vcs.transaction(&mut |tx| {
            tx.new_child(&tip)?;
            Ok(())
        })?;
        if let Some(b) = &session.home {
            if !home_ok {
                report.note(format!("'{b}' is gone (merged during sync)"));
            }
        }
        report.note(format!("back on {target} with a clean working copy"));
        self.clear_resolve_session();
        // Auto-resume the command that hit the conflict (e.g. push the now-resolved branches).
        match session.resume_cmd {
            Some(ResumeCmd::Sync { push }) => {
                report.note("resuming sync…");
                let sub = self.sync(push).await?;
                report.conflicts.extend(sub.conflicts);
            }
            None => {}
        }
        Ok(())
    }

    /// Narrate the current conflict step: how many changes remain, which one is now open for
    /// editing (and on which branch), its conflicted files, and the next command to run.
    async fn announce_conflict(&self, stack: &Stack, target: &ChangeId, report: &mut Report) {
        let remaining = stack
            .branches
            .iter()
            .flat_map(|b| b.commits.iter())
            .filter(|c| c.has_conflict)
            .count();
        let branch = branch_of(stack, target).unwrap_or("the stack");
        report.note(format!(
            "resolving conflicts — {remaining} {} remaining (editing {} on '{branch}')",
            plural(remaining, "change", "changes"),
            target.short()
        ));
        report.note("fix the conflict markers in these files — the rest of the change is already correct:");
        for p in self.vcs.conflicted_paths(target).await.unwrap_or_default() {
            report.note(format!("    {p}"));
        }
        report.note(
            "then `jjk resolve --continue`  (or `--interactive` to use your merge tool, `--abort` to bail)",
        );
    }

    // ---------------------------------------------------------------- resolve session

    fn resolve_session_path(&self) -> PathBuf {
        self.root.join(".jj").join("jjk").join("resolve.json")
    }

    fn load_resolve_session(&self) -> Option<ResolveSession> {
        std::fs::read_to_string(self.resolve_session_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
    }

    fn save_resolve_session(&self, s: &ResolveSession) -> Result<()> {
        let path = self.resolve_session_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string(s)?)?;
        Ok(())
    }

    fn clear_resolve_session(&self) {
        let _ = std::fs::remove_file(self.resolve_session_path());
    }

    /// Whether a conflict-resolution session is in progress (drives the `status` breadcrumb).
    pub fn has_resolve_session(&self) -> bool {
        self.resolve_session_path().exists()
    }

    /// Open a guided resolve session if one isn't already active — called after a command leaves the
    /// stack conflicted. Records where to return (the current branch), what to resume, and the
    /// pre-command checkpoint for `--abort`. No-op when a session already exists. Best-effort.
    pub async fn begin_resolve_session_if_absent(
        &self,
        resume: Option<ResumeCmd>,
        pushed: Vec<String>,
    ) -> Result<()> {
        if self.has_resolve_session() {
            return Ok(());
        }
        let home = self.current_branch().await?;
        let presync = self.load_checkpoints().last().cloned();
        self.save_resolve_session(&ResolveSession {
            home,
            resume_cmd: resume,
            presync,
            pushed,
        })
    }

    /// `jjk undo` — expose jj's op-log undo.
    /// Record a checkpoint before a mutating command runs, so `jjk undo` can revert the *whole*
    /// command in one step. A single jjk command maps to several jj operations (e.g. `commit` does
    /// `jj commit` + `jj bookmark set` + restacks); plain `jj undo` reverts only the last of them,
    /// which leaves the repo half-changed. We snapshot the head op id (and jjk's own state.toml)
    /// here and `jj op restore` to it in [`undo`]. Best-effort: a failure must never block the real
    /// command, so callers ignore the error (undo then falls back to a single `jj undo`).
    pub async fn checkpoint(&self) -> Result<()> {
        let op_id = self.vcs.current_op_id().await?;
        let state = std::fs::read_to_string(State::path_for(&self.root)).unwrap_or_default();
        let mut stack = self.load_checkpoints();
        stack.push(Checkpoint { op_id, state });
        // Keep the log bounded; old checkpoints fall off the bottom.
        let len = stack.len();
        if len > MAX_CHECKPOINTS {
            stack.drain(0..len - MAX_CHECKPOINTS);
        }
        self.save_checkpoints(&stack)
    }

    fn undo_log_path(&self) -> PathBuf {
        self.root.join(".jj").join("jjk").join("undo.json")
    }

    fn load_checkpoints(&self) -> Vec<Checkpoint> {
        std::fs::read_to_string(self.undo_log_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save_checkpoints(&self, stack: &[Checkpoint]) -> Result<()> {
        let path = self.undo_log_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string(stack)?)?;
        Ok(())
    }

    /// `jjk undo` — revert the last jjk command as one unit. Pops the most recent checkpoint and
    /// `jj op restore`s to it (also restoring jjk's state.toml). With no checkpoint recorded (e.g.
    /// the change predates this feature), falls back to a single `jj undo`.
    pub async fn undo(&mut self) -> Result<Report> {
        let mut report = Report::default();
        let mut stack = self.load_checkpoints();
        let msg = match stack.pop() {
            Some(ckpt) => {
                let msg = self.vcs.restore_op(&ckpt.op_id).await?;
                // Restore jjk's own state alongside the jj repo so the two don't drift.
                if !ckpt.state.is_empty() {
                    std::fs::write(State::path_for(&self.root), &ckpt.state)?;
                    self.state = State::load(&self.root)?;
                }
                self.save_checkpoints(&stack)?;
                msg
            }
            None => self.vcs.undo().await?,
        };
        for line in msg.lines() {
            report.note(line.to_string());
        }
        Ok(report)
    }
}
