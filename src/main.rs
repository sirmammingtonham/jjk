//! `jjk` entrypoint: parse the verb, dispatch to the engine, render output.
//! Data flows one way: cli → engine → {Vcs, Forge, state} → render.

use anyhow::Context;
use clap::{CommandFactory, Parser};
use jjk::cli::{
    BranchCmd, Cli, Command, DownstackCmd, PrCmd, RepoCmd, StashAction, SubmitArgs, UpstackCmd,
    WorktreeCmd,
};
use jjk::engine::{Engine, NavDir, SubmitOptions, SubmitScope};
use jjk::prompt::{PrDraft, Prompter};
use jjk::render;
use jjk::vcs::CommitScope;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // When the user is looking at top-level help, surface a jj-version compatibility note.
    if wants_top_level_help() {
        if let Some(w) = jjk::vcs::jj_cli::version_warning() {
            eprintln!("{w}\n");
        }
    }

    let cli = Cli::parse();
    let Some(command) = cli.command else {
        // Bare `jjk`: print help.
        let _ = Cli::command().print_help();
        println!();
        return ExitCode::SUCCESS;
    };

    match run(command).await {
        Ok(code) => code,
        Err(e) => {
            // Surface errors (including passed-through jj/gh errors) verbatim.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Whether a command changes the repo (so it should record an undo checkpoint). Pure reads and
/// `undo` itself don't; everything else does (including remote commands, which still create local
/// jj operations).
fn mutates(command: &Command) -> bool {
    !matches!(
        command,
        Command::Status
            | Command::Ls
            | Command::Ll
            | Command::Branch(BranchCmd::Diff)
            | Command::Worktree(WorktreeCmd::List)
            | Command::Pr(_)
            | Command::Undo
    )
}

/// Resolve a user-supplied path (relative to the current directory) to a repo-root-relative string,
/// so it can be anchored with jj's `root:` fileset regardless of where jjk was invoked. Falls back
/// to the path as typed if it can't be made relative to the root.
fn to_root_relative(cwd: &Path, root: &Path, p: &str) -> String {
    let pb = Path::new(p);
    let abs = if pb.is_absolute() { pb.to_path_buf() } else { cwd.join(pb) };
    let abs = abs.canonicalize().unwrap_or(abs);
    match abs.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => p.to_string(),
    }
}

/// True when the invocation is the top-level help (bare `jjk`, `jjk help`, `jjk --help`, `jjk -h`).
fn wants_top_level_help() -> bool {
    match std::env::args().nth(1) {
        None => true,
        Some(a) => matches!(a.as_str(), "help" | "--help" | "-h"),
    }
}

async fn run(command: Command) -> anyhow::Result<ExitCode> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;

    match command {
        Command::Repo(RepoCmd::Init(args)) => {
            let report = Engine::repo_init(&cwd, args.trunk, args.remote)?;
            render::print_report(&report);
            Ok(ExitCode::SUCCESS)
        }

        Command::Add => {
            println!("jjk has no staging area of its own, but it respects git's: `git add` some");
            println!("files and `jjk commit` will commit just those. With nothing staged it commits");
            println!("everything; `jjk commit <paths>` or `jjk commit -i` also scope a commit.");
            Ok(ExitCode::SUCCESS)
        }

        other => dispatch_in_repo(&cwd, other).await,
    }
}

async fn dispatch_in_repo(cwd: &std::path::Path, command: Command) -> anyhow::Result<ExitCode> {
    let mut engine = Engine::open(cwd)?;
    let mut conflicts = false;

    // Follow a plain `git checkout`: if git HEAD moved out from under jj, reconcile so position
    // tracking is correct (jjk's fast reads skip jj's HEAD import). Surface where we landed.
    if engine.reconcile_git_head()? {
        let here = engine.current_branch()?.unwrap_or_else(|| "trunk".to_string());
        eprintln!("note: followed git HEAD (now on {here})");
    }

    // Make a mutating jjk command a single undo unit: record a checkpoint so `jjk undo` can
    // `jj op restore` past *all* the jj operations the command performs (not just the last).
    // Best-effort — never block the real command if the checkpoint can't be written.
    if mutates(&command) {
        let _ = engine.checkpoint();
    }

    match command {
        Command::Commit(args) => {
            // `-i`/<paths> scope only the plain commit path; reject them on the restructuring verbs.
            if (args.interactive || !args.paths.is_empty())
                && (args.fixup.is_some() || args.amend || args.split || args.pick.is_some())
            {
                anyhow::bail!("`-i`/<paths> can't be combined with --amend/--fixup/--split/--pick");
            }

            let report = if let Some(target) = args.fixup.as_deref() {
                if !args.no_verify {
                    engine.run_pre_commit(&CommitScope::All)?;
                }
                engine.commit_fixup(target)?
            } else if args.split {
                // Restructuring (no new content) — no hook.
                engine.commit_split()?
            } else if let Some(rev) = args.pick.as_deref() {
                engine.commit_pick(rev)?
            } else if args.amend {
                if !args.no_verify {
                    engine.run_pre_commit(&CommitScope::All)?;
                }
                engine.commit_amend(args.message.as_deref())?
            } else {
                // Resolve what to commit: -i wins, then explicit paths, then git-staged files,
                // else the whole working copy.
                let scope = if args.interactive {
                    CommitScope::Interactive
                } else if !args.paths.is_empty() {
                    let root = engine.root().to_path_buf();
                    CommitScope::Paths(
                        args.paths.iter().map(|p| to_root_relative(cwd, &root, p)).collect(),
                    )
                } else {
                    let staged = engine.staged_paths()?;
                    if staged.is_empty() {
                        CommitScope::All
                    } else {
                        CommitScope::Paths(staged)
                    }
                };
                if !args.no_verify {
                    engine.run_pre_commit(&scope)?;
                }
                let msg = args
                    .message
                    .ok_or_else(|| anyhow::anyhow!("commit requires -m <message>"))?;
                if let CommitScope::Paths(p) = &scope {
                    if args.paths.is_empty() {
                        eprintln!(
                            "committing {} staged {}",
                            p.len(),
                            jjk::text::plural(p.len(), "file", "files")
                        );
                    }
                }
                engine.commit_scoped(&msg, &scope)?
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
        Command::Branch(BranchCmd::Onto(arg)) => {
            let report = engine.branch_onto(&arg.name)?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }
        Command::Branch(BranchCmd::Rename(args)) => {
            let report = match args.names.as_slice() {
                [new] => engine.branch_rename(None, new)?,
                [old, new] => engine.branch_rename(Some(old), new)?,
                _ => anyhow::bail!("rename takes <new> or <old> <new>"),
            };
            render::print_report(&report);
        }
        Command::Branch(BranchCmd::Diff) => {
            print!("{}", engine.branch_diff()?);
        }
        Command::Branch(BranchCmd::Squash(args)) => {
            let report = engine.branch_squash(args.message.as_deref())?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }
        Command::Branch(BranchCmd::Fold) => {
            render::print_report(&engine.branch_fold()?);
        }
        Command::Branch(BranchCmd::Split(args)) => {
            render::print_report(&engine.branch_split(&args.name, &args.at)?);
        }
        Command::Branch(BranchCmd::Submit(args)) => {
            let opts = prepare_submit(&mut engine, &args);
            render::print_report(&engine.submit_with(SubmitScope::Branch, opts).await?);
        }
        Command::Upstack(UpstackCmd::Submit(args)) => {
            let opts = prepare_submit(&mut engine, &args);
            render::print_report(&engine.submit_with(SubmitScope::Upstack, opts).await?);
        }
        Command::Downstack(DownstackCmd::Submit(args)) => {
            let opts = prepare_submit(&mut engine, &args);
            render::print_report(&engine.submit_with(SubmitScope::Downstack, opts).await?);
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
            // One snapshotting read of @ (detects uncommitted edits); the rest are non-snapshotting.
            let wc = engine.vcs().snapshot()?;
            let stack = engine.derive_stack()?;
            println!("{}", render::render_position(&stack));
            if wc.is_empty {
                println!("working copy is clean (empty @)");
            } else {
                println!("working copy has uncommitted changes");
            }
            print!("{}", render::render_ls(&stack));
            if stack.branches.iter().any(|b| b.has_conflict()) {
                conflicts = true;
                eprintln!("\nThis stack has conflicts; run `jjk resolve`.");
            }
        }
        Command::Ls => {
            let stack = engine.derive_stack()?;
            print!("{}", render::render_ls(&stack));
        }
        Command::Ll => {
            let stack = engine.derive_stack()?;
            print!("{}", render::render_ll(&stack));
        }

        Command::Up => render::print_report(&engine.navigate(NavDir::Up)?),
        Command::Down => render::print_report(&engine.navigate(NavDir::Down)?),
        Command::Top => render::print_report(&engine.navigate(NavDir::Top)?),
        Command::Bottom => render::print_report(&engine.navigate(NavDir::Bottom)?),
        Command::Trunk => render::print_report(&engine.trunk_checkout()?),

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
        Command::Pr(PrCmd::View(args)) => {
            // On --print, emit the URL; otherwise `gh` opened the browser and printed its own note.
            if let Some(url) = engine.pr_view(args.print).await? {
                println!("{url}");
            }
        }
        Command::Submit(args) => {
            let opts = prepare_submit(&mut engine, &args);
            let report = engine.submit_with(SubmitScope::Stack, opts).await?;
            render::print_report(&report);
        }
        Command::Sync(args) => {
            let report = engine.sync(/*push=*/ !args.no_push).await?;
            conflicts = !report.conflicts.is_empty();
            render::print_report(&report);
        }

        // Handled before reaching here.
        Command::Repo(_) | Command::Add => unreachable!(),
    }

    // Keep plain git in sync: re-attach git HEAD to the branch jjk is now on (jj detaches it when
    // it moves @). Best-effort — never fail a command over this.
    let _ = engine.sync_git_head_to_current();

    Ok(if conflicts {
        // Conflicts are reported, not fatal (ARCH D4); use a distinct nonzero code.
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

/// Decide how `submit` gathers details for new PRs and install a terminal prompter when fitting.
/// Interactive unless `--fill` was passed or stdio isn't a tty (scripts/CI fall back to the
/// engine's non-interactive default). Returns the engine-level options (draft).
fn prepare_submit(engine: &mut Engine, args: &SubmitArgs) -> SubmitOptions {
    let interactive =
        !args.fill && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if interactive {
        engine.set_prompter(Box::new(TerminalPrompter));
    }
    SubmitOptions { draft: args.draft }
}

/// Terminal implementation of the `Prompter` port: git-spice-style Title → Body → Draft prompts
/// when `submit` creates a new PR. Lives in `main` so the library never touches stdin/$EDITOR.
/// Prompts go to stderr so the rendered report on stdout stays pipe-clean.
struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    fn new_pr(
        &self,
        branch: &str,
        base: &str,
        defaults: PrDraft,
    ) -> jjk::error::Result<Option<PrDraft>> {
        eprintln!("\nCreating pull request: {branch} → {base}");
        let title = match prompt_line(&format!("Title [{}]: ", defaults.title))? {
            Some(s) if !s.is_empty() => s,
            _ => defaults.title,
        };
        let body = prompt_body(&defaults.body)?;
        let draft = prompt_yes_no("Draft?", defaults.draft)?;
        Ok(Some(PrDraft { title, body, draft }))
    }
}

/// Print a prompt to stderr and read one line from stdin. `None` on EOF (e.g. `^D`).
fn prompt_line(prompt: &str) -> jjk::error::Result<Option<String>> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
}

/// `[y/N]`-style confirm with a pre-selected default (empty/EOF answer takes the default).
fn prompt_yes_no(prompt: &str, default_yes: bool) -> jjk::error::Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    Ok(match prompt_line(&format!("{prompt} {hint}: "))? {
        Some(s) if !s.trim().is_empty() => matches!(s.trim().chars().next(), Some('y' | 'Y')),
        _ => default_yes,
    })
}

/// Body prompt: keep the derived default, or press `e` to edit it in `$EDITOR`.
fn prompt_body(default: &str) -> jjk::error::Result<String> {
    match prompt_line("Body: press [e] to edit in $EDITOR, [enter] to accept the default: ")? {
        Some(s) if s.trim().eq_ignore_ascii_case("e") => Ok(edit_in_editor(default)),
        _ => Ok(default.to_string()),
    }
}

/// Open `$VISUAL`/`$EDITOR` (fallback `vi`) on a temp file seeded with `initial`; return the edited
/// text. On any failure, keep `initial` — a missing editor shouldn't abort a submit.
fn edit_in_editor(initial: &str) -> String {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("jjk-pr-body-{}.md", std::process::id()));
    if std::fs::write(&path, initial).is_err() {
        return initial.to_string();
    }
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(prog).args(parts).arg(&path).status();
    let body = match status {
        Ok(st) if st.success() => {
            std::fs::read_to_string(&path).unwrap_or_else(|_| initial.to_string())
        }
        _ => {
            eprintln!("note: couldn't run $EDITOR ({editor}); keeping the default body");
            initial.to_string()
        }
    };
    let _ = std::fs::remove_file(&path);
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        initial.to_string()
    } else {
        format!("{trimmed}\n")
    }
}
