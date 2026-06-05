# jjk — jujutsu, kinda

**Stacked GitHub PRs with the git commands you already know, minus the restack pain.**

`jjk` is a CLI that gives you a familiar **git** command surface and a stacking workflow (inspired by
[git-spice](https://abhinav.github.io/git-spice/)) while using
**[Jujutsu (jj)](https://jj-vcs.github.io/jj/)** as the engine underneath. You keep the git mental
model. jj does the hard parts: automatic rebasing, first-class conflicts, stable change identity, and
instant undo.

![](https://i.imgflip.com/at3gj0.jpg)

📖 **[Documentation](https://ethan.website/jjk/)**

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
- **Domain Expansion** (experimental): work on a single branch and let an LLM split it into a
  reviewable stack of PRs — see [Two ways to stack](#two-ways-to-stack).

## Two ways to stack

jjk gives you two workflows for building a stack — they share all the same submit / review / sync
machinery, they just differ in who draws the PR boundaries:

- **Manual** — you create branches and commit to them yourself, one PR per branch (the usage below).
- **Automatic — Domain Expansion** (🧪 experimental) — you do all your work on a *single* branch and
  jjk uses an LLM (Claude) to decompose it into an ordered stack of small PRs. You review (and can
  edit) the proposed split, then address feedback by editing that one branch and re-syncing — no
  juggling branches.

  ```bash
  jjk branch create my-feature        # work on one branch
  # ...edit, jjk commit, edit, jjk commit...
  jjk domain expansion                # turn on auto-stacking (mode: change)
  jjk submit                          # split it → review → open a stacked PR per piece
  # ...address review on the same branch, jjk commit...
  jjk sync                            # re-split; existing PRs stay put
  ```

  Needs `ANTHROPIC_API_KEY`. Full details in the
  [Domain Expansion guide](https://ethan.website/jjk/domain-expansion).

## Installation

Requires [jj](https://jj-vcs.github.io/jj/), the [GitHub CLI](https://cli.github.com/) (`gh`,
authenticated, for PR commands), and a configured jj identity.

```bash
brew install jj
gh auth login
jj config set --user user.name "Your Name"
jj config set --user user.email "you@example.com"

cargo install --path .
```

See the [installation guide](https://ethan.website/jjk/installation) for details.

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

The [quickstart](https://ethan.website/jjk/quickstart) and
[workflow guide](https://ethan.website/jjk/workflow) walk through the full loop, and the
[command reference](https://ethan.website/jjk/commands) lists everything.

## Contributing

```bash
cargo test
cargo clippy --all-targets
```

The docs site (built with [Vocs](https://vocs.dev)) lives in [`docs/`](docs); design notes are in
[`docs/design/`](docs/design).

## License

MIT. See [LICENSE](LICENSE).
