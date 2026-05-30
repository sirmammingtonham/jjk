//! CLI vocabulary (clap derive). Pure parsing — no logic. git/git-spice verbs map to engine calls.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "jjk",
    version,
    about = "git/git-spice command semantics over Jujutsu (jj) for stacked GitHub PRs"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
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
    /// Stack diagram and current position.
    Ls,

    /// Move up one branch (away from trunk).
    Up,
    /// Move down one branch (toward trunk).
    Down,
    /// Jump to the top of the stack.
    Top,
    /// Jump to the bottom of the stack.
    Bottom,

    /// Undo the last operation (jj op-log).
    Undo,

    /// Manage parallel working trees (jj workspaces).
    #[command(subcommand)]
    Worktree(WorktreeCmd),

    /// Park/unpark working-copy changes (muscle memory; switching is safe in jj).
    Stash(StashArgs),

    /// Open the lowest conflicted change to resolve it.
    Resolve,

    /// Restack the upstack (usually a no-op with jj).
    Restack,

    /// Fetch from the remote.
    Fetch,
    /// Pull trunk and rebase the current stack onto it.
    Pull,
    /// Push the current branch.
    Push,
    /// Create/update PRs for the stack (bottom-up, correctly based).
    Submit,
    /// Pull trunk and reconcile merged branches.
    Sync,

    /// The friendly "no staging area needed" note (D1).
    Add,
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
