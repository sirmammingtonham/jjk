//! `jjk` entrypoint: parse the verb, dispatch to the engine, render output.
//! Data flows one way: cli → engine → {Vcs, Forge, state} → render.

use anyhow::Context;
use clap::Parser;
use jjk::cli::{BranchCmd, Cli, Command, RepoCmd, StashAction, WorktreeCmd};
use jjk::engine::{Engine, NavDir};
use jjk::render;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            // Surface errors (including passed-through jj/gh errors) verbatim.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;

    match cli.command {
        Command::Repo(RepoCmd::Init(args)) => {
            let report = Engine::repo_init(&cwd, args.trunk, args.remote)?;
            render::print_report(&report);
            Ok(ExitCode::SUCCESS)
        }

        Command::Add => {
            println!("jjk has no staging area — `jjk commit -m` commits all current changes.");
            println!("(Save incremental progress with multiple commits; see `jjk commit --help`.)");
            Ok(ExitCode::SUCCESS)
        }

        other => dispatch_in_repo(&cwd, other).await,
    }
}

async fn dispatch_in_repo(cwd: &std::path::Path, command: Command) -> anyhow::Result<ExitCode> {
    let mut engine = Engine::open(cwd)?;
    let mut conflicts = false;

    match command {
        Command::Commit(args) => {
            let report = if args.amend {
                engine.commit_amend(args.message.as_deref())?
            } else {
                let msg = args
                    .message
                    .ok_or_else(|| anyhow::anyhow!("commit requires -m <message>"))?;
                engine.commit(&msg)?
            };
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }

        Command::Checkout(args) => {
            let report = if args.create {
                engine.branch_create(&args.name, /*tracked=*/ false)?
            } else {
                engine.checkout(&args.name)?
            };
            render::print_report(&report);
        }

        Command::Branch(BranchCmd::Create(arg)) => {
            let report = engine.branch_create(&arg.name, /*tracked=*/ true)?;
            render::print_report(&report);
        }
        Command::Branch(BranchCmd::Delete(arg)) => {
            let report = engine.branch_delete(&arg.name)?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }
        Command::Track(arg) => {
            render::print_report(&engine.set_tracked(arg.name.as_deref(), true)?);
        }
        Command::Untrack(arg) => {
            render::print_report(&engine.set_tracked(arg.name.as_deref(), false)?);
        }
        Command::Restack => {
            let report = engine.restack()?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }

        Command::Status => {
            let stack = engine.derive_stack()?;
            let wc = engine.vcs().working_copy()?;
            println!("{}", render::render_position(&stack));
            if wc.is_empty {
                println!("working copy is clean (empty @)");
            } else {
                println!("working copy has uncommitted changes");
            }
            print!("{}", render::render_ls(&stack));
            if engine.has_conflicts()? {
                conflicts = true;
                eprintln!("\nThis stack has conflicts; run the resolve flow (see README).");
            }
        }
        Command::Ls => {
            let stack = engine.derive_stack()?;
            print!("{}", render::render_ls(&stack));
        }

        Command::Up => render::print_report(&engine.navigate(NavDir::Up)?),
        Command::Down => render::print_report(&engine.navigate(NavDir::Down)?),
        Command::Top => render::print_report(&engine.navigate(NavDir::Top)?),
        Command::Bottom => render::print_report(&engine.navigate(NavDir::Bottom)?),

        Command::Undo => render::print_report(&engine.undo()?),

        Command::Worktree(WorktreeCmd::Add(args)) => {
            let report = engine.worktree_add(
                std::path::Path::new(&args.path),
                args.name.as_deref(),
                args.branch.as_deref(),
            )?;
            render::print_report(&report);
        }
        Command::Worktree(WorktreeCmd::List) => {
            let rows = engine.worktree_list()?;
            print!("{}", render::render_worktrees(&rows));
        }
        Command::Worktree(WorktreeCmd::Remove(arg)) => {
            render::print_report(&engine.worktree_remove(&arg.name)?);
        }

        Command::Stash(args) => {
            let report = match args.action {
                Some(StashAction::Pop) => engine.stash_pop()?,
                None => engine.stash()?,
            };
            render::print_report(&report);
        }

        Command::Resolve => {
            let report = engine.resolve()?;
            render::print_report(&report);
        }

        Command::Fetch => render::print_report(&engine.fetch()?),
        Command::Push => render::print_report(&engine.push_current()?),

        Command::Pull => {
            let report = engine.pull()?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }
        Command::Submit => {
            let report = engine.submit().await?;
            render::print_report(&report);
        }
        Command::Sync => {
            let report = engine.sync().await?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }

        // Handled before reaching here.
        Command::Repo(_) | Command::Add => unreachable!(),
    }

    Ok(if conflicts {
        // Conflicts are reported, not fatal (ARCH D4); use a distinct nonzero code.
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}
