//! In-process `Vcs` adapter built on the `jj-lib` crate (the sole VCS backend).
//!
//! A whole jjk command loads the repo once and groups its mutations into a single jj-lib
//! transaction, instead of spawning a `jj` subprocess per step. Interactive diff/merge editing
//! reuses `jj-cli`'s `merge_tools` (linked as a library — there is **no `jj` binary requirement**);
//! git fetch/push go through jj-lib's git layer (which drives the `git` subprocess and keeps jj's
//! view in sync via import/export of refs). The colocated git index/HEAD pokes that jj never models
//! (staged paths, `git rev-parse HEAD`) stay tiny `git` subprocess calls.
//!
//! Concurrency: the loaded `Workspace`/repo are not `Sync`, and jjk drives one command to
//! completion on the current async runtime (never spawning its work onto another thread), so the
//! `Vcs` futures are `?Send` and the mutable state lives behind a `tokio::sync::Mutex` (which makes
//! the adapter itself `Send`).

use crate::error::Result;
use crate::model::{
    Bookmark, Capabilities, ChangeId, CommitId, CommitInfo, RemoteRef, WorkspaceInfo,
};
use crate::vcs::{CommitScope, PushOpts, Vcs, VcsTx};
use anyhow::anyhow;
use async_trait::async_trait;
use futures::TryStreamExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use jj_lib::backend::CommitId as JjCommitId;
use jj_lib::commit::Commit;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::object_id::ObjectId as _;
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefNameBuf, RemoteName, WorkspaceNameBuf};
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo, StoreFactories};
use jj_lib::rewrite::{squash_commits, CommitWithSelection};
use jj_lib::transaction::Transaction;
use jj_lib::working_copy::{SnapshotOptions, WorkingCopyFreshness};
use jj_lib::revset::{
    RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
    RevsetWorkspaceContext,
};
use jj_lib::repo_path::RepoPathUiConverter;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{
    default_working_copy_factories, DefaultWorkspaceLoaderFactory, Workspace, WorkspaceLoaderFactory,
};
use jj_cli::revset_util::RevsetExpressionEvaluator;

/// Shared, interior-mutable backend state. Held by the adapter and by any open transaction handle.
struct Shared {
    settings: UserSettings,
    aliases: RevsetAliasesMap,
    extensions: Arc<RevsetExtensions>,
    workspace_name: WorkspaceNameBuf,
    /// Absolute workspace root (this workspace's working-copy directory).
    root: PathBuf,
    /// The `.jj/repo` store path (for re-reading config fresh).
    repo_path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    workspace: Workspace,
    /// Cached repo at head; reloaded after external changes / our own mutations.
    repo: Option<Arc<ReadonlyRepo>>,
}

/// The `jj-lib` VCS adapter.
pub struct JjLib {
    shared: Arc<Shared>,
}

impl JjLib {
    /// Open an existing colocated jj workspace rooted at `root`.
    pub fn open(root: &Path) -> Result<Self> {
        let loader = DefaultWorkspaceLoaderFactory
            .create(root)
            .map_err(|e| anyhow!("failed to locate jj workspace at {}: {e}", root.display()))?;
        let settings = build_settings(loader.repo_path(), loader.workspace_root())?;
        let workspace = loader
            .load(
                &settings,
                &StoreFactories::default(),
                &default_working_copy_factories(),
            )
            .map_err(|e| anyhow!("failed to load jj workspace: {e}"))?;
        let ui = jj_cli::ui::Ui::null();
        let aliases = jj_cli::cli_util::load_revset_aliases(&ui, settings.config())
            .map_err(|e| anyhow!("loading revset aliases: {e:?}"))?;
        let workspace_name = workspace.workspace_name().to_owned();
        let root = workspace.workspace_root().to_owned();
        let repo_path = workspace.repo_path().to_owned();
        Ok(Self {
            shared: Arc::new(Shared {
                settings,
                aliases,
                extensions: Arc::new(RevsetExtensions::new()),
                workspace_name,
                root,
                repo_path,
                inner: Mutex::new(Inner {
                    workspace,
                    repo: None,
                }),
            }),
        })
    }

    /// `jj git init --colocate`: create a colocated jj repo at `dir`, then open it.
    pub async fn init_colocated(dir: &Path) -> Result<Self> {
        let settings = build_settings(&dir.join(".jj").join("repo"), dir)?;
        Workspace::init_colocated_git(&settings, dir)
            .await
            .map_err(|e| anyhow!("jj git init --colocate failed: {e}"))?;
        Self::open(dir)
    }
}

/// Build [`UserSettings`] the way `jj` does — honoring `JJ_CONFIG`, user config, repo/workspace
/// config, and the default revset aliases (`trunk()`, `immutable_heads()`, …) — by reusing
/// `jj-cli`'s config machinery.
fn build_settings(repo_path: &Path, workspace_root: &Path) -> Result<UserSettings> {
    use jj_cli::config::{config_from_environment, default_config_layers, ConfigEnv};
    let ui = jj_cli::ui::Ui::null();
    let mut raw = config_from_environment(default_config_layers());
    let mut env = ConfigEnv::from_environment();
    env.reload_user_config(&mut raw)
        .map_err(|e| anyhow!("loading user jj config: {e}"))?;
    env.reset_repo_path(repo_path);
    env.reload_repo_config(&ui, &mut raw)
        .map_err(|e| anyhow!("loading repo jj config: {e:?}"))?;
    env.reset_workspace_path(workspace_root);
    let _ = env.reload_workspace_config(&ui, &mut raw);
    let config = env
        .resolve_config(&raw)
        .map_err(|e| anyhow!("resolving jj config: {e}"))?;
    UserSettings::from_config(config).map_err(|e| anyhow!("building jj settings: {e}"))
}

impl Shared {
    /// The repo at head, loading (and caching) it on first use.
    async fn repo(&self) -> Result<Arc<ReadonlyRepo>> {
        let mut inner = self.inner.lock().await;
        if inner.repo.is_none() {
            let repo = inner
                .workspace
                .repo_loader()
                .load_at_head()
                .await
                .map_err(|e| anyhow!("loading jj repo at head: {e}"))?;
            inner.repo = Some(repo);
        }
        Ok(inner.repo.as_ref().unwrap().clone())
    }

    /// Drop the cached repo so the next read reloads at head (after an external or our own change).
    async fn invalidate(&self) {
        self.inner.lock().await.repo = None;
    }

    /// Reload the workspace (and its repo loader / index) from disk. Our own mutations leave the
    /// reused in-memory index treating rewritten commits as still-visible — which makes change-id
    /// symbol resolution see false divergence — so after any mutation we rebuild from disk, exactly
    /// as a fresh `jj` invocation would. Caller holds the `inner` lock.
    fn reload_inner(&self, inner: &mut Inner) -> Result<()> {
        let loader = DefaultWorkspaceLoaderFactory
            .create(&self.root)
            .map_err(|e| anyhow!("reloading workspace: {e}"))?;
        inner.workspace = loader
            .load(
                &self.settings,
                &StoreFactories::default(),
                &default_working_copy_factories(),
            )
            .map_err(|e| anyhow!("reloading workspace: {e}"))?;
        inner.repo = None;
        Ok(())
    }

    /// Parse + resolve + evaluate `revset` against `repo`, returning the matched commits in
    /// reverse-topological (newest-first) order — matching `jj log`.
    async fn eval(&self, repo: &dyn Repo, revset: &str) -> Result<Vec<Commit>> {
        let path_converter = RepoPathUiConverter::Fs {
            cwd: self.root.clone(),
            base: self.root.clone(),
        };
        let workspace_ctx = RevsetWorkspaceContext {
            path_converter: &path_converter,
            workspace_name: &self.workspace_name,
        };
        let fileset_aliases = jj_lib::fileset::FilesetAliasesMap::new();
        let mut diagnostics = RevsetDiagnostics::new();
        let parse_ctx = RevsetParseContext {
            aliases_map: &self.aliases,
            local_variables: std::collections::HashMap::new(),
            user_email: self.settings.user_email(),
            date_pattern_context: chrono::Local::now().into(),
            default_ignored_remote: Some(RemoteName::new("git")),
            fileset_aliases_map: &fileset_aliases,
            use_glob_by_default: false,
            extensions: self.extensions.as_ref(),
            workspace: Some(workspace_ctx),
        };
        let expr = jj_lib::revset::parse(&mut diagnostics, revset, &parse_ctx)
            .map_err(|e| anyhow!("parsing revset `{revset}`: {e}"))?;
        let id_prefix = IdPrefixContext::new(self.extensions.clone());
        let evaluator =
            RevsetExpressionEvaluator::new(repo, self.extensions.clone(), &id_prefix, expr);
        let commits: Vec<Commit> = evaluator
            .evaluate_to_commits()
            .map_err(|e| anyhow!("evaluating revset `{revset}`: {e}"))?
            .try_collect()
            .await
            .map_err(|e| anyhow!("reading commits for `{revset}`: {e}"))?;
        Ok(commits)
    }

    /// Resolve `revset` to neutral [`CommitInfo`]s.
    async fn resolve_infos(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        let repo = self.repo().await?;
        let commits = self.eval(repo.as_ref(), revset).await?;
        let wc = repo.view().get_wc_commit_id(&self.workspace_name).cloned();
        let mut out = Vec::with_capacity(commits.len());
        for c in &commits {
            out.push(commit_info(&repo, c, wc.as_ref()).await?);
        }
        Ok(out)
    }

    /// Resolve a single revset (e.g. a change id) against `repo` to a jj-lib [`Commit`].
    async fn resolve_one(&self, repo: &dyn Repo, revset: &str) -> Result<Commit> {
        self.eval(repo, revset)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no commit matches `{revset}`"))
    }

    /// Follow an external `git checkout`: import the git `HEAD` and reset `@` onto it (a fresh empty
    /// commit on the new HEAD), without touching the working-copy files (git already updated them).
    /// No-op when `HEAD` already matches jj's view. Mirrors jj-cli's `import_git_head`.
    async fn import_head_op(&self) -> Result<()> {
        let repo = self.repo().await?;
        let mut inner = self.inner.lock().await;
        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.workspace_name);
        jj_lib::git::import_head(tx.repo_mut())
            .await
            .map_err(|e| anyhow!("importing git HEAD: {e}"))?;
        if !tx.repo().has_changes() {
            return Ok(());
        }
        let new_git_head = tx.repo().view().git_head().as_normal().cloned();
        if let Some(head_id) = new_git_head {
            let head_commit = tx
                .repo()
                .store()
                .get_commit(&head_id)
                .map_err(|e| anyhow!("loading new HEAD commit: {e}"))?;
            let wc_commit = tx
                .repo_mut()
                .check_out(self.workspace_name.clone(), &head_commit)
                .await
                .map_err(|e| anyhow!("checking out new HEAD: {e}"))?;
            let mut locked_ws = inner
                .workspace
                .start_working_copy_mutation()
                .await
                .map_err(|e| anyhow!("locking working copy: {e}"))?;
            locked_ws
                .locked_wc()
                .reset(&wc_commit)
                .await
                .map_err(|e| anyhow!("resetting working-copy state: {e}"))?;
            tx.repo_mut()
                .rebase_descendants()
                .await
                .map_err(|e| anyhow!("rebasing descendants: {e}"))?;
            let new_repo = tx
                .commit("import git head")
                .await
                .map_err(|e| anyhow!("committing git head import: {e}"))?;
            locked_ws
                .finish(new_repo.op_id().clone())
                .await
                .map_err(|e| anyhow!("finishing working-copy lock: {e}"))?;
            drop(new_repo);
        } else {
            let new_repo = tx
                .commit("import git head")
                .await
                .map_err(|e| anyhow!("committing git head import: {e}"))?;
            drop(new_repo);
        }
        self.reload_inner(&mut inner)?;
        Ok(())
    }

    /// Snapshot the working copy: capture on-disk edits into `@`, rewriting it within a new
    /// operation. No-op (beyond releasing the lock) if nothing changed. Updates the cached repo.
    async fn snapshot_working_copy(&self) -> Result<()> {
        self.import_head_op().await?;
        let repo = self.repo().await?;
        let mut inner = self.inner.lock().await;
        let wc_id = match repo.view().get_wc_commit_id(&self.workspace_name) {
            Some(id) => id.clone(),
            None => return Ok(()),
        };
        let wc_commit = repo
            .store()
            .get_commit(&wc_id)
            .map_err(|e| anyhow!("loading working-copy commit: {e}"))?;

        let mut locked_ws = inner
            .workspace
            .start_working_copy_mutation()
            .await
            .map_err(|e| anyhow!("locking working copy: {e}"))?;
        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: u64::MAX,
        };
        let (new_tree, _stats) = locked_ws
            .locked_wc()
            .snapshot(&options)
            .await
            .map_err(|e| anyhow!("snapshotting working copy: {e}"))?;

        if new_tree.tree_ids_and_labels() == wc_commit.tree().tree_ids_and_labels() {
            // Nothing changed on disk; release the lock against the current op without rewriting.
            locked_ws
                .finish(repo.op_id().clone())
                .await
                .map_err(|e| anyhow!("finishing working-copy lock: {e}"))?;
            return Ok(());
        }

        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.workspace_name);
        let mut_repo = tx.repo_mut();
        let new_commit = mut_repo
            .rewrite_commit(&wc_commit)
            .set_tree(new_tree)
            .write()
            .await
            .map_err(|e| anyhow!("rewriting @ with snapshot: {e}"))?;
        mut_repo
            .set_wc_commit(self.workspace_name.clone(), new_commit.id().clone())
            .map_err(|e| anyhow!("setting working-copy commit: {e}"))?;
        mut_repo
            .rebase_descendants()
            .await
            .map_err(|e| anyhow!("rebasing descendants: {e}"))?;
        // Keep the colocated git bookmark refs in sync, else a stale `<bookmark>@git` keeps the
        // rewritten commit visible and change-id resolution sees false divergence. (We deliberately
        // do NOT `reset_head` here: snapshotting must not reset the git index / wipe the user's
        // staging — that only happens on a real commit/checkout.)
        let _ = jj_lib::git::export_refs(tx.repo_mut());
        let new_repo = tx
            .commit("snapshot working copy")
            .await
            .map_err(|e| anyhow!("committing snapshot: {e}"))?;
        locked_ws
            .finish(new_repo.op_id().clone())
            .await
            .map_err(|e| anyhow!("finishing working-copy lock: {e}"))?;
        drop(new_repo);
        self.reload_inner(&mut inner)?;
        Ok(())
    }

    /// Run the configured diff editor over `before`→`after` and return the edited ("selected")
    /// tree. Honors `ui.diff-editor` (read fresh), opening the user's external/GUI tool or the
    /// built-in TUI — no `jj` binary involved.
    async fn run_diff_editor(
        &self,
        before: &jj_lib::merged_tree::MergedTree,
        after: &jj_lib::merged_tree::MergedTree,
        instructions: &str,
    ) -> Result<jj_lib::merged_tree::MergedTree> {
        use jj_cli::merge_tools::DiffEditor;
        use jj_lib::matchers::EverythingMatcher;
        use jj_lib::merge::Diff;
        let ui = jj_cli::ui::Ui::null();
        let settings = build_settings(&self.repo_path, &self.root)?;
        let marker_style = settings
            .get("ui.conflict-marker-style")
            .unwrap_or(jj_lib::conflicts::ConflictMarkerStyle::Diff);
        let editor = DiffEditor::from_settings(&ui, &settings, GitIgnoreFile::empty(), marker_style)
            .map_err(|e| anyhow!("configuring diff editor: {e}"))?;
        let instr = instructions.to_string();
        editor
            .edit(Diff { before, after }, &EverythingMatcher, || instr)
            .await
            .map_err(|e| anyhow!("running diff editor: {e}"))
    }

    /// Import git refs (after a `git fetch`/`push`) into jj's view, recording one operation.
    async fn import_git(&self) -> Result<()> {
        let repo = self.repo().await?;
        let mut inner = self.inner.lock().await;
        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.workspace_name);
        let opts = jj_lib::git::GitImportOptions {
            auto_local_bookmark: false,
            abandon_unreachable_commits: false,
            remote_auto_track_bookmarks: std::collections::HashMap::new(),
        };
        jj_lib::git::import_refs(tx.repo_mut(), &opts)
            .await
            .map_err(|e| anyhow!("importing git refs: {e}"))?;
        // Track the imported remote bookmarks that jjk manages locally. `jj git push` tracks pushed
        // bookmarks; without this they stay untracked, so `untracked_remote_bookmarks()` (part of
        // `immutable_heads()`) would make the pushed stack immutable and drop it from
        // `derive_stack`. Only track those with a matching LOCAL bookmark, so a deliberately
        // untracked/forgotten remote bookmark (e.g. an already-landed branch) stays immutable.
        let view = tx.repo().view();
        let to_track: Vec<jj_lib::ref_name::RemoteRefSymbolBuf> = view
            .all_remote_bookmarks()
            .filter(|(sym, rref)| {
                sym.remote.as_str() != "git"
                    && !rref.is_tracked()
                    && view.get_local_bookmark(sym.name).is_present()
            })
            .map(|(sym, _)| sym.to_owned())
            .collect();
        for sym in &to_track {
            let _ = tx.repo_mut().track_remote_bookmark(sym.as_ref());
        }
        let new_repo = tx
            .commit("import git refs")
            .await
            .map_err(|e| anyhow!("committing git import: {e}"))?;
        drop(new_repo);
        self.reload_inner(&mut inner)?;
        Ok(())
    }

    /// Restore the repo (commits, bookmarks, working copy) to an earlier operation — the core of
    /// `jj undo` / `jj op restore`. Records a new operation whose view is the target's.
    async fn restore_to_operation(&self, op: &jj_lib::operation::Operation) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let repo = match &inner.repo {
            Some(r) => r.clone(),
            None => inner
                .workspace
                .repo_loader()
                .load_at_head()
                .await
                .map_err(|e| anyhow!("loading repo at head: {e}"))?,
        };
        let target = repo
            .loader()
            .load_at(op)
            .await
            .map_err(|e| anyhow!("loading target operation: {e}"))?;
        let old_wc = repo
            .view()
            .get_wc_commit_id(&self.workspace_name)
            .and_then(|id| repo.store().get_commit(id).ok());

        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.workspace_name);
        tx.repo_mut().set_view(target.view().store_view().clone());
        let new_wc_id = tx
            .repo()
            .view()
            .get_wc_commit_id(&self.workspace_name)
            .cloned();
        if let Some(id) = &new_wc_id {
            if let Ok(c) = tx.repo().store().get_commit(id) {
                // Force the colocated git HEAD ref back to the restored position. `reset_head`
                // short-circuits when jj's *recorded* git-head already matches the target — but
                // after an op restore the actual git ref is stale (it points at the undone commit),
                // so clear the record first to make `reset_head` rewrite the real ref. Otherwise a
                // later `import_head` would revive the undone commit (divergent change id).
                tx.repo_mut().set_git_head_target(RefTarget::absent());
                let _ = jj_lib::git::reset_head(tx.repo_mut(), &c).await;
            }
        }
        let _ = jj_lib::git::export_refs(tx.repo_mut());
        let new_repo = tx
            .commit("restore operation")
            .await
            .map_err(|e| anyhow!("committing restore: {e}"))?;
        if let Some(id) = &new_wc_id {
            let new_wc = new_repo
                .store()
                .get_commit(id)
                .map_err(|e| anyhow!("loading restored @: {e}"))?;
            let old_tree = old_wc.as_ref().map(|c| c.tree());
            inner
                .workspace
                .check_out(new_repo.op_id().clone(), old_tree.as_ref(), &new_wc)
                .await
                .map_err(|e| anyhow!("checking out restored @: {e}"))?;
        }
        drop(new_repo);
        self.reload_inner(&mut inner)?;
        Ok(())
    }
}

/// Map a jj-lib [`Commit`] to a neutral [`CommitInfo`].
async fn commit_info(
    repo: &Arc<ReadonlyRepo>,
    c: &Commit,
    wc: Option<&JjCommitId>,
) -> Result<CommitInfo> {
    let view = repo.view();
    // Bookmarks pointing at this commit.
    let mut local_bookmarks = Vec::new();
    for (name, target) in view.local_bookmarks_for_commit(c.id()) {
        let _ = target;
        local_bookmarks.push(name.as_str().to_string());
    }
    let mut remote_bookmarks = Vec::new();
    for (sym, rref) in view.all_remote_bookmarks() {
        if sym.remote.as_str() == "git" {
            continue; // colocated git pseudo-remote (JJ_NOTES §0)
        }
        if rref.target.as_normal() == Some(c.id()) {
            remote_bookmarks.push(RemoteRef {
                name: sym.name.as_str().to_string(),
                remote: sym.remote.as_str().to_string(),
            });
        }
    }
    // Parent change ids (load each parent; the store caches commits).
    let mut parents = Vec::with_capacity(c.parent_ids().len());
    for pid in c.parent_ids() {
        let p = repo
            .store()
            .get_commit(pid)
            .map_err(|e| anyhow!("loading parent commit: {e}"))?;
        parents.push(ChangeId(p.change_id().reverse_hex()));
    }
    let is_empty = c
        .is_empty(repo.as_ref())
        .await
        .map_err(|e| anyhow!("computing emptiness: {e}"))?;
    Ok(CommitInfo {
        change_id: ChangeId(c.change_id().reverse_hex()),
        commit_id: CommitId(c.id().hex()),
        parents,
        local_bookmarks,
        remote_bookmarks,
        description: c.description().to_string(),
        time_ago: humanize_ago(c.committer().timestamp.timestamp.0),
        is_empty,
        has_conflict: c.has_conflict(),
        is_working_copy: wc == Some(c.id()),
        is_immutable: false,
    })
}

/// A coarse "N units ago" string from a millis-since-epoch timestamp (display only).
fn humanize_ago(millis: i64) -> String {
    use chrono::Utc;
    let now = Utc::now().timestamp_millis();
    let secs = ((now - millis).max(0) / 1000) as u64;
    let (n, unit) = if secs < 60 {
        (secs, "second")
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else if secs < 2_592_000 {
        (secs / 86_400, "day")
    } else if secs < 31_536_000 {
        (secs / 2_592_000, "month")
    } else {
        (secs / 31_536_000, "year")
    };
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// Whether `path` is an executable file (git only runs hooks that are executable).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run a `git` subprocess in the workspace; error (with stderr) on non-zero exit.
async fn git_run(root: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("failed to spawn git: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a `git` subprocess in the workspace, returning stdout (or `None` on failure).
async fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[async_trait(?Send)]
impl Vcs for JjLib {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            atomic_transactions: true,
            in_process: true,
        }
    }

    async fn trunk(&self) -> Result<ChangeId> {
        self.shared
            .resolve_infos("trunk()")
            .await?
            .into_iter()
            .next()
            .map(|c| c.change_id)
            .ok_or_else(|| anyhow!("trunk() resolved to nothing"))
    }

    async fn has_remote_trunk(&self) -> Result<bool> {
        Ok(!self.shared.resolve_infos("trunk() ~ root()").await?.is_empty())
    }

    async fn resolve(&self, revset: &str) -> Result<Vec<CommitInfo>> {
        self.shared.resolve_infos(revset).await
    }

    async fn working_copy(&self) -> Result<CommitInfo> {
        self.shared
            .resolve_infos("@")
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not resolve working-copy commit @"))
    }

    async fn snapshot(&self) -> Result<CommitInfo> {
        self.shared.snapshot_working_copy().await?;
        self.working_copy().await
    }

    async fn split_interactive(&self, rev: &ChangeId) -> Result<()> {
        let repo = self.shared.repo().await?;
        let c = self.shared.resolve_one(repo.as_ref(), rev.as_str()).await?;
        let parent_tree = c
            .parent_tree(repo.as_ref())
            .await
            .map_err(|e| anyhow!("reading parent tree: {e}"))?;
        // The user selects what goes into the FIRST (lower) commit; the rest stays in `rev`.
        let first_tree = self
            .shared
            .run_diff_editor(&parent_tree, &c.tree(), "Select changes for the first commit")
            .await?;
        if first_tree.tree_ids_and_labels() == parent_tree.tree_ids_and_labels() {
            return Ok(()); // nothing selected — no split
        }
        let mut inner = self.shared.inner.lock().await;
        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.shared.workspace_name);
        let first = tx
            .repo_mut()
            .new_commit(c.parent_ids().to_vec(), first_tree)
            .set_description(c.description())
            .write()
            .await
            .map_err(|e| anyhow!("creating first split commit: {e}"))?;
        // `rev` keeps its full tree and change id, reparented onto the first commit.
        tx.repo_mut()
            .rewrite_commit(&c)
            .set_parents(vec![first.id().clone()])
            .set_description("")
            .write()
            .await
            .map_err(|e| anyhow!("creating second split commit: {e}"))?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|e| anyhow!("rebasing descendants: {e}"))?;
        let new_wc = tx.repo().view().get_wc_commit_id(&self.shared.workspace_name).cloned();
        if let Some(id) = &new_wc {
            if let Ok(wc) = tx.repo().store().get_commit(id) {
                let _ = jj_lib::git::reset_head(tx.repo_mut(), &wc).await;
            }
        }
        let _ = jj_lib::git::export_refs(tx.repo_mut());
        let old_wc = repo
            .view()
            .get_wc_commit_id(&self.shared.workspace_name)
            .and_then(|id| repo.store().get_commit(id).ok());
        let new_repo = tx
            .commit("split commit")
            .await
            .map_err(|e| anyhow!("committing split: {e}"))?;
        if let Some(id) = &new_wc {
            if let Ok(wc) = new_repo.store().get_commit(id) {
                let old_tree = old_wc.as_ref().map(|c| c.tree());
                let _ = inner
                    .workspace
                    .check_out(new_repo.op_id().clone(), old_tree.as_ref(), &wc)
                    .await;
            }
        }
        drop(new_repo);
        self.shared.reload_inner(&mut inner)?;
        Ok(())
    }

    async fn bookmarks(&self) -> Result<Vec<Bookmark>> {
        let repo = self.shared.repo().await?;
        let mut out = Vec::new();
        for (name, target) in repo.view().local_bookmarks() {
            if let Some(id) = target.as_normal() {
                let c = repo
                    .store()
                    .get_commit(id)
                    .map_err(|e| anyhow!("loading bookmark target: {e}"))?;
                out.push(Bookmark {
                    name: name.as_str().to_string(),
                    target: ChangeId(c.change_id().reverse_hex()),
                });
            }
        }
        Ok(out)
    }

    async fn diff(&self, revset: &str) -> Result<String> {
        // Render via git between the two endpoints' trees (commit ids are git shas in a colocated
        // repo). jjk always passes `base..tip`.
        let repo = self.shared.repo().await?;
        let (base, tip) = revset.split_once("..").unwrap_or(("", revset));
        let tip_c = self.shared.resolve_one(repo.as_ref(), tip).await?;
        let tip_sha = tip_c.id().hex();
        let base_sha = if base.is_empty() {
            // diff against tip's first parent
            tip_c
                .parent_ids()
                .first()
                .map(|p| p.hex())
                .unwrap_or_else(|| tip_sha.clone())
        } else {
            self.shared.resolve_one(repo.as_ref(), base).await?.id().hex()
        };
        // jj's root commit has an all-zero id with no git object; diff against the git empty tree.
        const GIT_EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let base_sha = if base_sha.chars().all(|c| c == '0') {
            GIT_EMPTY_TREE.to_string()
        } else {
            base_sha
        };
        git_run(&self.shared.root, &["diff", &base_sha, &tip_sha]).await
    }

    async fn conflicted_paths(&self, rev: &ChangeId) -> Result<Vec<String>> {
        let repo = self.shared.repo().await?;
        let c = match self.shared.resolve_one(repo.as_ref(), rev.as_str()).await {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for (path, _) in c.tree().conflicts() {
            out.push(path.as_internal_file_string().to_string());
        }
        Ok(out)
    }

    async fn resolve_with_merge_tool(&self, rev: &ChangeId) -> Result<()> {
        use jj_cli::merge_tools::MergeEditor;
        use jj_lib::repo_path::RepoPath;
        let repo = self.shared.repo().await?;
        let c = self.shared.resolve_one(repo.as_ref(), rev.as_str()).await?;
        let tree = c.tree();
        let paths: Vec<jj_lib::repo_path::RepoPathBuf> =
            tree.conflicts().map(|(p, _)| p).collect();
        if paths.is_empty() {
            return Ok(());
        }
        let ui = jj_cli::ui::Ui::null();
        let path_converter = RepoPathUiConverter::Fs {
            cwd: self.shared.root.clone(),
            base: self.shared.root.clone(),
        };
        // Read settings fresh so a just-configured `ui.merge-editor` is honored.
        let settings = build_settings(&self.shared.repo_path, &self.shared.root)?;
        let marker_style = settings
            .get("ui.conflict-marker-style")
            .unwrap_or(jj_lib::conflicts::ConflictMarkerStyle::Diff);
        let editor = MergeEditor::from_settings(&ui, &settings, path_converter, marker_style)
            .map_err(|e| anyhow!("configuring merge editor: {e}"))?;
        let path_refs: Vec<&RepoPath> = paths.iter().map(|p| p.as_ref()).collect();
        let (new_tree, _partial) = editor
            .edit_files(&ui, &tree, &path_refs)
            .await
            .map_err(|e| anyhow!("running merge editor: {e}"))?;
        // Rewrite the change with the resolved tree (descendants auto-rebase, propagating the fix).
        let mut inner = self.shared.inner.lock().await;
        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.shared.workspace_name);
        tx.repo_mut()
            .rewrite_commit(&c)
            .set_tree(new_tree)
            .write()
            .await
            .map_err(|e| anyhow!("writing resolved tree: {e}"))?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|e| anyhow!("rebasing descendants: {e}"))?;
        let new_wc = tx.repo().view().get_wc_commit_id(&self.shared.workspace_name).cloned();
        if let Some(id) = &new_wc {
            if let Ok(wc) = tx.repo().store().get_commit(id) {
                let _ = jj_lib::git::reset_head(tx.repo_mut(), &wc).await;
            }
        }
        let _ = jj_lib::git::export_refs(tx.repo_mut());
        let new_repo = tx
            .commit("resolve conflicts")
            .await
            .map_err(|e| anyhow!("committing resolution: {e}"))?;
        // Update the working copy if the resolved change is (an ancestor of) @.
        if let Some(id) = &new_wc {
            if let Ok(wc) = new_repo.store().get_commit(id) {
                let old_tree = inner
                    .repo
                    .as_ref()
                    .and_then(|r| r.view().get_wc_commit_id(&self.shared.workspace_name).cloned())
                    .and_then(|oid| repo.store().get_commit(&oid).ok())
                    .map(|c| c.tree());
                let _ = inner
                    .workspace
                    .check_out(new_repo.op_id().clone(), old_tree.as_ref(), &wc)
                    .await;
            }
        }
        drop(new_repo);
        self.shared.reload_inner(&mut inner)?;
        Ok(())
    }

    async fn staged_paths(&self) -> Result<Vec<String>> {
        let out = git_out(&self.shared.root, &["diff", "--cached", "--name-only", "-z"]).await;
        Ok(out
            .unwrap_or_default()
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    async fn git_head(&self) -> Result<Option<String>> {
        let s = git_out(&self.shared.root, &["rev-parse", "--verify", "-q", "HEAD"]).await;
        Ok(s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
    }

    async fn set_git_head_branch(&self, branch: &str) -> Result<()> {
        let refname = format!("refs/heads/{branch}");
        let exists = git_out(&self.shared.root, &["show-ref", "--verify", "--quiet", &refname])
            .await
            .is_some();
        if exists {
            let _ = git_out(&self.shared.root, &["symbolic-ref", "HEAD", &refname]).await;
        }
        Ok(())
    }

    async fn begin_transaction(&self) -> Result<Box<dyn VcsTx + '_>> {
        let repo = self.shared.repo().await?;
        let mut txn = repo.start_transaction();
        txn.set_workspace_name(&self.shared.workspace_name);
        Ok(Box::new(JjLibTx {
            shared: self.shared.clone(),
            txn: Some(txn),
            created: std::collections::HashMap::new(),
        }))
    }

    async fn undo(&self) -> Result<String> {
        // Restore to the parent of the current head operation (`jj undo`).
        let repo = self.shared.repo().await?;
        let head = repo.operation().clone();
        let parent = head
            .parents()
            .await
            .map_err(|e| anyhow!("reading op parents: {e}"))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("nothing to undo (at the root operation)"))?;
        self.shared.restore_to_operation(&parent).await?;
        Ok("Undid operation".to_string())
    }

    async fn current_op_id(&self) -> Result<String> {
        // Snapshot first so pending edits are part of the returned op (faithful "before" point).
        self.shared.snapshot_working_copy().await?;
        let repo = self.shared.repo().await?;
        Ok(repo.op_id().hex())
    }

    async fn restore_op(&self, op_id: &str) -> Result<String> {
        let repo = self.shared.repo().await?;
        let op = jj_lib::op_walk::resolve_op_with_repo(repo.as_ref(), op_id)
            .await
            .map_err(|e| anyhow!("resolving operation {op_id}: {e}"))?;
        self.shared.restore_to_operation(&op).await?;
        Ok("Restored to operation".to_string())
    }

    async fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let repo = self.shared.repo().await?;
        let current_stale = self.is_stale().await.unwrap_or(false);
        let current = self.shared.workspace_name.as_str();
        let mut out = Vec::new();
        for (name, commit_id) in repo.view().wc_commit_ids() {
            let c = repo
                .store()
                .get_commit(commit_id)
                .map_err(|e| anyhow!("loading workspace @: {e}"))?;
            let nm = name.as_str().to_string();
            let is_stale = current_stale && nm == current;
            out.push(WorkspaceInfo {
                name: nm,
                working_copy: ChangeId(c.change_id().reverse_hex()),
                is_stale,
            });
        }
        Ok(out)
    }

    async fn workspace_count(&self) -> Result<usize> {
        let repo = self.shared.repo().await?;
        Ok(repo.view().wc_commit_ids().len())
    }

    async fn add_workspace(&self, path: &Path, name: &str, at: &ChangeId) -> Result<()> {
        let repo = self.shared.repo().await?;
        let mut inner = self.shared.inner.lock().await;
        let at_commit = self.shared.resolve_one(repo.as_ref(), at.as_str()).await?;
        let factories = default_working_copy_factories();
        let wc_factory = factories
            .get("local")
            .ok_or_else(|| anyhow!("no local working-copy factory"))?
            .as_ref();
        let repo_path = inner.workspace.repo_path().to_owned();
        std::fs::create_dir_all(path).map_err(|e| anyhow!("creating workspace dir: {e}"))?;
        let name_buf = WorkspaceNameBuf::from(name);
        let (mut new_ws, repo2) = Workspace::init_workspace_with_existing_repo(
            path,
            &repo_path,
            &repo,
            wc_factory,
            name_buf.clone(),
        )
        .await
        .map_err(|e| anyhow!("creating workspace: {e}"))?;
        // Start the new workspace's `@` as an empty child of `at`.
        let mut tx = repo2.start_transaction();
        tx.set_workspace_name(&name_buf);
        let new_wc = tx
            .repo_mut()
            .new_commit(vec![at_commit.id().clone()], at_commit.tree())
            .write()
            .await
            .map_err(|e| anyhow!("creating workspace @: {e}"))?;
        tx.repo_mut()
            .edit(name_buf.clone(), &new_wc)
            .await
            .map_err(|e| anyhow!("setting workspace @: {e}"))?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|e| anyhow!("rebasing descendants: {e}"))?;
        let new_repo = tx
            .commit("create workspace")
            .await
            .map_err(|e| anyhow!("committing workspace: {e}"))?;
        new_ws
            .check_out(new_repo.op_id().clone(), None, &new_wc)
            .await
            .map_err(|e| anyhow!("checking out new workspace: {e}"))?;
        self.shared.reload_inner(&mut inner)?;
        Ok(())
    }

    async fn forget_workspace(&self, name: &str) -> Result<()> {
        let repo = self.shared.repo().await?;
        let mut inner = self.shared.inner.lock().await;
        let mut tx = repo.start_transaction();
        tx.set_workspace_name(&self.shared.workspace_name);
        tx.repo_mut()
            .remove_wc_commit(jj_lib::ref_name::WorkspaceName::new(name))
            .await
            .map_err(|e| anyhow!("forgetting workspace: {e}"))?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|e| anyhow!("rebasing descendants: {e}"))?;
        let new_repo = tx
            .commit("forget workspace")
            .await
            .map_err(|e| anyhow!("committing workspace forget: {e}"))?;
        drop(new_repo);
        self.shared.reload_inner(&mut inner)?;
        Ok(())
    }

    async fn update_stale(&self) -> Result<()> {
        let repo = self.shared.repo().await?;
        let mut inner = self.shared.inner.lock().await;
        let wc_id = match repo.view().get_wc_commit_id(&self.shared.workspace_name) {
            Some(id) => id.clone(),
            None => return Ok(()),
        };
        let wc_commit = repo
            .store()
            .get_commit(&wc_id)
            .map_err(|e| anyhow!("loading @ for stale update: {e}"))?;
        let mut locked_ws = inner
            .workspace
            .start_working_copy_mutation()
            .await
            .map_err(|e| anyhow!("locking working copy: {e}"))?;
        locked_ws
            .locked_wc()
            .check_out(&wc_commit)
            .await
            .map_err(|e| anyhow!("recovering stale working copy: {e}"))?;
        locked_ws
            .finish(repo.op_id().clone())
            .await
            .map_err(|e| anyhow!("finishing stale recovery: {e}"))?;
        self.shared.reload_inner(&mut inner)?;
        Ok(())
    }

    async fn is_stale(&self) -> Result<bool> {
        let repo = self.shared.repo().await?;
        let mut inner = self.shared.inner.lock().await;
        let wc_id = match repo.view().get_wc_commit_id(&self.shared.workspace_name) {
            Some(id) => id.clone(),
            None => return Ok(false),
        };
        let wc_commit = repo
            .store()
            .get_commit(&wc_id)
            .map_err(|e| anyhow!("loading @ for staleness check: {e}"))?;
        let mut locked_ws = inner
            .workspace
            .start_working_copy_mutation()
            .await
            .map_err(|e| anyhow!("locking working copy: {e}"))?;
        let freshness = WorkingCopyFreshness::check_stale(locked_ws.locked_wc(), &wc_commit, &repo)
            .await
            .map_err(|e| anyhow!("checking staleness: {e}"))?;
        // Release the lock without persisting (we only inspected).
        drop(locked_ws);
        Ok(!matches!(freshness, WorkingCopyFreshness::Fresh))
    }

    async fn fetch(&self, remote: &str, branch: Option<&str>) -> Result<()> {
        // jj-lib's git fetch drives the `git` subprocess anyway; do the fetch directly into the
        // colocated repo, then import the updated refs into jj's view.
        let mut args = vec!["fetch".to_string(), remote.to_string()];
        if let Some(b) = branch {
            // Only the trunk bookmark (stacking only needs trunk advanced).
            args.push(format!(
                "+refs/heads/{b}:refs/remotes/{remote}/{b}"
            ));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = git_run(&self.shared.root, &argv).await {
            // Tolerate a not-yet-existing trunk on the remote (jj's `git fetch --branch` is a no-op
            // in that case); surface anything else.
            if !e.to_string().contains("couldn't find remote ref") {
                return Err(e);
            }
        }
        self.shared.import_git().await
    }

    async fn push(&self, remote: &str, bookmark: &str, opts: PushOpts) -> Result<()> {
        if opts.delete {
            // Best-effort: the branch may not exist on the remote yet.
            let _ = git_run(&self.shared.root, &["push", remote, "--delete", bookmark]).await;
        } else {
            let refspec = format!("refs/heads/{bookmark}:refs/heads/{bookmark}");
            git_run(
                &self.shared.root,
                &["push", "--force-with-lease", remote, &refspec],
            )
            .await?;
        }
        self.shared.import_git().await
    }

    async fn push_deleted(&self, remote: &str) -> Result<()> {
        // Remote bookmarks that are tracked but have no local bookmark are pending deletions.
        let repo = self.shared.repo().await?;
        let rname = RemoteName::new(remote);
        let to_delete: Vec<String> = repo
            .view()
            .local_remote_bookmarks(rname)
            .filter(|(_, pair)| pair.local_target.is_absent() && pair.remote_ref.is_tracked())
            .map(|(name, _)| name.as_str().to_string())
            .collect();
        for b in &to_delete {
            let _ = git_run(&self.shared.root, &["push", remote, "--delete", b]).await;
        }
        if !to_delete.is_empty() {
            self.shared.import_git().await?;
        }
        Ok(())
    }

    async fn run_pre_commit_hook(&self, scope: &CommitScope) -> Result<()> {
        use std::process::Command;
        let root = &self.shared.root;
        // Resolve the hook path (honouring core.hooksPath).
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--git-path", "hooks/pre-commit"])
            .output()
            .map_err(|e| anyhow!("failed to spawn git: {e}"))?;
        if !out.status.success() {
            return Ok(());
        }
        let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let hook = root.join(rel);
        if !is_executable(&hook) {
            return Ok(());
        }
        // Stage the in-scope changes so index-based hooks see exactly what will be committed.
        let mut add = Command::new("git");
        add.arg("-C").arg(root).arg("add");
        match scope {
            CommitScope::Paths(paths) => {
                add.arg("--");
                add.args(paths);
            }
            CommitScope::All | CommitScope::Interactive => {
                add.arg("-A");
            }
        }
        let _ = add.output();
        let status = Command::new(&hook)
            .current_dir(root)
            .status()
            .map_err(|e| anyhow!("failed to run pre-commit hook {}: {e}", hook.display()))?;
        if !status.success() {
            return Err(anyhow!(
                "pre-commit hook failed (exit {}); commit `--no-verify`/`-n` to skip",
                status.code().unwrap_or(-1)
            ));
        }
        Ok(())
    }

    async fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        git_out(&self.shared.root, &["remote", "add", name, url])
            .await
            .ok_or_else(|| anyhow!("git remote add {name} failed"))?;
        // jj imports git remotes lazily; invalidate so the next op sees it.
        self.shared.invalidate().await;
        Ok(())
    }

    async fn remotes(&self) -> Result<Vec<String>> {
        let out = git_out(&self.shared.root, &["remote"]).await.unwrap_or_default();
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|r| !r.is_empty() && r != "git")
            .collect())
    }

    async fn remote_url(&self, name: &str) -> Result<Option<String>> {
        let out = git_out(&self.shared.root, &["remote", "get-url", name]).await;
        Ok(out.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
    }

    async fn config_get(&self, key: &str) -> Result<Option<String>> {
        let path: jj_lib::config::ConfigNamePathBuf = match key.parse() {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        // Re-read config fresh from disk: a value may have been set since the adapter opened
        // (e.g. `repo init` / `set_config_repo` within the same process).
        let settings = build_settings(&self.shared.repo_path, &self.shared.root)?;
        Ok(settings.get_string(&path).ok())
    }

    async fn set_config_repo(&self, key: &str, value: &str) -> Result<()> {
        use jj_lib::config::{ConfigFile, ConfigNamePathBuf, ConfigSource};
        let name: ConfigNamePathBuf = key
            .parse()
            .map_err(|e| anyhow!("invalid config key {key}: {e}"))?;
        let path = self.shared.root.join(".jj").join("repo").join("config.toml");
        let mut file = ConfigFile::load_or_empty(ConfigSource::Repo, path)
            .map_err(|e| anyhow!("loading repo config: {e}"))?;
        file.set_value(&name, value)
            .map_err(|e| anyhow!("setting {key}: {e}"))?;
        file.save().map_err(|e| anyhow!("saving repo config: {e}"))?;
        Ok(())
    }
}

/// Active transaction handle: owns a live jj-lib [`Transaction`] and a handle to the shared state
/// so [`commit`](VcsTx::commit) can update the on-disk working copy and cached repo.
struct JjLibTx {
    shared: Arc<Shared>,
    txn: Option<Transaction>,
    /// Change ids created or rewritten in this transaction, mapped to their current commit. A
    /// rewrite leaves the obsolete predecessor in the in-progress index, so re-resolving that
    /// change id by symbol would see false divergence; this records the authoritative target.
    created: std::collections::HashMap<String, JjCommitId>,
}

impl JjLibTx {
    fn mr(&mut self) -> &mut MutableRepo {
        self.txn.as_mut().expect("transaction is live").repo_mut()
    }

    /// Record that `change id` now resolves to `commit` within this transaction.
    fn note(&mut self, commit: &Commit) {
        self.created
            .insert(commit.change_id().reverse_hex(), commit.id().clone());
    }

    /// Resolve a revset (e.g. a change id) against the in-progress transaction's repo, preferring
    /// commits this transaction just created/rewrote (see [`created`]).
    async fn resolve(&self, revset: &str) -> Result<Commit> {
        if let Some(id) = self.created.get(revset) {
            let repo: &dyn Repo = self.txn.as_ref().expect("transaction is live").repo();
            return repo
                .store()
                .get_commit(id)
                .map_err(|e| anyhow!("loading {revset}: {e}"));
        }
        let repo: &dyn Repo = self.txn.as_ref().expect("transaction is live").repo();
        self.shared.resolve_one(repo, revset).await
    }

    async fn resolve_all(&self, revset: &str) -> Result<Vec<Commit>> {
        let repo: &dyn Repo = self.txn.as_ref().expect("transaction is live").repo();
        self.shared.eval(repo, revset).await
    }

    /// `jj commit <paths>`: finalize only `paths` from `@` into a new `@-`; the rest stays in `@`.
    async fn finalize_paths(&mut self, message: &str, paths: &[String]) -> Result<ChangeId> {
        use jj_lib::matchers::FilesMatcher;
        use jj_lib::repo_path::RepoPathBuf;
        let wc = self.resolve("@").await?;
        let repo: &dyn Repo = self.txn.as_ref().expect("tx live").repo();
        let parent_tree = wc
            .parent_tree(repo)
            .await
            .map_err(|e| anyhow!("reading parent tree: {e}"))?;
        let mut repo_paths = Vec::with_capacity(paths.len());
        for p in paths {
            repo_paths.push(
                RepoPathBuf::from_internal_string(p.clone())
                    .map_err(|e| anyhow!("invalid path {p}: {e}"))?,
            );
        }
        let matcher = FilesMatcher::new(repo_paths);
        // The finalized tree is the parent tree with the selected paths taken from `@`.
        let finalized_tree = jj_lib::rewrite::restore_tree(
            &wc.tree(),
            &parent_tree,
            "@".to_string(),
            "@-".to_string(),
            &matcher,
        )
        .await
        .map_err(|e| anyhow!("selecting paths: {e}"))?;
        self.finalize_subset(message, &wc, finalized_tree).await
    }

    /// `jj commit -i`: pick which changes to finalize via the configured diff editor.
    async fn finalize_interactive(&mut self, message: &str) -> Result<ChangeId> {
        let wc = self.resolve("@").await?;
        let repo: &dyn Repo = self.txn.as_ref().expect("tx live").repo();
        let parent_tree = wc
            .parent_tree(repo)
            .await
            .map_err(|e| anyhow!("reading parent tree: {e}"))?;
        let wc_tree = wc.tree();
        let finalized_tree = self
            .shared
            .run_diff_editor(&parent_tree, &wc_tree, "Select changes to commit")
            .await?;
        self.finalize_subset(message, &wc, finalized_tree).await
    }

    /// Insert a new `@-` with `finalized_tree`, leaving `@` (full tree) reparented on top of it.
    async fn finalize_subset(
        &mut self,
        message: &str,
        wc: &Commit,
        finalized_tree: jj_lib::merged_tree::MergedTree,
    ) -> Result<ChangeId> {
        let parents = wc.parent_ids().to_vec();
        let name = self.shared.workspace_name.clone();
        let mr = self.mr();
        let finalized = mr
            .new_commit(parents, finalized_tree)
            .set_description(message)
            .write()
            .await
            .map_err(|e| anyhow!("creating partial commit: {e}"))?;
        let new_wc = mr
            .rewrite_commit(wc)
            .set_parents(vec![finalized.id().clone()])
            .write()
            .await
            .map_err(|e| anyhow!("reparenting @: {e}"))?;
        mr.set_wc_commit(name, new_wc.id().clone())
            .map_err(|e| anyhow!("setting @: {e}"))?;
        self.note(&finalized);
        self.note(&new_wc);
        Ok(ChangeId(finalized.change_id().reverse_hex()))
    }

}

#[async_trait(?Send)]
impl VcsTx for JjLibTx {
    async fn finalize_working_copy(&mut self, message: &str) -> Result<ChangeId> {
        self.finalize_working_copy_scoped(message, &CommitScope::All).await
    }

    async fn finalize_working_copy_scoped(
        &mut self,
        message: &str,
        scope: &CommitScope,
    ) -> Result<ChangeId> {
        if let CommitScope::Paths(paths) = scope {
            return self.finalize_paths(message, paths).await;
        }
        if matches!(scope, CommitScope::Interactive) {
            return self.finalize_interactive(message).await;
        }
        // `@` was snapshotted by the caller, so it already contains the on-disk edits. Finalizing
        // = give `@` the description, then open a fresh empty child as the new `@`.
        let wc = self.resolve("@").await?;
        let name = self.shared.workspace_name.clone();
        let mr = self.mr();
        let finalized = mr
            .rewrite_commit(&wc)
            .set_description(message)
            .write()
            .await
            .map_err(|e| anyhow!("finalizing @: {e}"))?;
        let new_wc = mr
            .new_commit(vec![finalized.id().clone()], finalized.tree())
            .write()
            .await
            .map_err(|e| anyhow!("opening fresh @: {e}"))?;
        mr.set_wc_commit(name, new_wc.id().clone())
            .map_err(|e| anyhow!("setting @: {e}"))?;
        self.note(&finalized);
        self.note(&new_wc);
        Ok(ChangeId(finalized.change_id().reverse_hex()))
    }

    async fn describe(&mut self, rev: &ChangeId, message: &str) -> Result<()> {
        let c = self.resolve(rev.as_str()).await?;
        let new = self
            .mr()
            .rewrite_commit(&c)
            .set_description(message)
            .write()
            .await
            .map_err(|e| anyhow!("describing {}: {e}", rev.short()))?;
        self.note(&new);
        Ok(())
    }

    async fn squash_working_into(&mut self, into: &ChangeId) -> Result<()> {
        // Amend: `@` is a child of `into` containing `into`'s content plus the edits, so `@`'s tree
        // is exactly the amended tip. Rewrite `into` to that tree, abandon `@`, open a fresh `@`.
        let at = self.resolve("@").await?;
        let into_c = self.resolve(into.as_str()).await?;
        let name = self.shared.workspace_name.clone();
        let mr = self.mr();
        let new_tip = mr
            .rewrite_commit(&into_c)
            .set_tree(at.tree())
            .write()
            .await
            .map_err(|e| anyhow!("amending {}: {e}", into.short()))?;
        let new_wc = mr
            .new_commit(vec![new_tip.id().clone()], new_tip.tree())
            .write()
            .await
            .map_err(|e| anyhow!("opening fresh @: {e}"))?;
        mr.set_wc_commit(name, new_wc.id().clone())
            .map_err(|e| anyhow!("setting @: {e}"))?;
        mr.record_abandoned_commit(&at);
        self.note(&new_tip);
        self.note(&new_wc);
        Ok(())
    }

    async fn squash(&mut self, from: &ChangeId, into: &ChangeId) -> Result<()> {
        let from_c = self.resolve(from.as_str()).await?;
        let into_c = self.resolve(into.as_str()).await?;
        let repo: &dyn Repo = self.txn.as_ref().expect("tx live").repo();
        let parent_tree = from_c
            .parent_tree(repo)
            .await
            .map_err(|e| anyhow!("reading parent tree: {e}"))?;
        let cws = CommitWithSelection {
            commit: from_c.clone(),
            selected_tree: from_c.tree(),
            parent_tree,
        };
        let mr = self.mr();
        if let Some(sq) = squash_commits(mr, &[cws], &into_c, false)
            .await
            .map_err(|e| anyhow!("squashing: {e}"))?
        {
            let new = sq
                .commit_builder
                .write()
                .await
                .map_err(|e| anyhow!("writing squashed commit: {e}"))?;
            self.note(&new);
        }
        Ok(())
    }

    async fn squash_revset(&mut self, from_revset: &str, into: &ChangeId) -> Result<()> {
        let into_c = self.resolve(into.as_str()).await?;
        let sources = self.resolve_all(from_revset).await?;
        if sources.is_empty() {
            return Ok(());
        }
        let repo: &dyn Repo = self.txn.as_ref().expect("tx live").repo();
        let mut selections = Vec::with_capacity(sources.len());
        for c in &sources {
            let parent_tree = c
                .parent_tree(repo)
                .await
                .map_err(|e| anyhow!("reading parent tree: {e}"))?;
            selections.push(CommitWithSelection {
                commit: c.clone(),
                selected_tree: c.tree(),
                parent_tree,
            });
        }
        let mr = self.mr();
        if let Some(sq) = squash_commits(mr, &selections, &into_c, false)
            .await
            .map_err(|e| anyhow!("squashing range: {e}"))?
        {
            let new = sq
                .commit_builder
                .write()
                .await
                .map_err(|e| anyhow!("writing squashed commit: {e}"))?;
            self.note(&new);
        }
        Ok(())
    }

    async fn rename_bookmark(&mut self, old: &str, new: &str) -> Result<()> {
        let old_ref = RefNameBuf::from(old);
        let new_ref = RefNameBuf::from(new);
        let mr = self.mr();
        let target = mr.get_local_bookmark(&old_ref);
        if target.is_absent() {
            anyhow::bail!("branch '{old}' does not exist");
        }
        let target = target.clone();
        mr.set_local_bookmark_target(&new_ref, target);
        mr.set_local_bookmark_target(&old_ref, RefTarget::absent());
        Ok(())
    }

    async fn duplicate_after(&mut self, rev: &ChangeId, after: &ChangeId) -> Result<()> {
        let rev_c = self.resolve(rev.as_str()).await?;
        let after_c = self.resolve(after.as_str()).await?;
        let children = self
            .resolve_all(&format!("children({})", after.as_str()))
            .await?;
        let child_ids: Vec<JjCommitId> = children.iter().map(|c| c.id().clone()).collect();
        let descs = std::collections::HashMap::from([(
            rev_c.id().clone(),
            rev_c.description().to_string(),
        )]);
        let mr = self.mr();
        jj_lib::rewrite::duplicate_commits(
            mr,
            &[rev_c.id().clone()],
            &descs,
            &[after_c.id().clone()],
            &child_ids,
        )
        .await
        .map_err(|e| anyhow!("duplicating {}: {e}", rev.short()))?;
        Ok(())
    }

    async fn new_child(&mut self, parent: &ChangeId) -> Result<ChangeId> {
        let p = self.resolve(parent.as_str()).await?;
        let name = self.shared.workspace_name.clone();
        let mr = self.mr();
        let child = mr
            .new_commit(vec![p.id().clone()], p.tree())
            .write()
            .await
            .map_err(|e| anyhow!("creating child of {}: {e}", parent.short()))?;
        mr.set_wc_commit(name, child.id().clone())
            .map_err(|e| anyhow!("setting @: {e}"))?;
        self.note(&child);
        Ok(ChangeId(child.change_id().reverse_hex()))
    }

    async fn edit(&mut self, rev: &ChangeId) -> Result<()> {
        let c = self.resolve(rev.as_str()).await?;
        let name = self.shared.workspace_name.clone();
        self.mr()
            .edit(name, &c)
            .await
            .map_err(|e| anyhow!("editing {}: {e}", rev.short()))?;
        Ok(())
    }

    async fn create_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()> {
        let c = self.resolve(target.as_str()).await?;
        let refname = RefNameBuf::from(name);
        let mr = self.mr();
        if mr.get_local_bookmark(&refname).is_present() {
            anyhow::bail!("branch '{name}' already exists");
        }
        mr.set_local_bookmark_target(&refname, RefTarget::normal(c.id().clone()));
        Ok(())
    }

    async fn set_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()> {
        let c = self.resolve(target.as_str()).await?;
        let refname = RefNameBuf::from(name);
        self.mr()
            .set_local_bookmark_target(&refname, RefTarget::normal(c.id().clone()));
        Ok(())
    }

    async fn delete_bookmark(&mut self, name: &str) -> Result<()> {
        let refname = RefNameBuf::from(name);
        self.mr()
            .set_local_bookmark_target(&refname, RefTarget::absent());
        Ok(())
    }

    async fn forget_bookmark(&mut self, name: &str) -> Result<()> {
        let refname = RefNameBuf::from(name);
        self.mr()
            .set_local_bookmark_target(&refname, RefTarget::absent());
        Ok(())
    }

    async fn rebase(&mut self, source: &ChangeId, dest: &ChangeId) -> Result<()> {
        let src = self.resolve(source.as_str()).await?;
        let dst = self.resolve(dest.as_str()).await?;
        let mr = self.mr();
        let new = jj_lib::rewrite::rebase_commit(mr, src, vec![dst.id().clone()])
            .await
            .map_err(|e| anyhow!("rebasing {}: {e}", source.short()))?;
        self.note(&new);
        Ok(())
    }

    async fn abandon(&mut self, revs: &[ChangeId]) -> Result<()> {
        if revs.is_empty() {
            return Ok(());
        }
        let mut commits = Vec::with_capacity(revs.len());
        for r in revs {
            commits.push(self.resolve(r.as_str()).await?);
        }
        let mr = self.mr();
        for c in &commits {
            // Delete any local bookmarks on the abandoned commit (jj's `abandon` deletes them
            // rather than sliding them to the parent — JJ_NOTES §2).
            let names: Vec<RefNameBuf> = mr
                .view()
                .local_bookmarks_for_commit(c.id())
                .map(|(n, _)| n.to_owned())
                .collect();
            for n in &names {
                mr.set_local_bookmark_target(n, RefTarget::absent());
            }
            mr.record_abandoned_commit(c);
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        let mut txn = self.txn.take().expect("transaction is live");
        let name = &self.shared.workspace_name;
        txn.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|e| anyhow!("rebasing descendants: {e}"))?;

        // Capture old/new working-copy commits for the on-disk checkout.
        let base_repo = txn.base_repo().clone();
        let old_wc = base_repo
            .view()
            .get_wc_commit_id(name)
            .and_then(|id| base_repo.store().get_commit(id).ok());
        let new_wc_id = txn.repo().view().get_wc_commit_id(name).cloned();
        let new_wc = new_wc_id
            .as_ref()
            .and_then(|id| txn.repo().store().get_commit(id).ok());

        // Keep the colocated git repo in sync: point git HEAD at the new `@` and export bookmarks
        // to git refs, so plain `git` and our git-subprocess reads (push, HEAD) see jj's state.
        if let Some(wc) = &new_wc {
            let _ = jj_lib::git::reset_head(txn.repo_mut(), wc).await;
        }
        let _ = jj_lib::git::export_refs(txn.repo_mut());

        let new_repo = txn
            .commit("jjk")
            .await
            .map_err(|e| anyhow!("committing transaction: {e}"))?;

        let mut inner = self.shared.inner.lock().await;
        if let Some(new_wc) = &new_wc {
            let old_tree = old_wc.as_ref().map(|c| c.tree());
            inner
                .workspace
                .check_out(new_repo.op_id().clone(), old_tree.as_ref(), new_wc)
                .await
                .map_err(|e| anyhow!("updating working copy: {e}"))?;
        }
        // Reload from disk: the reused in-memory index would otherwise treat the rewritten
        // commits as still-visible, making change-id symbol resolution see false divergence.
        drop(new_repo);
        self.shared.reload_inner(&mut inner)?;
        Ok(())
    }
}
