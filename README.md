# jjk — jujutsu, kinda

**Stacked GitHub PRs with the git commands you already know, minus the restack pain.**

![](https://i.imgflip.com/at3gj0.jpg)

`jjk` is a CLI that gives you a familiar **git** command surface and a stacking workflow (inspired by
[git-spice](https://abhinav.github.io/git-spice/)) while using
**[Jujutsu (jj)](https://jj-vcs.github.io/jj/)** as the engine underneath. You keep the git mental
model. jj does the hard parts: automatic rebasing, first-class conflicts, stable change identity, and
instant undo.

📖 **[Documentation](https://sirmammingtonham.github.io/jjk/)**

```
🥞 jjk ls

    ┏━■ feat-c ◀
  ┏━┻□ feat-b
┏━┻□ feat-a
main
```

## Features

- **Familiar commands.** `commit`, `branch create`, `checkout`, `submit`, `sync`. No new VCS to learn.
- **Stacked PRs on GitHub**, each correctly based on the one below it, with an auto-updated stack
  navigation comment.
- **Mid-stack edits auto-restack the upstack.** No manual restack step and no replaying conflicts in
  commits that won't ship.
- **Clean syncs.** After a PR merges (squash or merge-commit), `jjk sync` rebases the survivors onto
  trunk, retargets their bases, and pushes, with no duplicated or garbled diffs.
- **Conflicts never stop the world.** They live inside commits; you resolve once and the fix
  propagates upstack.
- **Parallel worktrees** for working several branches at once, and `jjk undo` for anything.

## Installation

Requires [jj](https://jj-vcs.github.io/jj/) 0.41.0, the [GitHub CLI](https://cli.github.com/) (`gh`,
authenticated, for PR commands), and a configured jj identity.

```bash
brew install jj
gh auth login
jj config set --user user.name "Your Name"
jj config set --user user.email "you@example.com"

cargo install --path .
```

See the [installation guide](https://sirmammingtonham.github.io/jjk/installation) for details.

## Usage

```bash
jjk repo init                       # init a colocated jj+git repo
jjk branch create feat-a            # start a branch (one PR)
echo hi > file.txt
jjk commit -m "first change"        # commit; multiple commits per branch is fine
jjk branch create feat-b            # stack another branch on top
jjk commit -m "feat-b work"
jjk ls                              # show the stack
jjk submit                          # open a stacked PR per branch
# ...merge the bottom PR on GitHub...
jjk sync                            # reconcile, rebase, retarget, push
```

The [quickstart](https://sirmammingtonham.github.io/jjk/quickstart) and
[workflow guide](https://sirmammingtonham.github.io/jjk/workflow) walk through the full loop, and the
[command reference](https://sirmammingtonham.github.io/jjk/commands) lists everything.

## Contributing

```bash
cargo test
cargo clippy --all-targets
```

The docs site (built with [Vocs](https://vocs.dev)) lives in [`docs/`](docs); design notes are in
[`docs/design/`](docs/design).

## License

MIT. See [LICENSE](LICENSE).
