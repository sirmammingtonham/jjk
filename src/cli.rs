//! CLI vocabulary (clap derive). Pure parsing — no logic. git/git-spice verbs map to engine calls.

use crate::engine::expansion::Mode;
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "jjk",
    version,
    about = "Familiar git commands and a stacking workflow over Jujutsu (jj) for stacked GitHub PRs"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Repository setup (init/colocate).
    #[command(subcommand)]
    Repo(RepoCmd),

    /// Commit the current changes (optionally amend the branch tip).
    Commit(CommitArgs),

    /// Switch branches, or create one with -b.
    Checkout(CheckoutArgs),

    /// Stack-tracked branch management.
    #[command(subcommand)]
    Branch(BranchCmd),

    /// Convert a branch to tracked (defaults to the current branch).
    Track(OptNameArg),
    /// Convert a branch to untracked (defaults to the current branch).
    Untrack(OptNameArg),

    /// Working-copy status with stack position.
    Status,
    /// Stack diagram (branches only).
    Ls,
    /// Stack diagram with each branch's commits (log long).
    Ll,

    /// Move up one branch (away from trunk).
    Up,
    /// Move down one branch (toward trunk).
    Down,
    /// Jump to the top of the stack.
    Top,
    /// Jump to the bottom of the stack.
    Bottom,
    /// Switch to the trunk branch.
    Trunk,

    /// Undo the last jjk command as one unit (restores via jj's op-log); repeat to go further back.
    Undo,

    /// Manage parallel working trees (jj workspaces).
    #[command(subcommand)]
    Worktree(WorktreeCmd),

    /// Park/unpark working-copy changes (muscle memory; switching is safe in jj).
    Stash(StashArgs),

    /// Resolve conflicts step by step: open the lowest conflicted change, `--continue` to advance
    /// once fixed (and finish back on your branch), or `--abort` to undo the sync.
    Resolve(ResolveArgs),

    /// Restack the upstack (usually a no-op with jj).
    Restack,

    /// Fetch from the remote.
    Fetch,
    /// Pull trunk and rebase the current stack onto it.
    Pull,
    /// Push the current branch.
    Push,
    /// Inspect the current branch's pull request.
    #[command(subcommand)]
    Pr(PrCmd),
    /// Create/update PRs for the whole stack (bottom-up, correctly based).
    Submit(SubmitArgs),
    /// Submit the current branch and everything above it.
    #[command(subcommand)]
    Upstack(UpstackCmd),
    /// Submit the current branch and everything below it.
    #[command(subcommand)]
    Downstack(DownstackCmd),
    /// Fetch trunk, reconcile merged branches, rebase the stack, then push & retarget PRs.
    Sync(SyncArgs),

    /// Experimental: auto-stack a single monolith branch into reviewable PRs (Domain Expansion).
    #[command(subcommand)]
    Domain(DomainCmd),

    /// Friendly note: jjk has no staging area of its own but honors git's (D1).
    Add,
}

#[derive(Subcommand, Debug)]
pub enum DomainCmd {
    /// Activate domain expansion on the current branch (the monolith).
    Expansion(DomainExpansionArgs),
    /// Show the monolith ↔ layer-stack mapping (read-only).
    Status,
    /// Explain each layer's contents and the split rationale (read-only).
    Explain(DomainExplainArgs),
    /// (Re)build the layer stack locally for inspection — no PRs.
    Expand(DomainExpandArgs),
    /// Deactivate: forget the layer bookmarks, keep the monolith.
    Collapse,
}

#[derive(Args, Debug)]
pub struct DomainExpansionArgs {
    /// How to split: feature (few coherent PRs), change (self-contained), or slice (many thin).
    #[arg(long, value_enum, default_value_t = Mode::Change)]
    pub mode: Mode,
    /// Extra natural-language splitting guidance for the LLM.
    #[arg(long)]
    pub instruction: Option<String>,
    /// Build/test command to verify each layer is self-contained (opt-in hard gate).
    #[arg(long)]
    pub verify: Option<String>,
}

#[derive(Args, Debug)]
pub struct DomainExplainArgs {
    /// Limit to a single layer (by slug); default shows all.
    pub layer: Option<String>,
}

#[derive(Args, Debug)]
pub struct DomainExpandArgs {
    /// Print the proposed split without materializing layer bookmarks.
    #[arg(long)]
    pub preview: bool,
}

#[derive(Subcommand, Debug)]
pub enum RepoCmd {
    /// Initialize a colocated jjk repo here.
    Init(RepoInitArgs),
}

#[derive(Args, Debug)]
pub struct RepoInitArgs {
    /// Trunk bookmark name (default: detect main/master, else "main").
    #[arg(long)]
    pub trunk: Option<String>,
    /// Remote name (default: first configured remote, else "origin").
    #[arg(long)]
    pub remote: Option<String>,
}

#[derive(Args, Debug)]
pub struct CommitArgs {
    /// Commit message.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Amend the branch tip instead of creating a new commit.
    #[arg(long)]
    pub amend: bool,
    /// Fold the working-copy changes into this branch's tip (an older commit downstack).
    #[arg(long, value_name = "BRANCH")]
    pub fixup: Option<String>,
    /// Split the branch tip into two commits (interactive diff editor).
    #[arg(long)]
    pub split: bool,
    /// Copy a commit (e.g. from an upstack branch) onto the current branch.
    #[arg(long, value_name = "REV")]
    pub pick: Option<String>,
    /// Interactively choose what to commit (diff editor); the rest stays uncommitted.
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Skip the git pre-commit hook.
    #[arg(short = 'n', long)]
    pub no_verify: bool,
    /// Commit only these paths; the rest stays uncommitted. With no paths and no `-i`, jjk commits
    /// the git-staged files if any are staged, otherwise the whole working copy.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
pub struct CheckoutArgs {
    /// Branch name.
    pub name: String,
    /// Create a new (untracked) branch instead of switching.
    #[arg(short = 'b')]
    pub create: bool,
}

#[derive(Subcommand, Debug)]
pub enum BranchCmd {
    /// Create a new stack-tracked branch on top of the current one.
    Create(NameArg),
    /// Delete a branch and heal the stack.
    Delete(NameArg),
    /// Move the current branch (and its upstack) onto a new base.
    Onto(NameArg),
    /// Rename a branch: `rename <new>` (current) or `rename <old> <new>`.
    Rename(RenameArgs),
    /// Show the current branch's diff against its base.
    Diff,
    /// Collapse all of the current branch's commits into one.
    Squash(MessageArg),
    /// Fold the current branch into its downstack base.
    Fold,
    /// Split the current branch at a commit into two branches.
    Split(BranchSplitArgs),
    /// Create/update the PR for just the current branch.
    Submit(SubmitArgs),
}

#[derive(Args, Debug, Default, Clone)]
pub struct SubmitArgs {
    /// Don't prompt; fill new PRs' title/body from the commit messages (use for scripts/CI).
    #[arg(short, long)]
    pub fill: bool,
    /// Open newly-created PRs as drafts.
    #[arg(long)]
    pub draft: bool,
    /// Domain expansion: accept the proposed split without the interactive review gate.
    #[arg(long = "no-review", visible_alias = "yes", short = 'y')]
    pub no_review: bool,
}

#[derive(Args, Debug)]
pub struct BranchSplitArgs {
    /// Name for the new (lower) branch.
    pub name: String,
    /// The commit to split at (its commits and below go to the new branch); must be below the tip.
    pub at: String,
}

#[derive(Subcommand, Debug)]
pub enum PrCmd {
    /// Open the current branch's PR in the browser (`--print` to emit the URL instead).
    View(PrViewArgs),
}

#[derive(Args, Debug)]
pub struct PrViewArgs {
    /// Print the PR URL instead of opening a browser (for scripts / headless use).
    #[arg(short, long)]
    pub print: bool,
}

#[derive(Subcommand, Debug)]
pub enum UpstackCmd {
    /// Submit the current branch and everything above it.
    Submit(SubmitArgs),
}

#[derive(Subcommand, Debug)]
pub enum DownstackCmd {
    /// Submit the current branch and everything below it.
    Submit(SubmitArgs),
}

#[derive(Args, Debug)]
pub struct RenameArgs {
    /// Either `<new>` (rename the current branch) or `<old> <new>`.
    #[arg(num_args = 1..=2, required = true)]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct MessageArg {
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
}

#[derive(Args, Debug)]
pub struct NameArg {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct OptNameArg {
    pub name: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum WorktreeCmd {
    /// Add a workspace at PATH (optionally named, optionally starting on a branch).
    Add(WorktreeAddArgs),
    /// List workspaces and their current branch.
    List,
    /// Stop tracking a workspace by name (files left on disk).
    Remove(NameArg),
}

#[derive(Args, Debug)]
pub struct WorktreeAddArgs {
    /// Directory for the new workspace.
    pub path: String,
    /// Workspace name (default: directory basename).
    pub name: Option<String>,
    /// Start the workspace on this branch's tip (default: trunk).
    #[arg(long)]
    pub branch: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct ResolveArgs {
    /// Capture the current fix and advance to the next conflict — or, when the stack is clean,
    /// finish: return to the branch you started on and resume the sync.
    #[arg(long = "continue", conflicts_with_all = ["abort", "interactive"])]
    pub cont: bool,
    /// Give up: undo the sync and discard the resolution, restoring the pre-sync state.
    #[arg(long)]
    pub abort: bool,
    /// Resolve the current conflict with your configured merge tool (one file at a time), instead
    /// of hand-editing the markers.
    #[arg(short, long, conflicts_with = "abort")]
    pub interactive: bool,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Reconcile local state only (fetch, drop merged branches, rebase onto trunk); don't push
    /// branches or retarget PRs. Faster, and safe when you're not ready to update the remote.
    #[arg(short = 'n', long)]
    pub no_push: bool,
}

#[derive(Args, Debug)]
pub struct StashArgs {
    #[command(subcommand)]
    pub action: Option<StashAction>,
}

#[derive(Subcommand, Debug)]
pub enum StashAction {
    /// Restore the most recent stash into the working copy.
    Pop,
}
