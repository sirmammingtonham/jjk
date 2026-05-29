# jjk — Coding Agent Brief

You are implementing **`jjk`**, a Rust CLI that exposes familiar **git / git-spice command
semantics** while driving **Jujutsu (jj)** underneath, to manage stacked GitHub PRs.

**`ARCHITECTURE.md` is the source of truth for *what* to build.** This document governs *how*
to build it. Read ARCHITECTURE.md fully before writing any code. If this brief and the spec
ever conflict, follow the spec and flag the conflict.

---

## Non-negotiable operating principles

1. **Never assume a jj command or flag from memory.** jj is pre-1.0 and its CLI has churned
   (e.g. `branch` was renamed `bookmark`). Your training data is likely stale. Confirm every jj
   command against the actual pinned binary before coding against it. When unsure, run it in a
   scratch repo and read the real output.

2. **Program against the `Vcs`/`Forge` traits, never a concrete backend (ARCHITECTURE.md §4.1).**
   No `jj-lib` type and no CLI/JSON shape may leak into `engine`. The default, always-available
   backend is the **binary** adapter (`jj`/`gh` subprocess) — build everything against it first.
   In the binary adapter, parse machine-readable **templated** output
   (`jj log -T '<template>' --no-graph`), never scrape human-formatted output, and centralize all
   subprocess calls in that adapter module. The **crate** adapter (`jj-lib`/`octocrab`) is an
   opt-in later phase (Phase 6) behind a config switch; do **not** start it until the binary
   adapter passes the conformance suite.

3. **Let jj do the VCS work. Do not reimplement it.** No hand-rolled rebasing, no graph
   persistence, no staging area, no stop-the-world conflict handling. If you find yourself
   writing rebase logic, you've taken a wrong turn — derive state from jj and call jj.

4. **Build strictly phase by phase. Do not start a phase until the previous phase's gate is
   met.** The gates are acceptance tests, not "it compiles." In particular, Phase 1 must
   genuinely *feel like git* before any forge code exists.

5. **The five policy decisions in ARCHITECTURE.md §9 (D1–D5) are settled. Do not relitigate
   them.** If you believe one is wrong, stop and raise it with the human rather than silently
   diverging.

6. **Surface, don't guess.** When the spec is genuinely ambiguous or a jj behavior doesn't
   match the spec's assumption, stop and ask. A wrong assumption baked into Phase 1 is far more
   expensive than a question.

7. **Pass jj's errors through.** `jjk` is a translator; when jj fails, show the user jj's real
   error rather than swallowing or rewording it into something less actionable.

---

## Phase 0 — Behavior probe (do this first)

Before any application code, empirically confirm the load-bearing jj behaviors and record the
**exact** commands and outputs in a new file `JJ_NOTES.md`. Pick the latest stable jj release,
record its version, and pin it (CI + a documented requirement).

In a throwaway colocated repo, confirm and write down the real invocation for each of:
- create/move/delete/list a bookmark (`set` vs `move` semantics; what `--allow-new` is needed for on first push)
- `jj new` vs `jj new --insert-after`/`-A` behavior, and what happens to existing children
- whether an empty, undescribed, non-bookmarked `@` is auto-abandoned when you move away
- how a conflict materializes in a commit after a rebase (and how resolution propagates)
- how a change becomes **empty** after rebasing onto a trunk that already contains its content
  (test BOTH a squash-merge and a merge-commit landing)
- `trunk()` / remote-HEAD detection for non-`main` trunk names
- `jj workspace add` and what makes another workspace's `@` go stale + `update-stale`
- the template syntax to extract: change id, commit id, bookmarks, description, empty flag,
  conflict flag, parent ids

**Gate:** `JJ_NOTES.md` exists, with confirmed commands for every item above, and a pinned jj
version. Every later phase cites JJ_NOTES.md rather than guessing. **These confirmed behaviors
become the specification for the §4.1 conformance suite** that later guards backend parity.

---

## Phase 1 — Local feel (no GitHub)

Implement: `repo init`, `commit -m`, `commit --amend`, `checkout -b`, `checkout`,
`branch create`, `status`, `ls`, `up`/`down`/`top`/`bottom`, `undo`. Implement the commit
algorithm and empty-`@` invariant exactly as in ARCHITECTURE.md §3.

First, scaffold the boundary: the `model` domain types and the `Vcs`/`Forge` traits
(ARCHITECTURE.md §4.1), plus the **binary** adapters (`jj_cli`, `gh_cli`). Everything in
`engine` is written against the traits only.

**Gate (manual acceptance walkthrough — must pass before Phase 2):**
1. Init a colocated repo.
2. `branch create feat-a`; make changes; `commit -m`; make more changes; `commit -m`.
   Confirm `feat-a` now has **two** commits and `ls` shows it correctly.
3. `branch create feat-b` on top; commit once.
4. `down` to `feat-a`; `commit --amend` (or add a commit). Confirm `feat-b` **auto-restacked**
   on top with no manual step and `ls` reflects it.
5. `checkout feat-a` then `checkout feat-b` repeatedly with uncommitted changes present —
   confirm switching never errors and never loses work.
6. `undo` reverses the last operation.

If any step requires the user to think about jj, or feels unlike git, Phase 1 is not done.

Also: integration tests run against a **real temporary jj repo** (not mocks) covering steps
2–6. The VCS behavior is the risk surface; test it for real.

---

## Phase 2 — Stack derivation

Reconstruct the stack graph from revsets at runtime (no persistence beyond the branch→PR map).
Implement `restack` (expected to be near-no-op), `track`/`untrack`, `branch delete`
(heal-the-gap). Verify mid-stack commit auto-restacks the upstack (§3.3 step 3).

**Gate:** delete a middle branch and confirm the upstack reconnects to the deleted branch's
parent; commit into a middle branch and confirm the upstack rides the new commit. Tests against
a real temp repo.

---

## Phase 3 — Forge (GitHub)

Implement `fetch`, `pull`, `push`, `submit`. Abstract the forge behind a trait so tests can use
a fake, with a separate live **smoke test** against a real throwaway GitHub repo (ask the human
to provide one; do not exercise against real work repos). `submit` must be **idempotent**:
re-running creates nothing duplicate, updates existing PRs, and sets each PR's base to its
downstack branch (or trunk for the bottom).

**Gate:** from a 3-branch stack, `submit` opens 3 correctly-based PRs; edit + re-`submit`
updates them in place with no duplicates and correct bases.

---

## Phase 4 — `jjk sync` (hardest; do last)

Implement the algorithm in ARCHITECTURE.md §6 exactly. Do not special-case squash vs
merge-commit landing — rely on empty-change detection confirmed in Phase 0.

**Gate (test both landing methods):** stand up a stack, land the bottom PR by squash-merge;
run `sync`; confirm the merged change is abandoned, the rest auto-rebases onto trunk, branches
force-push cleanly, remaining PR bases are retargeted, and the next PR's diff is clean (no
duplicated/garbled content). Repeat with a merge-commit landing. Confirm conflicts during sync
are reported (not aborted) and the resolve flow works.

---

## Phase 5 — Worktrees + polish

`worktree add/list/remove` over `jj workspace`, with **per-workspace current branch** and
automatic stale detection/`update-stale` (§8). Then `stash` park/unpack, the conflict-summary
messaging (§9 D4), and optional `commit -p` → `jj split`.

**Gate:** two workspaces on different branches; a `sync` in one that rewrites history leaves
the other usable after automatic stale handling; `stash`/`pop` round-trips cleanly.

---

## Phase 6 — Crate adapters (optional, performance)

Only after Phases 1–5 are solid. Implement `vcs::jj_lib` and/or `forge::octocrab` behind the
existing traits. Build the **shared conformance suite** (seeded from `JJ_NOTES.md`) that runs the
same trait-level tests against both the binary and crate adapters over real temp repos.

**Gate:** the conformance suite passes identically for binary and crate adapters; flipping
`vcs.backend` / `forge.backend` in config changes nothing observable except speed. Pin `jj-lib`
to an exact version. Do not pursue unless profiling shows a real win; the binary adapters remain
the supported default and fallback.

---

## Definition of done (whole project)

- Every command in ARCHITECTURE.md §5 works and is covered by tests against a real temp jj repo.
- A new user who knows git but not jj can run the Phase 1 walkthrough and the stacked-PR flow
  without learning a single jj command.
- All VCS/forge access goes through the `Vcs`/`Forge` traits; no backend type leaks into
  `engine`. The binary adapter is templated and pinned to a known jj version. If a crate adapter
  is enabled, it passes the shared conformance suite identically to the binary adapter.
- `sync` produces clean upstack diffs after a bottom merge under both squash and merge-commit
  landings.

---

## Notes for the human (fill in before kicking off)
- Preferred/pinned jj version (else agent picks latest stable and records it).
- A throwaway GitHub repo for Phase 3/4 live smoke tests.
- Default push strategy confirmation: force-with-lease for v1 (D5).
