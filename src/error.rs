//! Error types. `jjk` is a translator: when jj/gh fail, we surface their real error
//! (ARCH brief principle 7) rather than reword it.

use thiserror::Error;

pub type Result<T> = anyhow::Result<T>;

/// Domain errors the engine raises that are not just a passed-through backend error.
#[derive(Debug, Error)]
pub enum JjkError {
    #[error("not a jjk repo (no .jj/jjk/state.toml); run `jjk repo init` first")]
    NotInitialized,

    #[error("a jjk repo already exists here")]
    AlreadyInitialized,

    #[error("no branch named '{0}' in this stack")]
    UnknownBranch(String),

    #[error("'{0}' is the trunk; cannot operate on it as a branch")]
    IsTrunk(String),

    #[error("not currently on a tracked branch (working copy sits on trunk)")]
    NotOnBranch,

    #[error("already at the {0} of the stack")]
    StackEnd(&'static str),

    #[error("{0}")]
    Msg(String),
}
