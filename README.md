# jjk — jujutsu, kinda

A CLI that gives you familiar **git / git-spice command semantics** while using
**[Jujutsu (jj)](https://jj-vcs.github.io/jj/)** as the engine underneath, to manage stacked
GitHub PRs. You keep the git mental model; jj does the hard work (automatic rebasing, first-class
conflicts, stable change identity, undo).

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the design, [`PROMPT.md`](PROMPT.md) for the build
plan, and [`JJ_NOTES.md`](JJ_NOTES.md) for the empirically-confirmed jj behaviors this is built on.

## Requirements

- **jj 0.41.0** (pinned — jj is pre-1.0 and its CLI churns; see `JJ_NOTES.md`).
- `gh` (for forge operations, Phase 3+).
- A jj user identity configured (`jj config set --user user.name/.email`), or `JJ_CONFIG` set.

```sh
brew install jj
cargo build --release
```

## Status

| Phase | Scope | State |
|---|---|---|
| 0 | jj behavior probe (`JJ_NOTES.md`) | ✅ done |
| 1 | Local feel: `repo init`, `commit`, `commit --amend`, `branch create`, `checkout [-b]`, `status`, `ls`, `up`/`down`/`top`/`bottom`, `undo` | ✅ done |
| 2 | Stack derivation: `restack`, `track`/`untrack`, `branch delete` (heal-the-gap) | ✅ done |
| 3 | Forge: `fetch`, `pull`, `push`, `submit` (idempotent, bottom-up bases) | ✅ done |
| 4 | `sync` (merged-branch reconciliation; squash + merge-commit) | ✅ done |
| 5 | Worktrees + polish | ⏳ next |
| 6 | Crate adapters (`jj-lib`/`octocrab`), optional | ⏳ |

## Quick start (Phase 1)

```sh
jjk repo init                 # init + colocate; detect/store trunk + remote
jjk branch create feat-a      # new stack-tracked branch
echo hi > file.txt
jjk commit -m "first change"  # commits ALL current changes (no staging area)
echo more >> file.txt
jjk commit -m "more"          # feat-a now has two commits
jjk branch create feat-b      # stack feat-b on top of feat-a
jjk commit -m "feat-b work"
jjk ls                        # show the stack
jjk down                      # move to feat-a; edits here auto-restack feat-b
jjk undo                      # reverse the last operation
```

`jjk ls` shows the stack top→bottom with the current branch marked:

```
◉ feat-b   (no PR)  1 commit   ← current
  feat-a   (no PR)  2 commits
  main (trunk)
```

## How it works (the short version)

- A **branch** = a jj **bookmark** at the tip of a contiguous range of jj commits (≈ one PR).
  Multiple commits per branch is natural.
- The working copy `@` is kept as an **empty child of the current branch tip**; your edits
  accumulate there and `jjk commit` finalizes them and advances the bookmark.
- The stack graph is **never persisted** — it's derived from jj at runtime via revsets. Only the
  branch→PR map and config live in `.jj/jjk/state.toml`.
- A **mid-stack commit** auto-restacks the upstack (jj rebases descendants; conflicts are stored
  in commits, never halting).

## Conflicts (resolve flow)

jj stores conflicts **inside commits** — operations complete rather than halting. When `jjk`
reports `CONFLICT: N change(s) need resolution`:

1. Switch to the conflicted branch and open its tip for editing (jj materializes conflict
   markers in the affected files).
2. Edit the files to resolve; the next `jjk` command re-snapshots the working copy and the
   resolution **propagates to descendants** automatically.
3. `jjk status` confirms the conflicts are cleared.

(A dedicated `jjk resolve` helper lands with later phases; today use `jj edit <tip>` then edit.)

## Development

```sh
cargo test            # integration tests run against real temporary jj repos (not mocks)
cargo clippy --all-targets
```

All `jj` subprocess calls are centralized in `src/vcs/jj_cli.rs` and parse **templated**
`--no-graph` output. The engine depends only on the `Vcs`/`Forge` traits — no backend type leaks
into it.
