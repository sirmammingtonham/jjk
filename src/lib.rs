//! `jjk` — git/git-spice command semantics over Jujutsu (jj) for stacked GitHub PRs.
//!
//! Layering (one-way: `cli → engine → {Vcs, Forge, state} → render`):
//! - [`model`]   backend-neutral domain types crossing the trait boundary
//! - [`vcs`]     the `Vcs` port + the in-process `jj_lib` adapter (links the `jj-lib` crate)
//! - [`forge`]   the `Forge` port + the `octocrab` adapter (default) and `gh_cli` fallback
//! - [`engine`]  verb → ordered plan of VCS/state/forge ops; depends only on the traits
//! - [`prompt`]  the `Prompter` port for interactive PR details (terminal impl lives in `main`)
//! - [`state`]   `.jj/jjk/state.toml` (branch↔PR map + config)
//! - [`render`]  `ls`/`status` output
//! - [`cli`]     clap vocabulary

pub mod cli;
pub mod engine;
pub mod error;
pub mod forge;
pub mod model;
pub mod prompt;
pub mod render;
pub mod state;
pub mod text;
pub mod vcs;
