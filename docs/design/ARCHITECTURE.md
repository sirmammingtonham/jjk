# jjk — Architecture Specification

A CLI that gives developers familiar **git / git-spice command semantics** while using
**Jujutsu (jj)** as the engine underneath. The goal: keep the git mental model, get jj's
benefits (automatic rebasing, first-class conflicts, stable change identity, undo) for free,
and manage stacked GitHub PRs the way git-spice does — but without git-spice's painful
restack conflicts and post-merge diff garbling.

This document is written to be handed to a coding agent. Implementation language: **Rust**.

---

## 1. Philosophy

`jjk` is **not a new VCS and not an abstraction that hides jj's power**. It is a thin
**semantic translation layer**:

```
user types git/gs verb  ->  jjk maps it to (jj ops + state update + forge calls)
```

The user never has to learn jj's mental model. jj does the hard work (rebasing, conflict
storage, identity tracking); `jjk` supplies a vocabulary the user's fingers already know and
adds the stacked-PR orchestration on top.

Most of the actual code is **GitHub orchestration**, not VCS logic — jj's primitives make the
VCS side small.

---

## 2. Why jj is the right engine (the two problems being solved)

1. **Restack conflicts on superseded commits.** In git, `rebase` cherry-picks commit by commit
   and *halts* on every conflict, including in commits whose changes don't survive to the tip.
   jj auto-rebases all descendants when an ancestor changes and **stores conflicts inside
   commits** instead of halting. You resolve a conflict once, where it actually matters, and the
   resolution propagates. `jjk` barely participates: jj already moved everything; `jjk` just
   pushes the bookmarks that moved.

2. **Post-merge diff garbling.** When the bottom PR squash-merges, jj's empty-change detection
   makes the corresponding local change **empty** after rebasing onto the new trunk (regardless
   of squash vs merge-commit). `jjk sync` abandons it, lets jj auto-rebase the rest, force-pushes,
   and retargets PR bases — automating the manual "merge master + restack" dance.

---

## 3. Core model

### 3.1 Branch = bookmark + commit range
- A **branch** (≈ a git-spice branch ≈ one PR) is a **jj bookmark** sitting at the tip of a
  contiguous range of jj commits.
- The branch's commits = revset `<parent-bookmark>..<branch-bookmark>`. For the bottom branch,
  `<parent-bookmark>` is `trunk()`.
- **Multiple commits per branch is natural**: just multiple jj commits in that range. The
  bookmark rides the tip.

### 3.2 Working-copy positioning (the empty-`@` invariant)
jj's working copy is itself a commit (`@`). `jjk` maintains this invariant in each workspace:

> When a branch is checked out, `@` is an (initially empty) working-copy commit that is a
> **child of that branch's tip**. The user's edits accumulate in `@`. Upstack branches stay on
> the real branch tip (NOT on the empty `@`), so empty working commits never enter a PR range.

This is what makes `git commit` feel like git: edit, commit, the branch grows, repeat.

### 3.3 Commit algorithm (`jjk commit -m M`)
1. Finalize the current `@` (with its changes) into a real commit `C` and open a fresh empty
   `@` on top — this is `jj commit -m M`.
2. Move the current branch's bookmark to `C` (`jj bookmark set <current> -r @-`).
3. If the branch had an upstack child branch, rebase it onto `C`
   (`jj rebase -s <upstack-first> -d C`). jj stores any conflicts; it does not halt.
4. Re-establish the invariant: ensure `@` is an empty child of `C`.

For the common case (current branch is the top of the stack) steps 3 is a no-op.

### 3.4 Derive, don't store
**Do not persist the stack graph.** Reconstruct it from jj at runtime via revsets (nearest
bookmark ancestors up to `trunk()`). Persisted graphs drift; derived graphs can't.

Persist only what jj genuinely cannot know:
- `branch name -> PR number` mapping
- config: trunk bookmark name, remote name, forge type

Stored in `.jj/jjk/state.toml`. Mapping is keyed/validated against jj **change IDs** (stable
across rewrites) so it never breaks when commits are amended/rebased.

### 3.5 Tracked vs untracked branches
- `checkout -b` creates an **untracked** branch (a plain bookmark, not part of any stack /
  no PR intent).
- `branch create` creates a **tracked** branch (recorded as stacked on the current branch).
- `track` / `untrack` convert between the two.
Tracking is recorded implicitly: a branch is "tracked" if it appears in the derived stack and
the user has opted it in (store a small set of tracked bookmark names in state, or infer from
ancestry + an explicit opt-in flag). Keep this minimal.

---

## 4. Architecture (layers / modules)

| Module | Responsibility |
|---|---|
| `cli` | Parse git/gs verbs with `clap`. Pure vocabulary, no logic. |
| `engine` | The heart. Each verb → ordered plan of (VCS ops, state updates, forge ops). Resolves all git↔jj impedance mismatches by policy. Depends **only** on the `Vcs`/`Forge` traits and `model` types — never on a concrete backend. |
| `model` | Backend-neutral domain types that cross the trait boundary (`ChangeId`, `CommitId`, `CommitInfo`, `Bookmark`, `WorkspaceInfo`, `PrRef`, …). |
| `vcs` | The `Vcs` **port** (trait) + adapters: `vcs::jj_cli` (binary) and `vcs::jj_lib` (crate). |
| `forge` | The `Forge` **port** (trait) + adapters: `forge::gh_cli` (binary) and `forge::octocrab` (crate). |
| `backend` | Reads config, constructs the selected `Vcs`/`Forge` adapters (factory), exposes their `Capabilities`. |
| `state` | Load/save `.jj/jjk/state.toml`; branch↔PR map + config (incl. backend selection). |
| `render` | `jjk ls` stack diagram and status output. |

Data flows one way: `cli -> engine -> {Vcs, Forge, state} -> render`, where `Vcs`/`Forge` are
traits resolved to a concrete adapter at startup by `backend`.

### 4.1 Backend abstraction (pluggable crate vs binary)

Each external dependency is a **port** (trait) owned by the engine's needs, with interchangeable
**adapters** (hexagonal / ports-and-adapters). Two ports:

- **`Vcs`** — jj operations. Adapters: `jj_cli` (shell out to the `jj` binary) and `jj_lib`
  (link the `jj-lib` crate, in-process).
- **`Forge`** — GitHub PR operations. Adapters: `gh_cli` (shell out to `gh`) and `octocrab`
  (link the `octocrab` crate).

Config (`.jj/jjk/state.toml`) selects an adapter per port, e.g. `vcs.backend = "jj_lib"`,
`forge.backend = "gh_cli"`. The factory in `backend` wires the chosen adapters; everything above
sees only the traits.

**The rule that makes adapters swappable:** the traits are defined in terms of the *domain
operations the engine needs*, not in terms of either implementation. **No `jj-lib` type and no
CLI/JSON shape may appear in a trait signature or anywhere in `engine`.** Adapters translate
*into* `model` types: `jj_cli` parses templated output into them; `jj_lib` maps jj-lib structs
into them. If a backend detail leaks across the boundary, the abstraction is broken.

Illustrative shape (signatures, not final):

```rust
// model — backend-neutral
pub struct ChangeId(pub String);   // stable across rewrites
pub struct CommitId(pub String);   // git sha; changes on rewrite
pub struct CommitInfo {
    pub change_id: ChangeId, pub commit_id: CommitId,
    pub parents: Vec<ChangeId>, pub bookmarks: Vec<String>,
    pub description: String, pub is_empty: bool, pub has_conflict: bool,
}
pub struct Bookmark { pub name: String, pub target: ChangeId }
pub struct WorkspaceInfo { pub name: String, pub working_copy: ChangeId, pub is_stale: bool }
pub struct Capabilities { pub atomic_transactions: bool, pub in_process: bool }

pub trait Vcs {
    fn capabilities(&self) -> Capabilities;
    // query
    fn trunk(&self) -> Result<ChangeId>;
    fn resolve(&self, revset: &str) -> Result<Vec<CommitInfo>>; // neutral revset subset
    fn working_copy(&self) -> Result<CommitInfo>;
    fn bookmarks(&self) -> Result<Vec<Bookmark>>;
    fn stack(&self, from: &ChangeId) -> Result<Vec<CommitInfo>>;
    // mutate — grouped so a backend can make it atomic
    fn transaction<R>(&self, f: &mut dyn FnMut(&mut dyn VcsTx) -> Result<R>) -> Result<R>;
    fn undo(&self) -> Result<()>;
    // workspaces
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>>;
    fn add_workspace(&self, path: &Path, name: &str) -> Result<()>;
    fn update_stale(&self, name: &str) -> Result<()>;
    // remote (git interop lives inside the VCS backend)
    fn fetch(&self, remote: &str) -> Result<()>;
    fn push(&self, bookmark: &str, opts: PushOpts) -> Result<()>;
}

pub trait VcsTx {
    fn new_commit(&mut self, parents: &[ChangeId], insert_after: Option<&ChangeId>) -> Result<ChangeId>;
    fn finalize_working_copy(&mut self, message: &str) -> Result<ChangeId>; // ≈ jj commit
    fn describe(&mut self, rev: &ChangeId, message: &str) -> Result<()>;
    fn squash(&mut self, from: &ChangeId, into: &ChangeId) -> Result<()>;
    fn set_bookmark(&mut self, name: &str, target: &ChangeId) -> Result<()>;
    fn delete_bookmark(&mut self, name: &str) -> Result<()>;
    fn rebase(&mut self, source: &ChangeId, dest: &ChangeId) -> Result<()>;
    fn abandon(&mut self, rev: &ChangeId) -> Result<()>;
}

#[async_trait]
pub trait Forge {
    async fn get_pr(&self, branch: &str) -> Result<Option<PrRef>>;
    async fn create_pr(&self, head: &str, base: &str, title: &str, body: &str) -> Result<PrRef>;
    async fn update_pr(&self, pr: u64, base: Option<&str>, body: Option<&str>) -> Result<()>;
    async fn is_merged(&self, pr: u64) -> Result<bool>;
}
```

**Transaction scope.** The engine expresses each command's mutations inside `Vcs::transaction`.
The `jj_lib` adapter maps this to a real jj-lib `Transaction` (atomic, a single op-log entry,
no repeated process spawn) — this batching is the *main* practical win, more than per-call
latency. The `jj_cli` adapter runs each `VcsTx` call as one `jj` invocation sequentially and
reports `atomic_transactions: false`; the engine may warn (or accept best-effort sequencing) for
multi-step mutations based on `Capabilities`.

**Conformance suite (mandatory).** A single trait-level test harness runs against **both**
adapters over real temporary repos, asserting identical observable behavior. Without it the two
implementations *will* drift and the config switch stops being safe. The Phase 0 behavior probe
(§11) defines the cases this suite must cover. The same pattern applies to `Forge` (fake adapter
for unit tests + a live smoke test).

**Honest guidance.** Ship on the **binary** adapters first — they are the stable, always-working
default and fallback (the CLI contract is far steadier than `jj-lib`, which has *no API stability
guarantees* and changes across jj releases). Add the **crate** adapters only after the binary
path works and the conformance suite is green, and only where profiling justifies it. Pin
`jj-lib` to an exact version and expect upgrade maintenance. The abstraction is precisely what
lets you defer, A/B, or reverse this decision freely.

**Async boundary.** Both ports are now **async**. `Forge` is async (octocrab is async). `Vcs` reads
are async, and a command's mutations run inside an async [`VcsTx`] obtained from
`begin_transaction()` and finalized with `commit()` — the `jj_lib` adapter maps that to one atomic
jj-lib transaction. The trait futures are `?Send` (jj-lib's in-memory repo/transaction handles are
not `Sync`); jjk drives one command to completion on the current runtime and never spawns a
command's work onto another thread.

> **Status (implemented).** The crate adapters described below as "Phase 6" are now the **shipped
> default**: `vcs::jj_lib` (the sole VCS adapter, in-process) and `forge::octocrab` (default forge,
> with `gh_cli` retained as a config-selectable fallback). There is **no `jj` binary dependency** —
> interactive diff/merge editing reuses the `jj-cli` crate's `merge_tools` library, and git
> fetch/push go through jj-lib's git layer (which shells out to `git`, as does jj itself).

---

## 5. Command reference

Notation: **jj** column lists conceptual jj operations; verify exact flags against the pinned
jj version (jj is pre-1.0 and flag names have churned — e.g. `branch` was renamed `bookmark`).

### Repo setup
| Command | Meaning | jj translation | Notes |
|---|---|---|---|
| `jjk repo init` | init + colocate | `jj git init --colocate` (or `jj git clone --colocate`) | Colocated so plain `git` still works as an escape hatch. Write state, detect/store trunk + remote. |

### Local work
| Command | Meaning | jj translation | Notes |
|---|---|---|---|
| `jjk commit -m M` | commit current changes | See §3.3 commit algorithm | Multiple commits per branch supported; mid-stack commit auto-restacks upstack. |
| `jjk commit --amend [-m M]` | amend tip | `jj squash` working changes into branch tip (`@-`) + `jj describe` | Descendants auto-rebase. Can target lower commits too (future flag). |
| `jjk checkout -b NAME` | new untracked branch | `jj new <current tip>` + `jj bookmark create NAME -r @-` style, **untracked** | Does not enter the stack. |
| `jjk checkout NAME` | switch to existing branch | `jj new <NAME tip>` (positions empty `@` per §3.2); set current=NAME | Switching is always safe in jj (no dirty-tree errors). |
| `jjk status` | working-copy status | `jj status` (+ short stack-position hint) | |
| `jjk stash` / `jjk stash pop` | park/unpark changes | park: bookmark current `@` aside (e.g. `jjk/stash/<n>`) + `jj new @-` for clean `@`; pop: `jj squash --from <stash> --into @` then abandon stash | Mostly unnecessary in jj (switching is safe), provided for muscle memory. |

### Stack management
| Command | Meaning | jj translation | Notes |
|---|---|---|---|
| `jjk branch create [NAME]` | `checkout -b`, **stack-tracked** | `jj new <current tip>` + create bookmark + record as stacked on current | Same as `checkout -b` but tracked. |
| `jjk branch delete NAME` | drop branch from stack | `jj abandon`/rebase to heal gap + `jj bookmark delete` + close PR | Heals the stack so upstack reconnects to NAME's parent. |
| `jjk track [NAME]` / `jjk untrack [NAME]` | convert tracked⇄untracked | toggle membership in stack state | |
| `jjk ls` | stack diagram + current position | `jj log` over the stack revset; annotate bookmarks, PR numbers, `@` marker | See §7 rendering. |
| `jjk restack` | restack the upstack | usually a **no-op** (jj already auto-rebased); recompute + push moved bookmarks | This is "free" with jj. |
| `jjk up` / `jjk down` / `jjk top` / `jjk bottom` | navigate the stack | move "current" along bookmark ancestry; reposition `@` via `jj new <target tip>` | |
| `jjk undo` | undo last operation | `jj undo` (or `jj op restore`) | Expose jj's op-log; cheap and a major safety net. |

### Remote / forge
| Command | Meaning | jj translation + forge | Notes |
|---|---|---|---|
| `jjk fetch` | fetch | `jj git fetch` | |
| `jjk pull` | pull trunk | `jj git fetch` + advance local trunk + rebase current stack onto trunk | **No** merged-PR detection (that's `sync`). |
| `jjk push` | push current branch | `jj git push -b <current>` (force-with-lease semantics; `--allow-new` for first push) | |
| `jjk submit` | create/update PRs | push stack bottom-up; for each branch create PR if none, else update; set each PR's base to its downstack branch (or trunk) | Idempotent: safe to re-run. |
| `jjk sync` | pull trunk + reconcile merged branches | See §6 | The hardest command; build last. |

---

## 6. `jjk sync` algorithm (the critical path)

```
1. jj git fetch
2. Query forge: for each branch in the stack with a PR, is the PR merged?
3. For each merged branch (bottom-up):
     a. abandon its now-empty change(s):  jj abandon <range>
        (after rebasing onto new trunk these become empty; jj detects emptiness for
         squash- and merge-landed PRs alike)
     b. jj bookmark delete <branch>
     c. record PR as merged in state
4. Rebase the remaining stack onto the updated trunk (jj auto-rebases descendants;
   conflicts are stored in commits, not blocking).
5. For any other workspace whose @ went stale: jj workspace update-stale
6. Force-push remaining branches (jj git push).
7. Retarget remaining PRs: set the new bottom branch's base to trunk; fix any base
   that pointed at a deleted branch.
8. Report: merged branches, conflicts that need resolution, new bases.
```

If step 4 produces conflicts, **do not halt** — finish the sync, then surface a git-style
conflict summary and the `jjk` command to resolve (see §9).

---

## 7. `jjk ls` rendering

Build from a single `jj log` call over the stack revset (e.g. `trunk()..@` plus sibling
branches), using a **template** for machine-readable fields. For each branch show: bookmark
name, PR number + state, commit count, and a marker for the current branch and `@`. Example
shape:

```
  feat-c   #1203 (open)    ↑
* feat-b   #1202 (open)    ← current
  feat-a   #1201 (merged)
  trunk
```

---

## 8. Worktrees → jj workspaces

git worktrees map to **`jj workspace`**, which is a better fit:
- All workspaces share the same repo, commits, and operation log — a commit in one workspace is
  immediately visible in others (no fetch/push between them).
- Each workspace has its own working-copy commit, addressable as `<name>@`.

| Command | jj translation | Notes |
|---|---|---|
| `jjk worktree add <path> [NAME]` | `jj workspace add <path>` + position `@` on branch NAME | Great for running multiple coding agents in parallel, each on a different branch. |
| `jjk worktree list` | `jj workspace list` | |
| `jjk worktree remove <path>` | `jj workspace forget <name>` | |

**Required handling:**
- **"Current branch" is per-workspace.** Derive it from each workspace's own `@` (nearest
  ancestor bookmark). Do not store a single global current branch.
- **Stale working copies.** When `jjk sync`/`restack`/`commit` rewrites commits, other
  workspaces' `@` can become stale. Detect and run `jj workspace update-stale` automatically
  (or prompt). Always check for staleness at the start of any command that reads `@`.

---

## 9. Resolved policy decisions (git ↔ jj impedance)

- **D1 Staging area:** No index. `jjk commit -m` commits **all** current changes. `jjk add`
  prints a friendly "not needed" note. (Optional later: `jjk commit -p`/`--interactive` →
  `jj split` for partial commits.) The multi-commit-per-branch model means users save
  incremental progress with multiple `commit`s, not with staging.
- **D2 Bookmark advancement:** `jjk commit` must move the current bookmark to the new commit
  (§3.3). Non-optional.
- **D3 Current branch:** Tracked per-workspace, derived from `@`'s nearest ancestor bookmark.
- **D4 Conflict UX:** Operations **complete** (jj stores conflicts); never emulate git's
  stop-the-world. After an op, print a git-flavored summary
  (`CONFLICT: N change(s) need resolution — run 'jjk status'`). Resolution flow: user edits
  conflicted files, then `jjk` re-snapshots `@` and the resolution propagates to descendants.
- **D5 Push strategy:** v1 uses **force-with-lease** (rebase + force-push). Consider an
  append-merge mode (sync downstream via merge commits to avoid force-push noise on PR
  timelines) as a later flag.

---

## 10. Rust implementation notes

- **Two pluggable adapters per port (see §4.1).** The engine depends only on the `Vcs`/`Forge`
  traits. The default and always-available fallback is the **binary** adapter
  (`jj`/`gh` subprocess); an opt-in **crate** adapter (`jj-lib`/`octocrab`) is selected by config
  for in-process speed and atomic batched transactions.
- **Binary adapter (`jj_cli`):** parse **templated** output (`jj log -T '<template>' --no-graph`)
  for anything read back — never scrape human-readable output. **Pin a specific jj version** and
  verify every flag against `jj help` for that version (flags have churned). Centralize all
  subprocess calls in this one module so version quirks live in one place.
- **Crate adapter (`jj_lib`):** `jj-lib` has **no API stability guarantees** and changes across
  jj releases — pin it to an exact version and expect maintenance on upgrades. Its real advantage
  is grouping a command's mutations into a single jj-lib transaction (atomic, one op-log entry,
  no repeated process spawn), not raw per-call latency. Build it only after the binary adapter
  works and the §4.1 conformance suite is green; enable it where profiling justifies it.
- **A shared conformance suite runs against BOTH adapters** to guarantee behavioral parity, so
  the config switch is genuinely free.
- **Colocated repo** (`--colocate`) so plain `git` works alongside as a safety net.
- **Forge adapters:** `octocrab` (async, in-process) and `gh` (subprocess). Read the token from
  `gh auth token` or env to reuse existing auth.
- **CLI:** `clap` (derive). **Errors:** `anyhow` + `thiserror`. **State:** `serde` + `toml`.
  **Process exec:** `std::process::Command` (or `xshell`/`duct`). **Async:** `tokio`,
  `async-trait`.

---

## 11. Build phases (suggested order)

Phases 1–5 are implemented **against the `Vcs`/`Forge` traits using the binary adapters**, which
establishes the conformance baseline. The crate adapters come last (Phase 6).

1. **Local feel (no GitHub).** `repo init`, `commit`, `commit --amend`, `checkout -b`,
   `checkout`, `branch create`, `status`, `ls`, `up`/`down`/`top`/`bottom`, `undo`.
   Goal: it feels exactly like git. Nail D1–D4 here.
2. **Stack derivation.** Reconstruct the graph from revsets; verify auto-rebase on amend and
   mid-stack commit; `restack`.
3. **Forge.** `submit`: create/update PRs and set bases bottom-up. `push`, `fetch`, `pull`.
4. **`sync`.** Fetch + merged-detection + abandon-empty + auto-rebase + retarget. Hardest;
   do last of the core.
5. **Worktrees + polish.** `worktree` commands, stale-handling, `stash`, conflict messaging,
   partial commit.
6. **Crate adapters — DONE.** `vcs::jj_lib` (in-process, the sole VCS adapter) and
   `forge::octocrab` (default forge) are implemented behind the existing traits and are now the
   default. The whole integration suite (the conformance gate) passes against them, and the `jj`
   binary is no longer required. `forge::gh_cli` remains as a config-selectable fallback.

---

## 12. Edge cases / gotchas for the implementer

- **Empty working `@`** must be excluded from PR ranges and from `ls`. jj auto-abandons an
  empty, undescribed, non-bookmarked `@` when you move away — verify this on the pinned version
  and clean up explicitly if not.
- **First push of a new bookmark** may require `--allow-new` (recent jj).
- **Mid-stack commit** must rebase the upstack child onto the new commit (§3.3 step 3).
- **Stale workspaces** after any history rewrite (§8).
- **Squash vs merge-commit landing** are both handled by empty-change detection in `sync`; do
  not special-case the merge method.
- **Conflicts are objects, not stop-states** — every command must be written to tolerate a
  conflicted commit existing in the stack and report it rather than abort.
- **Trunk name** isn't always `main`/`master`; detect via `jj`'s `trunk()` revset / remote HEAD
  and store it.
- **Backend parity is a hard requirement.** Both adapters must produce identical `model`-level
  results; codify the Phase 0 probe behaviors as the conformance suite (§4.1) and run it against
  each adapter. Any divergence is a bug, not a "backend difference."
- **jj-lib does not auto-snapshot the working copy.** The `jj` binary snapshots `@` on every
  command; `jj-lib` does not — the `jj_lib` adapter must explicitly snapshot the working copy
  before reads/mutations, or it will operate on stale state and diverge from the binary adapter.
