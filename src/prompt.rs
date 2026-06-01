//! The `Prompter` **port**: gathering pull-request details when `submit` is about to **create** a
//! PR for a new branch. The engine depends only on this trait, so the library stays free of
//! stdin/editor concerns and tests drive `submit` non-interactively. The terminal implementation
//! lives in `main` (the only place allowed to touch the user's tty/editor).

use crate::error::Result;

/// Editable details for a pull request jjk is about to create.
#[derive(Clone, Debug)]
pub struct PrDraft {
    pub title: String,
    pub body: String,
    pub draft: bool,
}

/// Supplies PR details when `submit` creates a PR for a **new** branch. Existing PRs are updated
/// without prompting (their title/body/draft state are left to the author).
pub trait Prompter: Send + Sync {
    /// Confirm/edit details for a new PR on `branch` (based on `base`). `defaults` is pre-filled:
    /// title from the commit subject, body derived from the branch, draft from the `--draft` flag.
    /// Returns the final draft, or `None` to skip creating this branch's PR.
    fn new_pr(&self, branch: &str, base: &str, defaults: PrDraft) -> Result<Option<PrDraft>>;
}

/// Non-interactive default: accept the derived defaults unchanged. Used by tests, in non-TTY / CI
/// runs, and under `jjk submit --fill`.
pub struct AutoFill;

impl Prompter for AutoFill {
    fn new_pr(&self, _branch: &str, _base: &str, defaults: PrDraft) -> Result<Option<PrDraft>> {
        Ok(Some(defaults))
    }
}
