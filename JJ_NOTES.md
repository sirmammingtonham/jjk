# JJ_NOTES.md — Phase 0 Behavior Probe

Empirically confirmed jj behaviors that the `jjk` binary adapter (`vcs::jj_cli`) is built
against. **Every later phase cites this file instead of guessing.** These confirmed behaviors are
also the specification for the §4.1 conformance suite that later guards backend parity.

## Pinned version

```
jj 0.41.0   (Homebrew bottle, arm64)
```

**Pin requirement:** `jjk` targets jj **0.41.0**. CI must install exactly this version and
`jjk repo init` (or a `jjk doctor`) should assert `jj --version` matches a known-good range. jj is
pre-1.0 and flags churn between releases — do not assume newer/older flags work.

Probe method: throwaway colocated repos under `/tmp`, identity supplied via a `JJ_CONFIG` file
(`[user] name/email`). All commands below were run and their real output recorded.

---

## 0. Global gotchas discovered

- **Colocation is now the DEFAULT** in 0.41.0 (`jj git init --colocate` works but `--colocate` is
  a no-op unless `git.colocate=false`). We still pass `--colocate` explicitly for clarity and
  forward/backward safety. There is a `--no-colocate` to opt out.
- **`jj git init` does not accept `-R`.** Use the positional destination:
  `jj git init --colocate <DIR>`. `-R/--repository` is only for operating on an *existing* repo.
- **A colocated repo has a `@git` pseudo-remote** mirroring the underlying git refs. Every local
  bookmark also appears as `<name>@git` in `remote_bookmarks`. **The engine must ignore the `git`
  remote** and only act on the configured forge remote (e.g. `origin`): filter
  `remote_bookmarks` by `b.remote() != "git"`.
- **Setting `user.name`/`user.email` via `jj config set --repo` AFTER the working-copy commit
  already exists does not retro-set the author** of existing commits (warns
  "author of the working copy will stay ' <>'"). `jjk repo init` should ensure identity is set
  (global jj config or via `--config`) *before* the first commit, or warn the user.
- **Pass messages as separate argv elements.** In the Rust adapter every flag/value is its own
  vector element, so empty messages (`-m ""`) are fine. (Shell `-m""` collapses to `-m` with no
  value and errors — irrelevant to the Rust code, noted to avoid confusion in scripts/tests.)

---

## 1. Templating (machine-readable read path)

Always read state via `jj log -T '<template>' --no-graph`. Confirmed working template fields:

```
jj log --no-graph -r '<revset>' -T '
  change_id.short(8) ++ "|" ++
  commit_id.short(8) ++ "|" ++
  description.first_line() ++ "|" ++
  empty ++ "|" ++
  conflict ++ "|" ++
  local_bookmarks.map(|b| b.name()).join(",") ++ "|" ++
  remote_bookmarks.map(|b| b.name() ++ "@" ++ b.remote()).join(",") ++ "|" ++
  parents.map(|p| p.change_id().short(8)).join(",") ++ "\n"'
```

| Field | Template expression | Notes |
|---|---|---|
| change id (stable) | `change_id` / `change_id.short(N)` | stable across rewrites — **key the state map on this** |
| commit id (git sha) | `commit_id` / `commit_id.short(N)` | changes on every rewrite |
| description | `description` / `description.first_line()` | |
| empty flag | `empty` | prints `true`/`false` |
| conflict flag | `conflict` | prints `true`/`false` |
| **local** bookmarks | `local_bookmarks.map(\|b\| b.name())` | **use this for stack derivation** |
| remote bookmarks | `remote_bookmarks.map(\|b\| b.name() ++ "@" ++ b.remote())` | includes `@git`; filter it out |
| parents | `parents.map(\|p\| p.change_id())` | |
| working copies here | `working_copies` | e.g. `default@`, `ws2@` |
| is current workspace @ | `current_working_copy` | `true` only in the active workspace |

- **`bookmarks` (the bare keyword) merges local + remote** and will show `@git` noise. Prefer
  `local_bookmarks` / `remote_bookmarks` explicitly.
- The `bookmarks()` **revset function** matches commits carrying a **local** bookmark (confirmed:
  returned only `feat-a/feat-b/main`, not `@git`).
- Recommend a non-`\n` field separator unlikely to occur in descriptions, or template only the
  fields needed per call and keep descriptions last / on their own call. Use `--no-graph` always.

Sample output (one line per commit):
```
lqkzrpxz|d309bddf||true|false||,|qkspqrzo
```

---

## 2. Bookmarks: create / set / move / delete / forget

- `jj bookmark create <NAME> -r <REV>` — create new (errors if exists). Default `-r @`.
- `jj bookmark set <NAME> -r <REV>` — **create OR move** by name. Needs `-B/--allow-backwards`
  for backwards/sideways moves; forward (fast-forward) moves don't. **`jjk commit` uses
  `bookmark set -r @-`** to advance the branch (add `-B` to be safe for restack/sideways).
- `jj bookmark move <NAMES|--from REVSET> -t <REV>` — move existing only (cannot create).
- `jj bookmark delete <NAME>` — deletes the local bookmark **and marks the deletion to be pushed
  to the remote on the next push**. Hint emitted: push with `jj git push --deleted`.
- `jj bookmark forget <NAME>` — drops the bookmark locally **without** scheduling a remote
  deletion (use when you don't want to delete the remote branch).
- **Abandoning a commit that a bookmark points at deletes that bookmark** (observed:
  `jj abandon feat-a` printed `Deleted bookmarks: feat-a`; a subsequent explicit
  `jj bookmark delete feat-a` reported "No matching bookmarks"). So in `sync`, abandoning a
  merged branch's tip range removes its local bookmark for free — but the **remote** branch still
  needs `jj git push --deleted` (or leave to GitHub auto-delete-on-merge).

---

## 3. `jj new` and `--insert-after`

- `jj new <REVSET...>` — new empty change with the given parents (default `@`); edits it (makes
  it the new `@`) unless `--no-edit`. Multiple revsets ⇒ merge commit.
- **Checkout = `jj new <branch-tip>`**: positions a fresh empty `@` as a child of the tip. Any
  existing upstack child stays on the real tip (sibling of the new empty `@`) — exactly the
  empty-`@` invariant (ARCH §3.2). This is why a mid-stack commit needs an explicit upstack
  rebase (§3.3 step 3) — inserting a sibling does NOT move the upstack.
- `-A/--insert-after <REV>` / `-B/--insert-before <REV>` exist and **rebase the target's existing
  children onto the new change**. We deliberately do NOT use `--insert-after` for checkout/commit,
  because it would place the empty `@` *between* the tip and its upstack child, putting the empty
  `@` inside the upstack's ancestry (violates the invariant). Sibling-insert + explicit rebase is
  the chosen approach.

---

## 4. Empty undescribed non-bookmarked `@` is auto-abandoned

Confirmed: an empty, no-description, no-bookmark `@` **is auto-abandoned** when you move away
(`jj new <elsewhere>`). A `@` that has content (a non-empty tree) is **kept** as an anonymous
commit. So the empty-`@` invariant self-maintains: repeated checkouts don't litter empty commits.
(ARCH §12 bullet 1 confirmed — no explicit cleanup needed on the pinned version.)

---

## 5. Commit algorithm (ARCH §3.3) — validated end to end

`jjk commit -m M`:
1. `jj commit -m M` — finalizes `@` into real commit `C`, opens a fresh empty `@` on top.
2. `jj bookmark set <current> -r @- -B` — advance the branch bookmark to `C`.
3. If an upstack child branch exists: `jj rebase -s <upstack-first-commit> -d C` (see §6 below).
4. Invariant re-established automatically (the fresh `@` from step 1 is the empty child of `C`).

Verified: two `commit`s in a row give a branch with **two commits**, bookmark riding the tip.
`commit --amend` = `jj squash --into @-` (+ `jj describe @- -m M` for a new message); this rewrites
the tip and **jj auto-rebases all descendants** ("Rebased N descendant commits"), keeping change
ids stable while commit ids change.

---

## 6. Rebase / mid-stack restack / auto-rebase

- **`jj rebase` destination flag is `-o/--onto`** in 0.41.0. **`-d` is still accepted as an
  alias** (`[aliases: -d]`), so `jj rebase -s X -d Y` works. Selection flags: `-s/--source`
  (rev + descendants), `-b/--branch`, `-r/--revisions` (rev only).
- **Mid-stack commit:** after `jj commit` inserts `C` as a *sibling* of the upstack child (both
  children of the old tip), jj does NOT move the upstack automatically (no ancestor was
  rewritten). The engine must:
  - `jj bookmark set <current> -r C -B`
  - `jj rebase -s <upstack-first-commit> -d C`  → upstack now rides `C`. Confirmed.
- **Amend auto-rebases descendants** for free (an ancestor was *rewritten*): no explicit rebase
  needed. Confirmed feat-b auto-followed an amended feat-a.
- **Revset to identify what to rebase / a branch's own commits:** `trunk()..<tip>` gives commits
  in the tip's ancestry not in trunk. `roots(trunk()..<tip>)` gives the lowest such commit(s) —
  the right thing to pass to `jj rebase -s` (see §9, sync).

---

## 7. Conflicts are objects, not stop-states (ARCH §9 D4)

- Causing a conflict (amend a lower commit so an upstack hunk no longer applies) **does not halt**:
  the command exits 0 and prints `New conflicts appeared in 1 commits:` plus the commit line. The
  conflicted commit gets `conflict=true` in templates.
- The materialized file contains jj conflict markers (`<<<<<<< conflict 1 of 1` … `%%%%%%%` diff
  block … `+++++++` side … `>>>>>>> conflict 1 of 1 ends`).
- **Resolution flow:** `jj edit <conflicted-commit>` makes it the working copy (markers on disk);
  edit the file to the resolved content; the **next jj command snapshots `@`** and `conflict`
  flips to `false`; descendants auto-rebase onto the resolved version. Then `jj new <branch>` to
  re-establish the empty-`@` invariant. Confirmed.
- `jjk` must (a) never use git's stop-the-world, (b) detect `conflict=true` commits in the stack
  after any op and print a git-flavored summary, (c) offer the `jj edit`→resolve→snapshot flow.

---

## 8. trunk() detection (non-`main` safe)

- **`trunk()` resolves to the tip of the remote's default branch** (`<default>@<remote>`). With
  **no remote configured it falls back to `root()`** (the zzzz… root commit), NOT to a local
  `main`. Confirmed: local-only repo → `trunk()` == root; after `jj git remote add origin` +
  pushing `main`, `trunk()` == the `main` commit.
- Implication: on a **local-only `repo init`** we cannot rely on `trunk()`. `jjk repo init` must
  detect/choose the trunk bookmark (prefer remote HEAD when cloning; otherwise default to the
  bookmark the user is on / `main`/`master` if present) and **store the trunk bookmark name in
  state**. Once a remote default exists, `trunk()` is authoritative and should be preferred.

---

## 9. Empty-change detection after landing (the heart of `sync`) — BOTH methods

Setup each time: `main` (trunk) ← `feat-a` ← `feat-b`, pushed to a bare git `origin`. On fetch,
a tracked local bookmark with no local-only commits (e.g. `main`) **fast-forwards automatically**
to match `main@origin`.

### 9a. Squash-merge landing
Remote `main` gets a NEW commit (different sha) containing feat-a's diff.
- `jj git fetch` advances `main` (and `main@origin`).
- `jj rebase -s feat-a -d main` → **feat-a's change becomes `empty=true`** (its content already
  in trunk); feat-b stays non-empty. Confirmed.
- `jj abandon <feat-a range>` removes the empty change **and auto-rebases descendants onto the
  abandoned commit's parent** ("Rebased N descendant commits onto parents of abandoned commits"),
  so feat-b reconnects to trunk. The feat-a bookmark is deleted by the abandon.

### 9b. Merge-commit landing  ⚠️ DIFFERENT
Remote `main` gets a real merge commit whose **second parent is feat-a's exact commit**.
- After fetch, feat-a's local commit is now an **ancestor of `trunk()`**, so jj marks it
  **immutable**. `jj rebase -s feat-a -d main` **fails**: `Error: Commit <sha> is immutable`.
  (Do NOT use `--ignore-immutable` to force it.)
- We don't need to: feat-a is literally already in trunk. We only need to move the **non-merged**
  remainder.

### 9c. UNIFORM handling for both (use this in sync)
Rebase only the commits not yet in trunk:
```
jj rebase -s 'roots(trunk()..<stack-tip>)' -d 'trunk()'
```
- Squash case: `trunk()..tip` still contains feat-a (new trunk commit ≠ feat-a), so feat-a moves
  and becomes empty → abandon it afterward.
- Merge-commit case: `trunk()..tip` **excludes** feat-a (it's in trunk's ancestry), so only
  feat-b is rebased onto the merge commit; feat-a's immutable commit is untouched. Confirmed.
- For each **merged** branch: `jj bookmark delete <name>` (and `jj git push --deleted`, or rely
  on GitHub delete-on-merge). Abandon its now-empty mutable commit(s) only in the squash case;
  in the merge-commit case there is nothing mutable to abandon.

This `roots(trunk()..tip)` rebase is the single primitive that makes `sync` landing-method-
agnostic, satisfying ARCH §6 / §12 "do not special-case squash vs merge-commit".

---

## 10. git remote: fetch / push / force-with-lease / first push

- `jj git fetch` (remote from `git.fetch` or single/`origin`). Fast-forwards tracked bookmarks.
- `jj git push -b <NAME>` pushes a bookmark. **Force-with-lease is the default** (remote updated
  only if it matches what jj last fetched) — satisfies D5; no extra flag needed.
- **First push of a brand-new bookmark: just `jj git push -b <NAME>`** — it auto-tracks the new
  remote bookmark. **`--allow-new` is DEPRECATED** in 0.41.0 (warns); do not use it. (Alternative:
  configure `remotes.<name>.auto-track-bookmarks`, or push `--named <NAME>=<REV>`.)
- `jj git push --deleted` pushes pending bookmark deletions. `--all`, `--tracked`, `-r/--revision`,
  `-c/--change`, `--dry-run` also available.
- `jj git remote add <name> <url>` to register a remote in a local init.

---

## 11. Workspaces (jj workspace ≈ git worktree) + staleness

- `jj workspace add <PATH> --name <NAME>` creates a workspace; `@` there starts as an empty child
  of the source `@`'s parent. `jj workspace list` shows `default` + each named workspace and its
  `@`. `jj workspace forget <NAME>` removes (≈ `worktree remove`).
- **Per-workspace current `@`:** template `working_copies` lists which workspaces sit on a commit
  (`default@`, `ws2@`); `current_working_copy` is true only for the active workspace. Derive the
  "current branch" per workspace from its own `@` via `heads(::@- & bookmarks())` (see §12).
- **Staleness:**
  - Rewriting a *metadata-only* aspect (e.g. `jj describe`) of another workspace's `@` ancestry
    does **not** make it stale if the tree is unchanged — jj transparently rebases the empty `@`.
  - Rewriting **content** in another workspace's `@` ancestry (e.g. `jj squash` real changes into
    an ancestor) **does** make it stale. The stale workspace then **errors on `jj status`**
    (non-zero exit): `Error: The working copy is stale (not updated since operation …)` with hint
    to run `jj workspace update-stale`.
  - `jj workspace update-stale` recovers it ("Updated working copy to fresh commit …"). On a
    non-stale workspace it's a harmless no-op ("the working copy is not stale").
- **jjk policy:** at the start of any command that reads `@` in a multi-workspace repo, either
  proactively run `update-stale` or catch the stale error and auto-recover, per ARCH §8/§12.

---

## 12. Current-branch derivation revset

The current branch = nearest **local** bookmark at-or-below `@`:
```
heads(::@- & bookmarks())        # returns the commit; read its local_bookmarks
```
- `bookmarks()` in revset = local bookmarks only (confirmed). Using `@-` because the empty `@`
  itself is unbookmarked; `::@ & bookmarks()` is equivalent given the invariant.
- Full stack for rendering / derivation: `trunk()..<tip>` then read `local_bookmarks` per commit;
  branch ranges are the spans between consecutive bookmarked commits (Phase 2).

---

## 13. undo

- `jj undo` reverses the last operation and prints `Undid operation: <id> (…)` and
  `Restored to operation: <id> (…)`. Confirmed (created then undid a bookmark). Cheap safety net;
  exposed directly as `jjk undo`. (`jj op log` / `jj op restore <id>` available for deeper undo.)

---

## 14. Checkout switching never loses work (Phase 1 gate §5)

Switching via `jj new <branch-tip>` while `@` has **uncommitted** changes: the work is **preserved**
as an anonymous non-empty commit (child of the branch you left); it does **not** error and is not
lost. It does **not** follow you to the new branch (jj's model: changes belong to a commit, not a
floating index). Re-checking-out the original branch yields a fresh empty `@`; the WIP commit
remains as a sibling above that branch's tip (outside the branch's PR range `parent..bookmark`).

> **Design note for the engine (within spec §3.2):** this satisfies "never loses work". If a more
> git-like "changes follow me" feel is later desired, `jjk checkout` could detect a non-empty `@`
> and carry it, but ARCH §3.2 specifies plain `jj new <tip>`. Implement per spec; surface the WIP
> commit in `status`/`ls` so it's never silently orphaned.

---

## 15. Quick command crib (pinned to 0.41.0, verified)

| jjk need | jj invocation |
|---|---|
| init colocated | `jj git init --colocate <DIR>` |
| add remote (local init) | `jj git remote add <name> <url>` |
| finalize @ → commit | `jj commit -m <MSG>` |
| advance branch | `jj bookmark set <NAME> -r @- -B` |
| create branch bookmark | `jj bookmark create <NAME> -r <REV>` |
| amend tip | `jj squash --into @-` (+ `jj describe @- -m <MSG>`) |
| checkout / navigate | `jj new <branch-tip>` |
| rebase subtree | `jj rebase -s <REV> -d <DEST>` (`-d` alias of `-o`) |
| abandon change | `jj abandon <REV>` (auto-rebases descendants; deletes bookmark on it) |
| delete branch (push deletion) | `jj bookmark delete <NAME>` then `jj git push --deleted` |
| forget branch (keep remote) | `jj bookmark forget <NAME>` |
| fetch | `jj git fetch` |
| push (force-with-lease default) | `jj git push -b <NAME>` (also first push; no `--allow-new`) |
| sync rebase (landing-agnostic) | `jj rebase -s 'roots(trunk()..<tip>)' -d 'trunk()'` |
| trunk | revset `trunk()` (needs remote default; else store name) |
| current branch | revset `heads(::@- & bookmarks())` → `local_bookmarks` |
| workspace add | `jj workspace add <PATH> --name <NAME>` |
| recover stale ws | `jj workspace update-stale` |
| undo | `jj undo` |
| read state | `jj log --no-graph -r <revset> -T '<template>'` |

All read paths use templated `--no-graph` output. All subprocess calls live in `vcs::jj_cli`.
