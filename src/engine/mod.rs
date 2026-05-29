//! The engine: each verb → an ordered plan of (VCS ops, state updates, forge ops). Depends only on
//! the `Vcs`/`Forge` traits and `model` types — never on a concrete backend.

pub mod stack;

use crate::error::{JjkError, Result};
use crate::forge::Forge;
use crate::model::{ChangeId, CommitInfo};
use crate::state::State;
use crate::vcs::{PushOpts, Vcs};
use stack::{Branch, Stack};

use std::path::{Path, PathBuf};

/// User-facing result of a command: notes to print + any conflicts surfaced (never aborts; ARCH D4).
#[derive(Debug, Default)]
pub struct Report {
    pub notes: Vec<String>,
    pub conflicts: Vec<String>,
}

impl Report {
    pub fn note(&mut self, s: impl Into<String>) {
        self.notes.push(s.into());
    }
}

pub struct Engine {
    root: PathBuf,
    vcs: Box<dyn Vcs>,
    #[allow(dead_code)]
    forge: Option<Box<dyn Forge>>,
    state: State,
}

impl Engine {
    pub fn new(root: PathBuf, vcs: Box<dyn Vcs>, forge: Option<Box<dyn Forge>>, state: State) -> Self {
        Self {
            root,
            vcs,
            forge,
            state,
        }
    }

    pub fn vcs(&self) -> &dyn Vcs {
        self.vcs.as_ref()
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---------------------------------------------------------------- repo setup

    /// `jjk repo init`: init colocated, detect/store trunk + remote, write state.
    pub fn repo_init(dir: &Path, trunk: Option<String>, remote: Option<String>) -> Result<Report> {
        use crate::vcs::jj_cli::JjCli;
        let mut report = Report::default();
        let already = dir.join(".jj").join("jjk").join("state.toml").exists();
        if already {
            return Err(JjkError::AlreadyInitialized.into());
        }
        // Reuse an existing colocated jj repo, or create one.
        let vcs = if dir.join(".jj").exists() {
            report.note("using existing jj repo");
            JjCli::new(dir)
        } else {
            report.note("initialized colocated jj repo (jj git init --colocate)");
            JjCli::init_colocated(dir)?
        };

        let remote = match remote {
            Some(r) => r,
            None => vcs.remotes()?.into_iter().next().unwrap_or_else(|| "origin".to_string()),
        };
        // Trunk detection: prefer an explicit name; else a `main`/`master` bookmark; else "main".
        let trunk = match trunk {
            Some(t) => t,
            None => {
                let bms = vcs.bookmarks()?;
                ["main", "master", "trunk"]
                    .into_iter()
                    .find(|cand| bms.iter().any(|b| b.name == *cand))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "main".to_string())
            }
        };
        report.note(format!("trunk = {trunk}, remote = {remote}"));
        let state = State::new(trunk, remote);
        state.save(dir)?;
        report.note(format!("wrote {}", State::path_for(dir).display()));
        Ok(report)
    }

    /// Open an existing jjk repo by walking up from `cwd` to find `.jj`.
    pub fn open(cwd: &Path) -> Result<Engine> {
        use crate::vcs::jj_cli::JjCli;
        let root = find_repo_root(cwd).ok_or(JjkError::NotInitialized)?;
        let state = State::load(&root)?;
        let vcs: Box<dyn Vcs> = match state.config.vcs_backend.as_str() {
            "jj_cli" => Box::new(JjCli::new(&root)),
            other => {
                return Err(JjkError::Msg(format!(
                    "unknown vcs backend '{other}' (only 'jj_cli' is available)"
                ))
                .into())
            }
        };
        Ok(Engine::new(root, vcs, None, state))
    }

    // ---------------------------------------------------------------- derivation

    /// Trunk anchor as `(revset, change_id)`. Prefers the stored trunk bookmark (since `trunk()`
    /// falls back to root() with no remote — JJ_NOTES §8); falls back to the `trunk()` revset.
    fn trunk_anchor(&self) -> Result<(String, ChangeId)> {
        let tname = self.state.config.trunk.clone();
        if let Some(bm) = self.vcs.bookmarks()?.into_iter().find(|b| b.name == tname) {
            Ok((tname, bm.target))
        } else {
            let id = self.vcs.trunk()?;
            Ok(("trunk()".to_string(), id))
        }
    }

    /// The branch the working copy currently sits on (nearest local bookmark at-or-below `@`,
    /// excluding trunk). `None` means the working copy is on trunk.
    pub fn current_branch(&self) -> Result<Option<String>> {
        let trunk = &self.state.config.trunk;
        let res = self.vcs.resolve("heads(::@ & bookmarks())")?;
        Ok(res
            .into_iter()
            .next()
            .and_then(|c| c.local_bookmarks.into_iter().find(|b| b != trunk)))
    }

    fn branch_tip(&self, name: &str) -> Result<ChangeId> {
        self.vcs
            .bookmarks()?
            .into_iter()
            .find(|b| b.name == name)
            .map(|b| b.target)
            .ok_or_else(|| JjkError::UnknownBranch(name.to_string()).into())
    }

    /// First commits of the branches directly upstack of `branch_tip` (its nearest descendant
    /// bookmarks). Used to restack the upstack after a mid-stack commit (ARCH §3.3 step 3).
    fn upstack_first_commits(&self, branch_tip: &ChangeId) -> Result<Vec<ChangeId>> {
        let tip = branch_tip.as_str();
        let near = self
            .vcs
            .resolve(&format!("roots(({tip}:: ~ {tip}) & bookmarks())"))?;
        let mut firsts = Vec::new();
        for b in near {
            let r = self
                .vcs
                .resolve(&format!("roots({tip}..{})", b.change_id.as_str()))?;
            if let Some(f) = r.into_iter().next() {
                firsts.push(f.change_id);
            }
        }
        Ok(firsts)
    }

    /// Reconstruct the stack containing `@` from jj. Bottom (nearest trunk) → top.
    pub fn derive_stack(&self) -> Result<Stack> {
        let (trunk_revset, trunk_id) = self.trunk_anchor()?;
        let current = self.current_branch()?;

        // Anchor for the "upstack" search: the current branch tip, or trunk if on trunk.
        let cur_tip_revset = match &current {
            Some(b) => b.clone(),
            None => trunk_revset.clone(),
        };

        // Topmost bookmarked tip in the stack containing the current position.
        let upmost = self.vcs.resolve(&format!(
            "heads(({cur_tip_revset}:: ~ {cur_tip_revset}) & bookmarks())"
        ))?;
        let top_id = match upmost.into_iter().next() {
            Some(c) => c.change_id,
            None => match &current {
                Some(b) => self.branch_tip(b)?,
                None => trunk_id.clone(),
            },
        };

        // Linear ancestry from trunk (exclusive) up to top (inclusive), newest-first → reverse.
        let mut commits = self
            .vcs
            .resolve(&format!("{}..{}", trunk_revset, top_id.as_str()))?;
        commits.reverse(); // bottom → top

        // Group into branches: a commit carrying a (non-trunk) local bookmark closes a branch.
        let trunk_name = self.state.config.trunk.clone();
        let mut branches: Vec<Branch> = Vec::new();
        let mut acc: Vec<CommitInfo> = Vec::new();
        for c in commits {
            acc.push(c.clone());
            if let Some(name) = c.local_bookmarks.iter().find(|b| **b != trunk_name) {
                let name = name.clone();
                let pr = self.state.pr_of(&name);
                let tracked = self.state.is_tracked(&name);
                branches.push(Branch {
                    name,
                    tip: c.change_id.clone(),
                    commits: std::mem::take(&mut acc),
                    pr,
                    tracked,
                });
            }
        }
        // Any trailing un-bookmarked commits (WIP above the top bookmark) are intentionally dropped
        // from the branch list; they are surfaced by `status`, not part of a PR range.

        Ok(Stack {
            trunk_name,
            trunk: trunk_id,
            branches,
            current,
        })
    }

    // ---------------------------------------------------------------- staleness guard

    /// Auto-recover a stale working copy before any command that reads `@` (ARCH §8/§12).
    fn ensure_fresh(&self, report: &mut Report) -> Result<()> {
        if self.vcs.is_stale()? {
            self.vcs.update_stale()?;
            report.note("recovered stale working copy (jj workspace update-stale)");
        }
        Ok(())
    }

    // ---------------------------------------------------------------- local commands

    /// `jjk commit -m M` — the commit algorithm (ARCH §3.3).
    pub fn commit(&mut self, message: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let branch = self
            .current_branch()?
            .ok_or(JjkError::NotOnBranch)?;
        let old_tip = self.branch_tip(&branch)?;
        let upstack_firsts = self.upstack_first_commits(&old_tip)?;

        let branch_cl = branch.clone();
        let mut new_tip: Option<ChangeId> = None;
        self.vcs.transaction(&mut |tx| {
            let c = tx.finalize_working_copy(message)?; // C = @-, fresh empty @ on top
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
            report.note(format!("restacked {} upstack branch(es)", upstack_firsts.len()));
        }
        report.note(format!("committed to {branch}"));
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk commit --amend [-m M]` — squash working changes into the branch tip; descendants
    /// auto-rebase (ARCH §3.3 amend).
    pub fn commit_amend(&mut self, message: Option<&str>) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let branch = self.current_branch()?.ok_or(JjkError::NotOnBranch)?;
        let tip = self.branch_tip(&branch)?;
        let msg = message.map(|s| s.to_string());
        self.vcs.transaction(&mut |tx| {
            tx.squash_working_into(&tip)?;
            if let Some(m) = &msg {
                tx.describe(&tip, m)?;
            }
            Ok(())
        })?;
        report.note(format!("amended {branch}"));
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk branch create [NAME]` (tracked) / `jjk checkout -b NAME` (untracked).
    pub fn branch_create(&mut self, name: &str, tracked: bool) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        if self.vcs.bookmarks()?.iter().any(|b| b.name == name) {
            return Err(JjkError::Msg(format!("branch '{name}' already exists")).into());
        }
        // Stack on the current branch tip, or trunk if on trunk.
        let base = match self.current_branch()? {
            Some(b) => self.branch_tip(&b)?,
            None => self.trunk_anchor()?.1,
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

    /// `jjk checkout NAME` — switch to an existing branch (always safe in jj).
    pub fn checkout(&mut self, name: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let tip = if name == self.state.config.trunk {
            self.trunk_anchor()?.1
        } else {
            self.branch_tip(name)?
        };
        self.vcs.transaction(&mut |tx| {
            tx.new_child(&tip)?;
            Ok(())
        })?;
        report.note(format!("switched to {name}"));
        Ok(report)
    }

    /// Navigation: reposition `@` onto a target branch's tip.
    pub fn navigate(&mut self, dir: NavDir) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
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
            self.branch_tip(&target)?
        };
        self.vcs.transaction(&mut |tx| {
            tx.new_child(&tip)?;
            Ok(())
        })?;
        report.note(format!("moved to {target}"));
        Ok(report)
    }

    /// `jjk undo` — expose jj's op-log undo.
    pub fn undo(&mut self) -> Result<Report> {
        let mut report = Report::default();
        let msg = self.vcs.undo()?;
        for line in msg.lines() {
            report.note(line.to_string());
        }
        Ok(report)
    }

    // ---------------------------------------------------------------- conflict surfacing

    /// Append a git-flavored conflict summary for the current stack (ARCH §9 D4).
    fn collect_conflicts(&self, report: &mut Report) -> Result<()> {
        let stack = self.derive_stack()?;
        for b in &stack.branches {
            if b.has_conflict() {
                report.conflicts.push(format!(
                    "{}: {} change(s) need resolution",
                    b.name,
                    b.commits.iter().filter(|c| c.has_conflict).count()
                ));
            }
        }
        Ok(())
    }

    /// Whether the current stack has any conflicts (for exit-code / messaging).
    pub fn has_conflicts(&self) -> Result<bool> {
        Ok(self.derive_stack()?.branches.iter().any(|b| b.has_conflict()))
    }

    // ---------------------------------------------------------------- remote passthrough (Phase 3 wires forge)

    pub fn fetch(&self) -> Result<Report> {
        let mut report = Report::default();
        self.vcs.fetch(&self.state.config.remote)?;
        report.note(format!("fetched {}", self.state.config.remote));
        Ok(report)
    }

    pub fn push_current(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let branch = self.current_branch()?.ok_or(JjkError::NotOnBranch)?;
        self.vcs
            .push(&self.state.config.remote, &branch, PushOpts::default())?;
        report.note(format!("pushed {branch}"));
        Ok(report)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum NavDir {
    Up,
    Down,
    Top,
    Bottom,
}

/// Walk up from `cwd` to find the directory containing `.jj`.
fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        if dir.join(".jj").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}
