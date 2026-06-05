//! The engine: each verb → an ordered plan of (VCS ops, state updates, forge ops). Depends only on
//! the `Vcs`/`Forge` traits and `model` types — never on a concrete backend.

mod branch;
mod commit;
mod domain;
mod resolve;
mod submit;
mod sync;
mod workspace;
pub mod expansion;
pub mod stack;

use crate::engine::expansion::ExpansionState;
use crate::error::{JjkError, Result};
use crate::forge::Forge;
use crate::llm::Splitter;
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

/// Above this many change-atoms, domain expansion splits hierarchically (bucket the atoms into
/// dependency-component groups and split each separately) so no single LLM call carries the whole
/// catalog. Sized to keep a single split prompt comfortably small.
const HIERARCHICAL_THRESHOLD: usize = 60;

/// A single undo point: the jj operation to restore to, plus a snapshot of jjk's state.toml so the
/// two stay in sync. Stored as a stack in `.jj/jjk/undo.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    op_id: String,
    state: String,
}

/// The command to re-run once the stack is conflict-free again. Only commands with a *deferred*
/// remote effect record one (sync skips pushing conflicted branches); pure-rebase commands don't.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ResumeCmd {
    /// Re-run `jjk sync` to push the now-resolved branches and retarget their PRs.
    Sync { push: bool },
}

/// An in-progress, git-style conflict-resolution session (persisted in `.jj/jjk/resolve.json`).
/// Created when a command leaves the stack conflicted; cleared when `resolve --continue` walks
/// through the last conflict, or on `resolve --abort`. Lets `resolve` narrate where you are and
/// return you to the branch you started on.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ResolveSession {
    /// Branch to return to when resolution finishes (`None` = trunk).
    home: Option<String>,
    /// What to auto-resume once the stack is clean.
    resume_cmd: Option<ResumeCmd>,
    /// Pre-command checkpoint to restore to on `--abort` (op id + state.toml snapshot).
    presync: Option<Checkpoint>,
    /// Branches the origin command already pushed before the conflict surfaced. An op-log rewind
    /// can't un-push them, so `--abort` mentions them in one line.
    pushed: Vec<String>,
}

/// The lowest conflicted change in a stack (bottom→top), if any. jj propagates a conflict to every
/// descendant, so resolving the lowest first usually clears the ones above it too.
fn lowest_conflict(stack: &Stack) -> Option<ChangeId> {
    stack
        .branches
        .iter()
        .flat_map(|b| b.commits.iter())
        .find(|c| c.has_conflict)
        .map(|c| c.change_id.clone())
}

/// Name of the branch whose commits contain `id`, for display in the resolve prompts.
fn branch_of<'a>(stack: &'a Stack, id: &ChangeId) -> Option<&'a str> {
    stack
        .branches
        .iter()
        .find(|b| b.commits.iter().any(|c| &c.change_id == id))
        .map(|b| b.name.as_str())
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
    /// Domain-expansion splitter, built on first use: the Anthropic adapter when an API key is
    /// present, else a deterministic offline fallback. Injected directly by tests.
    splitter: std::cell::OnceCell<Box<dyn Splitter>>,
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
            splitter: std::cell::OnceCell::new(),
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
    pub async fn repo_init(
        dir: &Path,
        trunk: Option<String>,
        remote: Option<String>,
    ) -> Result<Report> {
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
            None => vcs.remotes().await?.into_iter().next().unwrap_or_else(|| "origin".to_string()),
        };
        // Trunk detection: prefer an explicit name; else a `main`/`master` bookmark; else "main".
        let trunk = match trunk {
            Some(t) => t,
            None => {
                let bms = vcs.bookmarks().await?;
                ["main", "master", "trunk"]
                    .into_iter()
                    .find(|cand| bms.iter().any(|b| b.name == *cand))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "main".to_string())
            }
        };
        report.note(format!("trunk = {trunk}, remote = {remote}"));
        // Use standard git-style conflict markers (`<<<<<<< / ======= / >>>>>>>`) rather than jj's
        // default `%%%%%%%` diff style, so conflicts read the way a git user expects. Best-effort.
        if vcs
            .set_config_repo("ui.conflict-marker-style", "git")
            .await
            .is_ok()
        {
            report.note("set git-style conflict markers");
        }
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

    /// Replace the domain-expansion splitter (tests inject a [`FakeSplitter`](crate::llm::FakeSplitter)).
    pub fn set_splitter(&mut self, splitter: Box<dyn Splitter>) {
        self.splitter = std::cell::OnceCell::from(splitter);
    }

    /// The splitter, built on first use: the Anthropic adapter when `ANTHROPIC_API_KEY` is set, else
    /// a deterministic offline fallback (so expansion still yields a valid — if unrefined — stack).
    fn splitter(&self) -> &dyn Splitter {
        if self.splitter.get().is_none() {
            let _ = self.splitter.set(self.build_splitter());
        }
        self.splitter.get().expect("just initialized").as_ref()
    }

    fn build_splitter(&self) -> Box<dyn Splitter> {
        use crate::llm::anthropic::AnthropicLlm;
        use crate::llm::FakeSplitter;
        // Env overrides beat per-repo config, so a tougher split can pick a more powerful model /
        // more effort for a single run without editing state: `JJK_LLM_MODEL`, `JJK_LLM_THINKING`.
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let model = env("JJK_LLM_MODEL").unwrap_or_else(|| self.state.config.llm_model.clone());
        let thinking =
            env("JJK_LLM_THINKING").unwrap_or_else(|| self.state.config.llm_thinking.clone());
        match AnthropicLlm::from_env(&model, &thinking) {
            Some(llm) => Box::new(llm),
            None => Box::new(FakeSplitter::deterministic()),
        }
    }

    /// The forge, built on first use (querying the remote only when a forge command runs).
    async fn forge(&self) -> Result<&dyn Forge> {
        if self.forge.get().is_none() {
            let f = self.build_forge().await?;
            let _ = self.forge.set(f);
        }
        Ok(self.forge.get().expect("just initialized").as_ref())
    }

    async fn build_forge(&self) -> Result<Box<dyn Forge>> {
        use crate::forge::gh_cli::GhCli;
        match self.forge_backend.as_str() {
            "gh_cli" => {
                let slug = self
                    .vcs
                    .remote_url(&self.state.config.remote)
                    .await?
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
    async fn trunk_anchor(&self) -> Result<(String, ChangeId)> {
        let tname = self.state.config.trunk.clone();
        if let Some(id) = self.resolve_bookmark(&tname).await? {
            Ok((tname, id))
        } else {
            let id = self.vcs.trunk().await?;
            Ok(("trunk()".to_string(), id))
        }
    }

    /// Resolve a single local bookmark to its target change id, if it exists. Targeted query
    /// (`bookmarks(exact:..)`) rather than scanning every bookmark in the repo.
    async fn resolve_bookmark(&self, name: &str) -> Result<Option<ChangeId>> {
        Ok(self
            .vcs
            .resolve(&format!("bookmarks(exact:{name:?})"))
            .await?
            .into_iter()
            .next()
            .map(|c| c.change_id))
    }

    /// The branch the working copy currently sits on (nearest local bookmark at-or-below `@`,
    /// excluding trunk). `None` means the working copy is on trunk.
    pub async fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.current_branch_tip().await?.map(|(name, _)| name))
    }

    /// The current branch's name **and** tip change id in one query (the nearest stack bookmark
    /// at-or-below `@`). Avoids a second `branch_tip` lookup for callers that need both.
    async fn current_branch_tip(&self) -> Result<Option<(String, ChangeId)>> {
        let res = self
            .vcs
            .resolve(&format!("heads(::@ & {STACK_BOOKMARKS})"))
            .await?;
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

    async fn branch_tip(&self, name: &str) -> Result<ChangeId> {
        self.resolve_bookmark(name)
            .await?
            .ok_or_else(|| JjkError::UnknownBranch(name.to_string()).into())
    }

    /// First commits of the branches directly upstack of `branch_tip` (its nearest descendant
    /// bookmarks). Used to restack the upstack after a mid-stack commit (ARCH §3.3 step 3).
    async fn upstack_first_commits(&self, branch_tip: &ChangeId) -> Result<Vec<ChangeId>> {
        let tip = branch_tip.as_str();
        let near = self
            .vcs
            .resolve(&format!("roots(({tip}:: ~ {tip}) & {STACK_BOOKMARKS})"))
            .await?;
        // Each near branch's first commit is an independent `roots(tip..b)` read → resolve them
        // concurrently (one round-trip instead of one per upstack branch).
        let revsets: Vec<String> = near
            .iter()
            .map(|b| format!("roots({tip}..{})", b.change_id.as_str()))
            .collect();
        let revset_refs: Vec<&str> = revsets.iter().map(String::as_str).collect();
        let resolved = self.vcs.resolve_many(&revset_refs).await?;
        Ok(resolved
            .into_iter()
            .filter_map(|commits| commits.into_iter().next().map(|c| c.change_id))
            .collect())
    }

    /// Reconstruct the stack containing `@` from jj. Bottom (nearest trunk) → top.
    pub async fn derive_stack(&self) -> Result<Stack> {
        self.derive_stack_at(None).await
    }

    /// Like [`derive_stack`](Engine::derive_stack) but anchored at an explicit position instead of
    /// the working copy. `None` anchors at `@` (the normal view); `Some(tip)` derives the stack
    /// ending at `tip`. Domain-expansion uses the latter to operate on the reconstructed layer
    /// chain (anchored at the top layer) while `@` stays on the monolith.
    pub async fn derive_stack_at(&self, anchor: Option<&ChangeId>) -> Result<Stack> {
        // The trunk-bookmark resolution and the current-branch-tip query are independent reads;
        // issue them concurrently (one round-trip instead of two). Mirrors `trunk_anchor` +
        // `current_branch_tip`, kept inline so both can share a single `resolve_many`.
        let tname = self.state.config.trunk.clone();
        let anchor_rev: String = anchor
            .map(|c| c.as_str().to_string())
            .unwrap_or_else(|| "@".to_string());
        let batch = self
            .vcs
            .resolve_many(&[
                &format!("bookmarks(exact:{tname:?})"),
                &format!("heads(::{anchor_rev} & {STACK_BOOKMARKS})"),
            ])
            .await?;
        let (trunk_revset, trunk_id) = match batch[0].first() {
            Some(c) => (tname.clone(), c.change_id.clone()),
            // No local trunk bookmark — fall back to the `trunk()` revset (JJ_NOTES §8).
            None => ("trunk()".to_string(), self.vcs.trunk().await?),
        };
        let cur: Option<(String, ChangeId)> = batch[1].first().and_then(|c| {
            c.local_bookmarks
                .iter()
                .find(|b| self.is_stack_bookmark(b))
                .map(|name| (name.clone(), c.change_id.clone()))
        });
        let current = cur.as_ref().map(|(n, _)| n.clone());

        // Anchor for the "upstack" search: the current branch tip, or trunk if on trunk.
        let cur_tip_id = cur.as_ref().map(|(_, id)| id.clone()).unwrap_or_else(|| trunk_id.clone());
        let cur_tip = cur_tip_id.as_str();

        // Mutable ancestry from trunk (exclusive) up to the **top of the stack** (inclusive), in one
        // query. The top is the topmost bookmarked tip in the stack containing the current position,
        // or the current tip itself if there is no upstack bookmark — expressed as the nested
        // `(heads(...) | cur_tip)` sub-revset so we don't need a separate round-trip to resolve it
        // first. `& mutable()` excludes already-merged / shared (immutable) commits, which are part
        // of trunk's world, not the editable stack (without it, a branch built atop a previously
        // merged stack would display and try to rebase those immutable commits). Bounding the range
        // by the top **bookmark** (not `cur_tip::`) also excludes the empty `@` and any WIP above it.
        let top_expr = format!("heads(({cur_tip}:: ~ {cur_tip}) & {STACK_BOOKMARKS})");
        let mut commits = self
            .vcs
            .resolve(&format!(
                "({trunk_revset}..({top_expr} | {cur_tip})) & mutable()"
            ))
            .await?;
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
    pub async fn reconcile_git_head(&self) -> Result<bool> {
        // git HEAD (git) and jj's `@-` (jj) are independent reads on different tools — resolve them
        // concurrently so the common no-op case costs one round-trip, not two.
        let (head, base_commit) =
            tokio::try_join!(self.vcs.git_head(), self.vcs.resolve("@-"))?;
        let Some(head) = head else {
            return Ok(false);
        };
        let base = base_commit.into_iter().next().map(|c| c.commit_id.0);
        if base.as_deref() == Some(head.as_str()) {
            return Ok(false); // jj already in sync with git HEAD (the normal case)
        }
        if self.vcs.workspace_count().await? <= 1 {
            self.vcs.snapshot().await?; // triggers jj's "reset working copy parent to git HEAD"
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
    pub async fn sync_git_head_to_current(&self) -> Result<()> {
        // The current branch tip (`heads(::@ & STACK)`) and `@-` are independent reads — resolve
        // them concurrently; this runs after every command, so the saved round-trip adds up.
        let cur_revset = format!("heads(::@ & {STACK_BOOKMARKS})");
        let (cur, parent) = tokio::try_join!(
            self.vcs.resolve(&cur_revset),
            self.vcs.resolve("@-")
        )?;
        let Some((branch, tip)) = cur.into_iter().next().and_then(|c| {
            c.local_bookmarks
                .iter()
                .find(|b| self.is_stack_bookmark(b))
                .map(|name| (name.clone(), c.change_id.clone()))
        }) else {
            return Ok(());
        };
        let at_parent = parent
            .into_iter()
            .next()
            .is_some_and(|p| p.change_id == tip);
        if at_parent {
            self.vcs.set_git_head_branch(&branch).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- staleness guard

    /// Auto-recover a stale working copy before any command that reads `@` (ARCH §8/§12).
    async fn ensure_fresh(&self, report: &mut Report) -> Result<()> {
        // Staleness can only happen across multiple workspaces. With one workspace, skip the
        // (snapshotting, and therefore expensive) `jj status` check entirely.
        if self.vcs.workspace_count().await? <= 1 {
            return Ok(());
        }
        if self.vcs.is_stale().await? {
            self.vcs.update_stale().await?;
            report.note("recovered stale working copy (jj workspace update-stale)");
        }
        Ok(())
    }

    // ---------------------------------------------------------------- local commands

    /// Run the git `pre-commit` hook (git semantics): blocks the commit if it fails. Callers skip
    /// this when `--no-verify` is given.
    pub async fn run_pre_commit(&self, scope: &CommitScope) -> Result<()> {
        self.vcs.run_pre_commit_hook(scope).await
    }

    /// Root-relative paths currently staged in the colocated git index (empty if none).
    pub async fn staged_paths(&self) -> Result<Vec<String>> {
        self.vcs.staged_paths().await
    }




    // ---------------------------------------------------------------- conflict surfacing

    /// Append a git-flavored conflict summary for the current stack (ARCH §9 D4).
    async fn collect_conflicts(&self, report: &mut Report) -> Result<()> {
        let stack = self.derive_stack().await?;
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
    pub async fn has_conflicts(&self) -> Result<bool> {
        Ok(self.derive_stack().await?.branches.iter().any(|b| b.has_conflict()))
    }

    // ---------------------------------------------------------------- remote passthrough (Phase 3 wires forge)

    pub async fn fetch(&self) -> Result<Report> {
        let mut report = Report::default();
        self.vcs
            .fetch(&self.state.config.remote, Some(&self.state.config.trunk))
            .await?;
        report.note(format!("fetched {}", self.state.config.remote));
        Ok(report)
    }

    pub async fn push_current(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let branch = self.current_branch().await?.ok_or(JjkError::NotOnBranch)?;
        self.vcs
            .push(&self.state.config.remote, &branch, PushOpts::default())
            .await?;
        report.note(format!("pushed {branch}"));
        Ok(report)
    }

    /// `jjk pull` — fetch trunk and rebase the current stack onto it. **No** merged-PR detection
    /// (that's `sync`). The local trunk bookmark fast-forwards on fetch (JJ_NOTES §9).
    pub async fn pull(&mut self) -> Result<Report> {
        let mut report = Report::default();
        self.vcs
            .fetch(&self.state.config.remote, Some(&self.state.config.trunk))
            .await?;
        report.note(format!("fetched {}", self.state.config.remote));
        self.ensure_fresh(&mut report).await?;
        let moved = self.rebase_stack_onto_trunk().await?;
        if moved > 0 {
            report.note(format!(
                "rebased {moved} stack {} onto trunk",
                plural(moved, "root", "roots")
            ));
        } else {
            report.note("stack already on latest trunk");
        }
        self.collect_conflicts(&mut report).await?;
        Ok(report)
    }

    /// Rebase the roots of the current stack (`roots(trunk()..top)`) onto trunk. Landing-method
    /// agnostic: only commits not already in trunk's ancestry move (JJ_NOTES §9c). Returns the
    /// number of roots rebased.
    /// Fast-forward the local trunk bookmark to the fetched remote trunk (`trunk@remote`), so the
    /// rest of `sync` (which anchors on the local bookmark) rebases the stack onto the latest trunk.
    /// No-op when there's no local trunk bookmark (the `trunk()` fallback already follows the
    /// remote), when it's already current, or when the move wouldn't be a fast-forward (so a
    /// divergent local trunk is never clobbered).
    async fn advance_trunk_to_remote(&self, report: &mut Report) -> Result<()> {
        let trunk = self.state.config.trunk.clone();
        let remote = self.state.config.remote.clone();
        let Some(local_id) = self.resolve_bookmark(&trunk).await? else {
            return Ok(());
        };
        let remote_ref = format!("{trunk}@{remote}");
        let Some(remote_tip) = self.vcs.resolve(&remote_ref).await?.into_iter().next() else {
            return Ok(()); // remote trunk not fetched (e.g. brand-new repo)
        };
        if remote_tip.change_id == local_id {
            return Ok(());
        }
        // Only advance if the local trunk is an ancestor of the fetched remote trunk.
        let is_fast_forward = self
            .vcs
            .resolve(&format!("{} & ancestors({remote_ref})", local_id.as_str()))
            .await?
            .iter()
            .any(|c| c.change_id == local_id);
        if !is_fast_forward {
            return Ok(());
        }
        let target = remote_tip.change_id;
        let name = trunk.clone();
        self.vcs.transaction(&mut |tx| tx.set_bookmark(&name, &target))?;
        report.note(format!("updated trunk '{trunk}' to {remote}"));
        Ok(())
    }

    async fn rebase_stack_onto_trunk(&self) -> Result<usize> {
        let (trunk_revset, trunk_id) = self.trunk_anchor().await?;
        let stack = self.derive_stack().await?;
        let Some(top) = stack.top() else {
            return Ok(0);
        };
        // Only rebase MUTABLE roots. Immutable commits (already merged / shared) are part of
        // trunk's world; jj refuses to rewrite them, and we don't need to (a branch built atop a
        // previously-merged stack rebases its own mutable commits straight onto trunk).
        let roots = self
            .vcs
            .resolve(&format!(
                "roots(({}..{}) & mutable())",
                trunk_revset,
                top.tip.as_str()
            ))
            .await?;
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

}




/// Whether the remote already has `branch` at its current tip commit — i.e. a push would be a
/// no-op. Read from the tip commit's remote bookmarks in the derived stack, so it needs no extra
/// query: after a rewrite (rebase/amend) the tip is a new commit that no longer carries
/// `branch@remote`, so this returns false and the branch gets pushed.
fn tip_on_remote(branch: &Branch, remote: &str) -> bool {
    branch.commits.last().is_some_and(|c| {
        c.remote_bookmarks
            .iter()
            .any(|r| r.name == branch.name && r.remote == remote)
    })
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
    /// Domain expansion: skip the interactive split-review gate (accept the proposed split).
    pub no_review: bool,
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
