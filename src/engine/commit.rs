//! Working-copy commit verbs: commit / amend / fixup / split / pick.

use super::*;

impl Engine {
    /// `jjk commit -m M` — commit the whole working copy (ARCH §3.3).
    pub async fn commit(&mut self, message: &str) -> Result<Report> {
        self.commit_scoped(message, &CommitScope::All).await
    }

    /// `jjk commit` with an explicit scope: all, specific paths (incl. git-staged), or interactive.
    /// Anything outside the scope stays uncommitted in `@`.
    pub async fn commit_scoped(&mut self, message: &str, scope: &CommitScope) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let (branch, old_tip) = self.current_branch_tip().await?.ok_or(JjkError::NotOnBranch)?;
        let upstack_firsts = self.upstack_first_commits(&old_tip).await?;

        let branch_cl = branch.clone();
        let mut new_tip: Option<ChangeId> = None;
        self.vcs.transaction(&mut |tx| {
            let c = tx.finalize_working_copy_scoped(message, scope)?; // C = @-, fresh @ keeps the rest
            tx.set_bookmark(&branch_cl, &c)?; // advance bookmark (no-op if already there)
            for f in &upstack_firsts {
                tx.rebase(f, &c)?; // restack upstack onto C
            }
            new_tip = Some(c);
            Ok(())
        })?;

        if let Some(c) = &new_tip {
            self.state.branch_mut(&branch).change_id = Some(c.0.clone());
            self.state.save(&self.root)?;
        }
        if !upstack_firsts.is_empty() {
            let n = upstack_firsts.len();
            report.note(format!("restacked {n} upstack {}", plural(n, "branch", "branches")));
        }
        report.note(format!("committed to {branch}"));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// `jjk commit --amend [-m M]` — squash working changes into the branch tip; descendants
    /// auto-rebase (ARCH §3.3 amend).
    pub async fn commit_amend(&mut self, message: Option<&str>) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let (branch, tip) = self.current_branch_tip().await?.ok_or(JjkError::NotOnBranch)?;
        let msg = message.map(|s| s.to_string());
        self.vcs.transaction(&mut |tx| {
            tx.squash_working_into(&tip)?;
            if let Some(m) = &msg {
                tx.describe(&tip, m)?;
            }
            Ok(())
        })?;
        report.note(format!("amended {branch}"));
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }
}
