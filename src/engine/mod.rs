//! The engine: each verb → an ordered plan of (VCS ops, state updates, forge ops). Depends only on
//! the `Vcs`/`Forge` traits and `model` types — never on a concrete backend.

pub mod stack;

use crate::error::{JjkError, Result};
use crate::forge::Forge;
use crate::model::{ChangeId, CommitInfo, PrRef, PrState};
use crate::prompt::{AutoFill, PrDraft, Prompter};
use crate::state::State;
use crate::text::plural;
use crate::vcs::{CommitScope, PushOpts, Vcs};
use stack::{Branch, Stack};

use std::path::{Path, PathBuf};

/// Revset for bookmarks that participate in the stack — excludes internal `jjk/stash/*` bookmarks.
const STACK_BOOKMARKS: &str = r#"(bookmarks() ~ bookmarks(glob:"jjk/stash/*"))"#;

/// How many undo checkpoints to retain (one per mutating jjk command).
const MAX_CHECKPOINTS: usize = 100;

/// A single undo point: the jj operation to restore to, plus a snapshot of jjk's state.toml so the
/// two stay in sync. Stored as a stack in `.jj/jjk/undo.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    op_id: String,
    state: String,
}

/// User-facing result of a command: notes to print + any conflicts surfaced (never aborts; ARCH D4).
#[derive(Debug, Default)]
pub struct Report {
    pub notes: Vec<String>,
    pub conflicts: Vec<String>,
}

impl Report {
    pub fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        // Stream each note as it happens (stdout is line-buffered, so it flushes per line) rather
        // than buffering until the command ends — long commands like sync/submit then show
        // per-branch progress instead of dumping everything at once. Still recorded for inspection.
        println!("{s}");
        self.notes.push(s);
    }
}

pub struct Engine {
    root: PathBuf,
    vcs: Box<dyn Vcs>,
    state: State,
    forge_backend: String,
    /// Lazily built: forge construction queries the remote, so local commands (ls/commit/…) that
    /// never touch the forge don't pay for it.
    forge: std::cell::OnceCell<Box<dyn Forge>>,
    /// How `submit` gathers details for a new PR. Defaults to [`AutoFill`] (non-interactive); `main`
    /// installs a terminal prompter when stdio is a tty and `--fill` wasn't passed.
    prompter: Box<dyn Prompter>,
}

impl Engine {
    pub fn new(
        root: PathBuf,
        vcs: Box<dyn Vcs>,
        forge_backend: impl Into<String>,
        state: State,
    ) -> Self {
        Self {
            root,
            vcs,
            state,
            forge_backend: forge_backend.into(),
            forge: std::cell::OnceCell::new(),
            prompter: Box::new(AutoFill),
        }
    }

    /// Install the prompter `submit` uses to gather details for new PRs (terminal prompter from
    /// `main`; tests/CI keep the [`AutoFill`] default).
    pub fn set_prompter(&mut self, prompter: Box<dyn Prompter>) {
        self.prompter = prompter;
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
    ///
    /// Multi-workspace aware: the VCS is rooted at the **workspace** the user is in (so `@` is
    /// that workspace's working copy — per-workspace current branch, ARCH §8/D3), while shared
    /// `state.toml` is loaded from the **main** repo (resolved via `.jj/repo`).
    pub fn open(cwd: &Path) -> Result<Engine> {
        use crate::vcs::jj_cli::JjCli;
        let ws_root = find_workspace_root(cwd).ok_or(JjkError::NotInitialized)?;
        let main_root = main_root_of(&ws_root)?;
        let state = State::load(&main_root)?;
        let vcs: Box<dyn Vcs> = match state.config.vcs_backend.as_str() {
            "jj_cli" => Box::new(JjCli::new(&ws_root)),
            other => {
                return Err(JjkError::Msg(format!(
                    "unknown vcs backend '{other}' (only 'jj_cli' is available)"
                ))
                .into())
            }
        };
        let forge_backend = state.config.forge_backend.clone();
        Ok(Engine::new(main_root, vcs, forge_backend, state))
    }

    /// Replace the forge adapter (used by tests to inject a fake).
    pub fn set_forge(&mut self, forge: Box<dyn Forge>) {
        self.forge = std::cell::OnceCell::from(forge);
    }

    /// The forge, built on first use (querying the remote only when a forge command runs).
    fn forge(&self) -> Result<&dyn Forge> {
        if self.forge.get().is_none() {
            let f = self.build_forge()?;
            let _ = self.forge.set(f);
        }
        Ok(self.forge.get().expect("just initialized").as_ref())
    }

    fn build_forge(&self) -> Result<Box<dyn Forge>> {
        use crate::forge::gh_cli::GhCli;
        match self.forge_backend.as_str() {
            "gh_cli" => {
                let slug = self
                    .vcs
                    .remote_url(&self.state.config.remote)?
                    .and_then(|u| GhCli::slug_from_url(&u));
                Ok(Box::new(GhCli::new(slug)))
            }
            other => Err(JjkError::Msg(format!(
                "unknown forge backend '{other}' (only 'gh_cli' is available)"
            ))
            .into()),
        }
    }

    // ---------------------------------------------------------------- derivation

    /// Trunk anchor as `(revset, change_id)`. Prefers the stored trunk bookmark (since `trunk()`
    /// falls back to root() with no remote — JJ_NOTES §8); falls back to the `trunk()` revset.
    fn trunk_anchor(&self) -> Result<(String, ChangeId)> {
        let tname = self.state.config.trunk.clone();
        if let Some(id) = self.resolve_bookmark(&tname)? {
            Ok((tname, id))
        } else {
            let id = self.vcs.trunk()?;
            Ok(("trunk()".to_string(), id))
        }
    }

    /// Resolve a single local bookmark to its target change id, if it exists. Targeted query
    /// (`bookmarks(exact:..)`) rather than scanning every bookmark in the repo.
    fn resolve_bookmark(&self, name: &str) -> Result<Option<ChangeId>> {
        Ok(self
            .vcs
            .resolve(&format!("bookmarks(exact:{name:?})"))?
            .into_iter()
            .next()
            .map(|c| c.change_id))
    }

    /// The branch the working copy currently sits on (nearest local bookmark at-or-below `@`,
    /// excluding trunk). `None` means the working copy is on trunk.
    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.current_branch_tip()?.map(|(name, _)| name))
    }

    /// The current branch's name **and** tip change id in one query (the nearest stack bookmark
    /// at-or-below `@`). Avoids a second `branch_tip` lookup for callers that need both.
    fn current_branch_tip(&self) -> Result<Option<(String, ChangeId)>> {
        let res = self
            .vcs
            .resolve(&format!("heads(::@ & {STACK_BOOKMARKS})"))?;
        Ok(res.into_iter().next().and_then(|c| {
            c.local_bookmarks
                .iter()
                .find(|b| self.is_stack_bookmark(b))
                .map(|name| (name.clone(), c.change_id.clone()))
        }))
    }

    /// Whether a bookmark is a stack branch (not trunk, not an internal stash bookmark).
    fn is_stack_bookmark(&self, name: &str) -> bool {
        name != self.state.config.trunk && !name.starts_with("jjk/stash/")
    }

    fn branch_tip(&self, name: &str) -> Result<ChangeId> {
        self.resolve_bookmark(name)?
            .ok_or_else(|| JjkError::UnknownBranch(name.to_string()).into())
    }

    /// First commits of the branches directly upstack of `branch_tip` (its nearest descendant
    /// bookmarks). Used to restack the upstack after a mid-stack commit (ARCH §3.3 step 3).
    fn upstack_first_commits(&self, branch_tip: &ChangeId) -> Result<Vec<ChangeId>> {
        let tip = branch_tip.as_str();
        let near = self
            .vcs
            .resolve(&format!("roots(({tip}:: ~ {tip}) & {STACK_BOOKMARKS})"))?;
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
        let cur = self.current_branch_tip()?;
        let current = cur.as_ref().map(|(n, _)| n.clone());

        // Anchor for the "upstack" search: the current branch tip, or trunk if on trunk.
        let cur_tip_id = cur.as_ref().map(|(_, id)| id.clone()).unwrap_or_else(|| trunk_id.clone());
        let cur_tip = cur_tip_id.as_str();

        // Topmost bookmarked tip in the stack containing the current position; if none, the current
        // branch tip itself (reusing the id we already resolved — no extra query).
        let upmost = self
            .vcs
            .resolve(&format!("heads(({cur_tip}:: ~ {cur_tip}) & {STACK_BOOKMARKS})"))?;
        let top_id = upmost
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .unwrap_or(cur_tip_id);

        // Mutable ancestry from trunk (exclusive) up to top (inclusive), newest-first → reverse.
        // `& mutable()` excludes already-merged / shared (immutable) commits, which are part of
        // trunk's world, not the editable stack. Without this, a branch built on top of a
        // previously-merged stack would display (and try to rebase) those immutable commits.
        let mut commits = self
            .vcs
            .resolve(&format!("({}..{}) & mutable()", trunk_revset, top_id.as_str()))?;
        commits.reverse(); // bottom → top

        // Group into branches: a commit carrying a (non-trunk) local bookmark closes a branch.
        let trunk_name = self.state.config.trunk.clone();
        let mut branches: Vec<Branch> = Vec::new();
        let mut acc: Vec<CommitInfo> = Vec::new();
        for c in commits {
            acc.push(c.clone());
            if let Some(name) = c.local_bookmarks.iter().find(|b| self.is_stack_bookmark(b)) {
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

    // ---------------------------------------------------------------- git interop

    /// Follow an external `git checkout`. jjk's fast reads use `--ignore-working-copy`, which skips
    /// jj's import of git `HEAD` — so after a plain `git checkout` jjk's position would lag. When
    /// git `HEAD` no longer matches jj's `@-`, snapshot once to let jj reconcile (it resets `@`
    /// onto the new `HEAD`). Returns `true` if it had to reconcile. Cheap in the common case (two
    /// non-snapshotting queries, no snapshot). Staleness (multi-workspace only) is handled
    /// separately; with one workspace a snapshot can't be stale-blocked.
    pub fn reconcile_git_head(&self) -> Result<bool> {
        let Some(head) = self.vcs.git_head()? else {
            return Ok(false);
        };
        let base = self.vcs.resolve("@-")?.into_iter().next().map(|c| c.commit_id.0);
        if base.as_deref() == Some(head.as_str()) {
            return Ok(false); // jj already in sync with git HEAD (the normal case)
        }
        if self.vcs.workspace_count()? <= 1 {
            self.vcs.snapshot()?; // triggers jj's "reset working copy parent to git HEAD"
            return Ok(true);
        }
        Ok(false)
    }

    /// Attach git `HEAD` to the branch jjk is currently on, so plain `git` shows the same branch.
    /// jj detaches `HEAD` whenever it moves `@`, so this runs after each command to keep them in
    /// sync. Best-effort.
    ///
    /// Only attaches when the branch tip is exactly `@-`, preserving jj's invariant that git `HEAD`
    /// == `@-` (so the next [`reconcile_git_head`](Self::reconcile_git_head) is a no-op). In the
    /// normal "checked out" state the current branch sits at `@-`. Right after `branch create` the
    /// bookmark rides the empty `@` (tip == `@`, not `@-`); attaching there would point `HEAD` at
    /// `@` and trigger a spurious reconcile, so we leave it until the first commit moves the
    /// bookmark down to `@-`.
    pub fn sync_git_head_to_current(&self) -> Result<()> {
        let Some((branch, tip)) = self.current_branch_tip()? else {
            return Ok(());
        };
        let at_parent = self
            .vcs
            .resolve("@-")?
            .into_iter()
            .next()
            .is_some_and(|p| p.change_id == tip);
        if at_parent {
            self.vcs.set_git_head_branch(&branch)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- staleness guard

    /// Auto-recover a stale working copy before any command that reads `@` (ARCH §8/§12).
    fn ensure_fresh(&self, report: &mut Report) -> Result<()> {
        // Staleness can only happen across multiple workspaces. With one workspace, skip the
        // (snapshotting, and therefore expensive) `jj status` check entirely.
        if self.vcs.workspace_count()? <= 1 {
            return Ok(());
        }
        if self.vcs.is_stale()? {
            self.vcs.update_stale()?;
            report.note("recovered stale working copy (jj workspace update-stale)");
        }
        Ok(())
    }

    // ---------------------------------------------------------------- local commands

    /// Run the git `pre-commit` hook (git semantics): blocks the commit if it fails. Callers skip
    /// this when `--no-verify` is given.
    pub fn run_pre_commit(&self, scope: &CommitScope) -> Result<()> {
        self.vcs.run_pre_commit_hook(scope)
    }

    /// Root-relative paths currently staged in the colocated git index (empty if none).
    pub fn staged_paths(&self) -> Result<Vec<String>> {
        self.vcs.staged_paths()
    }

    /// `jjk commit -m M` — commit the whole working copy (ARCH §3.3).
    pub fn commit(&mut self, message: &str) -> Result<Report> {
        self.commit_scoped(message, &CommitScope::All)
    }

    /// `jjk commit` with an explicit scope: all, specific paths (incl. git-staged), or interactive.
    /// Anything outside the scope stays uncommitted in `@`.
    pub fn commit_scoped(&mut self, message: &str, scope: &CommitScope) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let (branch, old_tip) = self.current_branch_tip()?.ok_or(JjkError::NotOnBranch)?;
        let upstack_firsts = self.upstack_first_commits(&old_tip)?;

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
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk commit --amend [-m M]` — squash working changes into the branch tip; descendants
    /// auto-rebase (ARCH §3.3 amend).
    pub fn commit_amend(&mut self, message: Option<&str>) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let (branch, tip) = self.current_branch_tip()?.ok_or(JjkError::NotOnBranch)?;
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

    /// `jjk restack` — ensure each upstack branch's first commit is parented on its downstack
    /// branch's tip. Usually a **no-op** (jj already auto-rebased on every rewrite); this repairs
    /// any drift and reports what moved.
    pub fn restack(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;

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
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk track [NAME]` / `jjk untrack [NAME]` — toggle stack-tracking. Defaults to the current
    /// branch. Tracking governs PR intent (only tracked branches are submitted in Phase 3).
    pub fn set_tracked(&mut self, name: Option<&str>, tracked: bool) -> Result<Report> {
        let mut report = Report::default();
        let name = match name {
            Some(n) => n.to_string(),
            None => self.current_branch()?.ok_or(JjkError::NotOnBranch)?,
        };
        if name == self.state.config.trunk {
            return Err(JjkError::IsTrunk(name).into());
        }
        if !self.vcs.bookmarks()?.iter().any(|b| b.name == name) {
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
    pub fn branch_delete(&mut self, name: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        if name == self.state.config.trunk {
            return Err(JjkError::IsTrunk(name.to_string()).into());
        }
        let stack = self.derive_stack()?;
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
        if self.vcs.bookmarks()?.iter().any(|b| b.name == name) {
            let n = name.to_string();
            self.vcs.transaction(&mut |tx| tx.delete_bookmark(&n))?;
        }

        self.state.branches.remove(name);
        self.state.save(&self.root)?;
        report.note(format!("deleted branch '{name}'"));
        if had_upstack {
            report.note("upstack reconnected to its parent");
        }
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    // ---------------------------------------------------------------- branch restructuring

    /// `jjk trunk` — switch to the trunk branch.
    pub fn trunk_checkout(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let (_, id) = self.trunk_anchor()?;
        self.vcs.transaction(&mut |tx| {
            tx.new_child(&id)?;
            Ok(())
        })?;
        report.note(format!("switched to trunk '{}'", self.state.config.trunk));
        Ok(report)
    }

    /// `jjk branch onto <target>` — move the current branch and everything stacked above it onto a
    /// new base (`target` branch's tip, or trunk). The upstack rides along (jj auto-rebases).
    pub fn branch_onto(&mut self, target: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
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
            self.branch_tip(target)?
        };
        self.vcs.transaction(&mut |tx| {
            tx.rebase(&first, &dest)?;
            Ok(())
        })?;
        report.note(format!("moved '{branch}' onto '{target}'"));
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk branch rename [old] <new>` — rename a branch (default: the current one), preserving its
    /// PR mapping in state.
    pub fn branch_rename(&mut self, old: Option<&str>, new: &str) -> Result<Report> {
        let mut report = Report::default();
        let old = match old {
            Some(o) => o.to_string(),
            None => self.current_branch()?.ok_or(JjkError::NotOnBranch)?,
        };
        if old == self.state.config.trunk {
            return Err(JjkError::IsTrunk(old).into());
        }
        if self.resolve_bookmark(&old)?.is_none() {
            return Err(JjkError::UnknownBranch(old).into());
        }
        if self.resolve_bookmark(new)?.is_some() {
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
    pub fn branch_diff(&self) -> Result<String> {
        let stack = self.derive_stack()?;
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
    }

    /// `jjk branch squash [-m M]` — collapse all of the current branch's commits into one.
    pub fn branch_squash(&mut self, message: Option<&str>) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
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
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk branch fold` — fold the current branch into its downstack base: the base's bookmark
    /// advances over the current branch's commits and the current bookmark is dropped (one fewer
    /// PR; the upstack reconnects to the base).
    pub fn branch_fold(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
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
    pub fn commit_fixup(&mut self, target: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        if target == self.state.config.trunk {
            return Err(JjkError::IsTrunk(target.to_string()).into());
        }
        let target_tip = self.branch_tip(target)?;
        // Snapshot so on-disk edits are captured into @ before folding them down.
        let wc = self.vcs.snapshot()?;
        if wc.is_empty {
            report.note("nothing to fix up (working copy is clean)");
            return Ok(report);
        }
        self.vcs
            .transaction(&mut |tx| tx.squash_working_into(&target_tip))?;
        report.note(format!("fixed up '{target}' with working-copy changes"));
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk commit --split` — split the current branch's tip into two commits via an interactive
    /// diff editor (`jj split`); descendants auto-rebase.
    pub fn commit_split(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let (branch, tip) = self.current_branch_tip()?.ok_or(JjkError::NotOnBranch)?;
        self.vcs.split_interactive(&tip)?;
        report.note(format!("split the tip of '{branch}'"));
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk commit --pick <rev>` — copy a commit (e.g. from an upstack branch) onto the current
    /// branch's tip; the upstack rides along.
    pub fn commit_pick(&mut self, rev: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let (branch, tip) = self.current_branch_tip()?.ok_or(JjkError::NotOnBranch)?;
        let src = self
            .vcs
            .resolve(rev)?
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
            .resolve(&format!("children({})", tip.as_str()))?
            .into_iter()
            .next()
            .ok_or_else(|| JjkError::Msg("could not locate the picked commit".into()))?
            .change_id;
        let bname = branch.clone();
        self.vcs
            .transaction(&mut |tx| tx.set_bookmark(&bname, &dup))?;
        report.note(format!("picked {} onto '{branch}'", src.short()));
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// `jjk branch split <new> <commit>` — split the current branch at `commit`: a new tracked
    /// branch `<new>` takes the commits up to and including `commit`; the current branch keeps the
    /// rest. (`commit` must be within the branch and below its tip.)
    pub fn branch_split(&mut self, new_name: &str, at: &str) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
        let branch = stack.current.clone().ok_or(JjkError::NotOnBranch)?;
        let b = stack
            .branch(&branch)
            .ok_or_else(|| JjkError::UnknownBranch(branch.clone()))?;
        if self.resolve_bookmark(new_name)?.is_some() {
            return Err(JjkError::Msg(format!("branch '{new_name}' already exists")).into());
        }
        let at_id = self
            .vcs
            .resolve(at)?
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

    // ---------------------------------------------------------------- worktrees (jj workspaces)

    /// `jjk worktree add <path> [name] [--branch B]` — create a jj workspace. The new workspace's
    /// `@` starts as an empty child of `B`'s tip (or trunk). Great for parallel agents per branch.
    pub fn worktree_add(
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
            Some(b) if b != self.state.config.trunk => self.branch_tip(b)?,
            _ => self.trunk_anchor()?.1,
        };
        self.vcs.add_workspace(path, &name, &at)?;
        report.note(format!("added workspace '{name}' at {}", path.display()));
        if let Some(b) = branch {
            report.note(format!("starting on '{b}'"));
        }
        Ok(report)
    }

    /// `jjk worktree list` — workspaces with their per-workspace current branch.
    pub fn worktree_list(&self) -> Result<Vec<WorktreeRow>> {
        let mut rows = Vec::new();
        for ws in self.vcs.workspaces()? {
            let current = self.branch_at(&format!("{}@", ws.name))?;
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
    pub fn worktree_remove(&self, name: &str) -> Result<Report> {
        let mut report = Report::default();
        self.vcs.forget_workspace(name)?;
        report.note(format!("removed workspace '{name}' (files left on disk)"));
        Ok(report)
    }

    /// Nearest non-trunk local bookmark at-or-below the commit named by `at_revset` (e.g. `ws2@`).
    fn branch_at(&self, at_revset: &str) -> Result<Option<String>> {
        let res = self
            .vcs
            .resolve(&format!("heads(::{at_revset} & {STACK_BOOKMARKS})"))?;
        Ok(res
            .into_iter()
            .next()
            .and_then(|c| c.local_bookmarks.into_iter().find(|b| self.is_stack_bookmark(b))))
    }

    // ---------------------------------------------------------------- stash (muscle memory; D-§5)

    /// `jjk stash` — park the working-copy changes aside on a `jjk/stash/N` bookmark and leave a
    /// clean empty `@` in place. (Mostly unnecessary in jj since switching is safe.)
    pub fn stash(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        // Snapshot so on-disk edits are seen before deciding whether there's anything to stash.
        let wc = self.vcs.snapshot()?;
        if wc.is_empty {
            report.note("nothing to stash (working copy is clean)");
            return Ok(report);
        }
        let parent = wc
            .parents
            .first()
            .cloned()
            .ok_or_else(|| JjkError::Msg("working copy has no parent to stash onto".into()))?;
        let n = self.next_stash_number()?;
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
    pub fn stash_pop(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stashes = self.list_stashes()?;
        let (name, from) = stashes
            .into_iter()
            .max_by_key(|(_, _, n)| *n)
            .map(|(name, id, _)| (name, id))
            .ok_or_else(|| JjkError::Msg("no stash to pop".into()))?;
        let into = self.vcs.working_copy()?.change_id;
        let name_cl = name.clone();
        self.vcs.transaction(&mut |tx| {
            tx.squash(&from, &into)?; // restore changes into @ (abandons the now-empty stash)
            tx.forget_bookmark(&name_cl)?; // bookmark slid to the parent; drop it (local-only)
            Ok(())
        })?;
        report.note(format!("popped {name}"));
        Ok(report)
    }

    fn list_stashes(&self) -> Result<Vec<(String, ChangeId, u64)>> {
        let mut out = Vec::new();
        for b in self.vcs.bookmarks()? {
            if let Some(rest) = b.name.strip_prefix("jjk/stash/") {
                if let Ok(n) = rest.parse::<u64>() {
                    out.push((b.name.clone(), b.target, n));
                }
            }
        }
        Ok(out)
    }

    fn next_stash_number(&self) -> Result<u64> {
        Ok(self
            .list_stashes()?
            .into_iter()
            .map(|(_, _, n)| n)
            .max()
            .unwrap_or(0)
            + 1)
    }

    // ---------------------------------------------------------------- conflict resolution helper

    /// `jjk resolve` — open the lowest conflicted commit for editing (`jj edit`). The user edits
    /// the files to resolve; the next jjk command re-snapshots and the resolution propagates to
    /// descendants (JJ_NOTES §7). Then `jjk checkout <branch>` restores the empty-`@` invariant.
    pub fn resolve(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
        // Lowest conflicted commit across the stack (bottom→top).
        let target = stack
            .branches
            .iter()
            .flat_map(|b| b.commits.iter())
            .find(|c| c.has_conflict)
            .map(|c| c.change_id.clone());
        match target {
            None => {
                report.note("no conflicts to resolve");
            }
            Some(id) => {
                self.vcs.transaction(&mut |tx| tx.edit(&id))?;
                report.note(format!("editing conflicted change {} — resolve the marked files,", id.short()));
                report.note("then run any jjk command to re-snapshot; the fix propagates upstack.");
                report.note("finally `jjk checkout <branch>` to restore a clean working copy.");
            }
        }
        Ok(report)
    }

    /// `jjk undo` — expose jj's op-log undo.
    /// Record a checkpoint before a mutating command runs, so `jjk undo` can revert the *whole*
    /// command in one step. A single jjk command maps to several jj operations (e.g. `commit` does
    /// `jj commit` + `jj bookmark set` + restacks); plain `jj undo` reverts only the last of them,
    /// which leaves the repo half-changed. We snapshot the head op id (and jjk's own state.toml)
    /// here and `jj op restore` to it in [`undo`]. Best-effort: a failure must never block the real
    /// command, so callers ignore the error (undo then falls back to a single `jj undo`).
    pub fn checkpoint(&self) -> Result<()> {
        let op_id = self.vcs.current_op_id()?;
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
    pub fn undo(&mut self) -> Result<Report> {
        let mut report = Report::default();
        let mut stack = self.load_checkpoints();
        let msg = match stack.pop() {
            Some(ckpt) => {
                let msg = self.vcs.restore_op(&ckpt.op_id)?;
                // Restore jjk's own state alongside the jj repo so the two don't drift.
                if !ckpt.state.is_empty() {
                    std::fs::write(State::path_for(&self.root), &ckpt.state)?;
                    self.state = State::load(&self.root)?;
                }
                self.save_checkpoints(&stack)?;
                msg
            }
            None => self.vcs.undo()?,
        };
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
                let n = b.commits.iter().filter(|c| c.has_conflict).count();
                report.conflicts.push(format!(
                    "{}: {n} {} resolution",
                    b.name,
                    plural(n, "change needs", "changes need")
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

    /// `jjk pull` — fetch trunk and rebase the current stack onto it. **No** merged-PR detection
    /// (that's `sync`). The local trunk bookmark fast-forwards on fetch (JJ_NOTES §9).
    pub fn pull(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.vcs.fetch(&self.state.config.remote)?;
        report.note(format!("fetched {}", self.state.config.remote));
        self.ensure_fresh(&mut report)?;
        let moved = self.rebase_stack_onto_trunk()?;
        if moved > 0 {
            report.note(format!(
                "rebased {moved} stack {} onto trunk",
                plural(moved, "root", "roots")
            ));
        } else {
            report.note("stack already on latest trunk");
        }
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }

    /// Rebase the roots of the current stack (`roots(trunk()..top)`) onto trunk. Landing-method
    /// agnostic: only commits not already in trunk's ancestry move (JJ_NOTES §9c). Returns the
    /// number of roots rebased.
    fn rebase_stack_onto_trunk(&self) -> Result<usize> {
        let (trunk_revset, trunk_id) = self.trunk_anchor()?;
        let stack = self.derive_stack()?;
        let Some(top) = stack.top() else {
            return Ok(0);
        };
        // Only rebase MUTABLE roots. Immutable commits (already merged / shared) are part of
        // trunk's world; jj refuses to rewrite them, and we don't need to (a branch built atop a
        // previously-merged stack rebases its own mutable commits straight onto trunk).
        let roots = self.vcs.resolve(&format!(
            "roots(({}..{}) & mutable())",
            trunk_revset,
            top.tip.as_str()
        ))?;
        if roots.is_empty() {
            return Ok(0);
        }
        // Skip roots already parented on trunk (no-op rebases).
        let to_move: Vec<ChangeId> = roots
            .into_iter()
            .filter(|r| !r.parents.contains(&trunk_id))
            .map(|r| r.change_id)
            .collect();
        if to_move.is_empty() {
            return Ok(0);
        }
        let n = to_move.len();
        self.vcs.transaction(&mut |tx| {
            for r in &to_move {
                tx.rebase(r, &trunk_id)?;
            }
            Ok(())
        })?;
        Ok(n)
    }

    /// `jjk pr view` — the current branch's PR. With `print`, return its URL (don't open a browser);
    /// otherwise open it in the browser and return `None`. Errors if not on a branch, or the branch
    /// has no submitted PR.
    pub async fn pr_view(&self, print: bool) -> Result<Option<String>> {
        let branch = self.current_branch()?.ok_or(JjkError::NotOnBranch)?;
        let pr = self.state.pr_of(&branch).ok_or_else(|| {
            JjkError::Msg(format!("no PR for '{branch}' yet — run `jjk submit` first"))
        })?;
        self.forge()?.view_pr(pr, !print).await
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
        self.ensure_fresh(&mut report)?;
        let stack = self.derive_stack()?;
        let remote = self.state.config.remote.clone();
        let trunk_name = stack.trunk_name.clone();
        // Easter egg: a user can opt a PR-body flourish in via their jj config (undocumented).
        let yuji = self.vcs.config_get(YUJI_KEY)?.as_deref() == Some(YUJI_VALUE);

        // Full tracked list (bottom→top) with correct bases. Bases come from the *whole* stack —
        // a subset submit still bases each PR on its real downstack branch, not the subset.
        struct Item {
            name: String,
            base: String,
            title: String,
            body: String,
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
            plan.push(Item {
                name: b.name.clone(),
                base: base.clone(),
                title,
                body: pr_body(b, &base),
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
            let forge = self.forge()?;
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
            self.vcs.push(&remote, &item.name, PushOpts::default())?;
            let number = match &existing[slot] {
                Some(pr) => {
                    let pr = pr.number;
                    // Retarget the base only — never the body, so we don't clobber the author's
                    // description on a re-submit.
                    self.forge()?.update_pr(pr, Some(&item.base)).await?;
                    report.note(format!("updated #{} {} (base {})", pr, item.name, item.base));
                    pr
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
                        .forge()?
                        .create_pr(&item.name, &item.base, &d.title, &d.body, d.draft)
                        .await?;
                    let kind = if d.draft { "draft " } else { "" };
                    report.note(format!(
                        "created {}#{} {} (base {})",
                        kind, pr.number, item.name, item.base
                    ));
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
    async fn refresh_nav_comments(&mut self, stack_prs: &[(String, u64)], yuji: bool) -> Result<usize> {
        // Seed the per-PR comment ids we already know (pr → comment id) from state.
        let known: std::collections::HashMap<u64, u64> = stack_prs
            .iter()
            .filter_map(|(name, pr)| self.state.nav_comment_of(name).map(|cid| (*pr, cid)))
            .collect();
        let touched = self.upsert_nav_comments(stack_prs, yuji, &known).await?;
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
        known: &std::collections::HashMap<u64, u64>,
    ) -> Result<Vec<(u64, u64)>> {
        if prs.len() < 2 {
            return Ok(Vec::new());
        }
        let forge = self.forge()?;
        let tasks = prs.iter().enumerate().map(|(idx, (_, pr))| {
            let pr = *pr;
            let body = nav_comment_body(prs, idx, yuji);
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
/// footer shows this PR's position (`x/N`) and links jjk.
fn nav_comment_body(prs: &[(String, u64)], current_idx: usize, yuji: bool) -> String {
    let n = prs.len();
    let mut s = format!("**🥞 This change is part of the following stack · PR {}/{}**\n\n", current_idx + 1, n);
    for (i, (_branch, pr)) in prs.iter().enumerate() {
        let indent = "    ".repeat(i);
        let marker = if i == current_idx { " ◀" } else { "" };
        s.push_str(&format!("{indent}- #{pr}{marker}\n"));
    }
    s.push_str("\nManaged by [jjk](https://github.com/sirmammingtonham/jjk).\n");
    if yuji {
        s.push('\n');
        s.push_str(YUJI_FLOURISH);
        s.push('\n');
    }
    s.push_str(NAV_MARKER);
    s.push('\n');
    s
}

impl Engine {
    /// `jjk sync` — fetch trunk, reconcile merged branches, rebase the survivors, and (when `push`)
    /// force-push and retarget their PR bases (ARCHITECTURE §6). Landing-method agnostic (JJ_NOTES
    /// §9): squash and merge-commit are both handled by the `roots(trunk()..top)` rebase +
    /// empty/immutable handling. With `push = false`, only local state is reconciled (no force-push,
    /// no PR retarget, no remote deletions).
    pub async fn sync(&mut self, push: bool) -> Result<Report> {
        let mut report = Report::default();

        // 1. Capture branch→PR BEFORE fetching: a merge-commit landing absorbs the merged branch
        // into trunk on fetch, after which it no longer appears in the derived stack (JJ_NOTES §9b).
        self.ensure_fresh(&mut report)?;
        let pre = self.derive_stack()?;
        let candidates: Vec<(String, u64)> = pre
            .branches
            .iter()
            .filter_map(|b| b.pr.map(|pr| (b.name.clone(), pr)))
            .collect();

        // 2. fetch (advances local trunk; JJ_NOTES §9), then query merged-state.
        self.vcs.fetch(&self.state.config.remote)?;
        report.note(format!("fetched {}", self.state.config.remote));
        // Query merged-state for all candidate PRs concurrently (independent reads by PR number) —
        // one round-trip instead of N. Order is preserved by zipping back onto `candidates`.
        let merged_flags = {
            let forge = self.forge()?;
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
        let moved = self.rebase_stack_onto_trunk()?;
        if moved > 0 {
            report.note(format!(
                "rebased {moved} stack {} onto trunk",
                plural(moved, "root", "roots")
            ));
        }

        // 4. Reconcile each merged branch.
        let post = self.derive_stack()?;
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
                    if self.vcs.bookmarks()?.iter().any(|bm| bm.name == *name) {
                        let n = name.clone();
                        self.vcs.transaction(&mut |tx| tx.delete_bookmark(&n))?;
                    }
                    report.note(format!("dropped merged '{name}' (merge-commit landing)"));
                }
            }
            self.state.branches.remove(name);
        }

        // 5. Recover the current workspace if a rewrite left it stale (cross-workspace: Phase 5).
        if self.vcs.is_stale().unwrap_or(false) {
            self.vcs.update_stale()?;
            report.note("recovered stale working copy");
        }

        // 6 + 7. Force-push survivors and retarget their PR bases bottom-up. Skipped with --no-push,
        // which reconciles local state only (no force-push, no PR retarget, no remote deletions).
        if !push {
            report.note("synced local state only (--no-push); run `jjk sync` to push & retarget");
        } else {
            let remote = self.state.config.remote.clone();
            let survivors = self.derive_stack()?;
            let trunk_name = survivors.trunk_name.clone();

            // Prefetch each pushable branch's PR record concurrently (read-only) so the sequential
            // push/retarget pass below doesn't pay a `gh pr list` round-trip per branch. Same query
            // as before (by head); conflicted branches are skipped anyway, so don't fetch them.
            let pr_by_head: std::collections::HashMap<String, Option<PrRef>> = {
                let forge = self.forge()?;
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
            let mut pushed = 0usize;
            for b in survivors.branches.iter().filter(|b| b.tracked) {
                // A conflicted commit cannot be pushed; skip and report (D4 — don't abort).
                if b.has_conflict() {
                    report.note(format!(
                        "skipped '{}' — has conflicts; resolve then re-run `jjk sync`",
                        b.name
                    ));
                    prev_tracked = Some(b.name.clone());
                    continue;
                }
                self.vcs.push(&remote, &b.name, PushOpts::default())?;
                pushed += 1;
                if let Some(pr) = b.pr {
                    let base = prev_tracked.clone().unwrap_or_else(|| trunk_name.clone());
                    // Retarget best-effort: a dependent PR may have been closed by GitHub when its
                    // base branch was deleted on merge — you can't retarget a closed PR, so report.
                    match pr_by_head.get(&b.name).cloned().flatten() {
                        Some(p) if p.state == PrState::Open => {
                            self.forge()?.update_pr(pr, Some(&base)).await?;
                            report.note(format!("#{pr} {} → base {base}", b.name));
                        }
                        Some(p) => report.note(format!(
                            "#{pr} {} is {}; not retargeting (reopen it to restack the PR)",
                            b.name, p.state
                        )),
                        None => report.note(format!("{}: PR not found; skipping retarget", b.name)),
                    }
                }
                prev_tracked = Some(b.name.clone());
            }
            if pushed > 0 {
                report.note(format!(
                    "force-pushed {pushed} surviving {}",
                    plural(pushed, "branch", "branches")
                ));
            }
            // Best-effort: propagate merged-branch deletions to the remote.
            if !merged_names.is_empty() {
                let _ = self.vcs.push_deleted(&remote);
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
            let yuji = self.vcs.config_get(YUJI_KEY)?.as_deref() == Some(YUJI_VALUE);
            self.refresh_nav_comments(&stack_prs, yuji).await?;
        }

        self.state.save(&self.root)?;
        self.collect_conflicts(&mut report)?;
        Ok(report)
    }
}

/// PR body: a small jjk-managed marker plus the branch's commit subjects and its base.
fn pr_body(branch: &Branch, base: &str) -> String {
    let mut body = String::from("Managed by jjk (stacked PR).\n\n");
    body.push_str(&format!("Base: `{base}`\n\nCommits:\n"));
    for c in &branch.commits {
        let subj = c.subject();
        if !subj.is_empty() {
            body.push_str(&format!("- {subj}\n"));
        }
    }
    body
}

/// Opt-in (undocumented) jj config key/value that toggles the PR-body flourish, and the flourish
/// itself. Set `yuji = "it_doesnt_matter"` in jj config to enable.
const YUJI_KEY: &str = "yuji";
const YUJI_VALUE: &str = "it_doesnt_matter";
const YUJI_FLOURISH: &str = "\n<a href=\"https://ethan.website/jjk\"/><img src=\"https://media.tenor.com/Ax5XJTSDE6kAAAAe/yuji-itadori-son.png\" width=200/></a>";

#[derive(Clone, Copy, Debug)]
pub enum NavDir {
    Up,
    Down,
    Top,
    Bottom,
}

/// Options for [`Engine::submit_with`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SubmitOptions {
    /// Create newly-opened PRs as drafts. Pre-fills the prompt's draft default; with `AutoFill`
    /// (`--fill`/non-TTY) it is the final value. Has no effect on PRs that already exist.
    pub draft: bool,
}

/// Which branches `submit` operates on, relative to the current branch.
#[derive(Clone, Copy, Debug)]
pub enum SubmitScope {
    /// The whole stack (default `jjk submit`).
    Stack,
    /// Only the current branch.
    Branch,
    /// The current branch and everything above it.
    Upstack,
    /// The current branch and everything below it.
    Downstack,
}

/// A row for `jjk worktree list`.
#[derive(Clone, Debug)]
pub struct WorktreeRow {
    pub name: String,
    pub working_copy: ChangeId,
    pub current_branch: Option<String>,
    pub is_stale: bool,
}

/// Walk up from `cwd` to find the workspace root (nearest directory containing `.jj`).
fn find_workspace_root(cwd: &Path) -> Option<PathBuf> {
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        if dir.join(".jj").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Resolve the **main** repo root from a workspace root. The main workspace has a `.jj/repo`
/// directory; a secondary workspace has a `.jj/repo` *file* containing a path (relative to its
/// `.jj/`) to the main repo's `.jj/repo` (JJ_NOTES / probed). Shared `state.toml` lives there.
fn main_root_of(ws_root: &Path) -> Result<PathBuf> {
    let repo = ws_root.join(".jj").join("repo");
    let meta = std::fs::symlink_metadata(&repo)
        .map_err(|e| JjkError::Msg(format!("cannot stat {}: {e}", repo.display())))?;
    if meta.is_dir() {
        return Ok(ws_root.to_path_buf());
    }
    // Secondary workspace: follow the pointer file to the main repo.
    let target = std::fs::read_to_string(&repo)
        .map_err(|e| JjkError::Msg(format!("cannot read {}: {e}", repo.display())))?;
    let main_repo = ws_root.join(".jj").join(target.trim());
    let main_root = main_repo
        .parent() // .../main/.jj
        .and_then(|p| p.parent()) // .../main
        .ok_or_else(|| JjkError::Msg("could not resolve main repo root".into()))?;
    main_root
        .canonicalize()
        .map_err(|e| JjkError::Msg(format!("canonicalize main root failed: {e}")).into())
}
