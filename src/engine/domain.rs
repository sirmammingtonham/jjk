//! `jjk domain ...` — Domain Expansion engine methods: activation/status/explain/collapse,
//! the split pipeline (compute/hierarchical), scratch-workspace reconstruction, the review-gated
//! (re)expansion, and domain-aware sync. The `Engine` struct + shared helpers live in the parent.

use crate::engine::expansion::{self, ExpansionState, Materialized, Mode, PersistedLayer};
use crate::error::{JjkError, Result};
use crate::model::{ChangeId, FileChangeKind};
use crate::prompt::SplitReview;
use crate::vcs::Vcs;
use super::HIERARCHICAL_THRESHOLD;
use std::path::{Path, PathBuf};

use super::{Engine, Report, SubmitOptions, SubmitScope};

// ---------------------------------------------------------------- domain expansion (experimental)

impl Engine {
    /// `jjk domain expansion` — activate auto-stacking on the current branch (the monolith). Records
    /// mode/instruction/verify in the sidecar; the actual split happens on the next `submit`/`sync`
    /// (or `domain expand`).
    pub async fn domain_activate(
        &mut self,
        mode: Mode,
        instruction: Option<String>,
        verify: Option<String>,
    ) -> Result<Report> {
        let mut report = Report::default();
        let branch = self.current_branch().await?.ok_or(JjkError::NotOnBranch)?;
        let mut st = ExpansionState::load(&self.root)?
            .unwrap_or_else(|| ExpansionState::new(branch.clone(), mode));
        st.monolith = branch.clone();
        st.mode = mode;
        st.instruction = instruction;
        st.verify_cmd = verify;
        st.save(&self.root)?;
        report.note(format!("domain expansion active on '{branch}' (mode: {mode})"));
        report.note("run `jjk submit` to split it into a reviewable stack");
        Ok(report)
    }

    /// `jjk domain status` — read-only map of the monolith and its generated layers.
    pub async fn domain_status(&self) -> Result<Report> {
        let mut report = Report::default();
        let Some(st) = ExpansionState::load(&self.root)? else {
            report.note("domain expansion is not active (run `jjk domain expansion`)");
            return Ok(report);
        };
        report.note(format!("monolith: {} (mode: {})", st.monolith, st.mode));
        if let Some(instr) = &st.instruction {
            report.note(format!("instruction: {instr}"));
        }
        if let Some(v) = &st.verify_cmd {
            report.note(format!("verify: {v}"));
        }
        if st.layers.is_empty() {
            report.note("no layers yet — run `jjk submit` or `jjk domain expand`");
        } else {
            report.note(format!("{} layer(s) (bottom→top):", st.layers.len()));
            for (i, l) in st.layers.iter().enumerate() {
                let pr = self
                    .state
                    .pr_of(&l.bookmark)
                    .map(|n| format!(" → #{n}"))
                    .unwrap_or_default();
                report.note(format!("  {}. {} [{}]{}", i + 1, l.title, l.slug, pr));
            }
        }
        Ok(report)
    }

    /// Compute the proposed split: diff the monolith over trunk, atomize, build dependency hints,
    /// ask the splitter to draw the boundaries, then enforce the completeness invariant. The LLM
    /// owns the grouping/ordering; this only guarantees every atom is placed exactly once.
    async fn compute_split(&self, st: &ExpansionState) -> Result<expansion::ResolvedSplit> {
        let monolith_tip = self.branch_tip(&st.monolith).await?;
        let (trunk_revset, trunk_id) = self.trunk_anchor().await?;
        let files = self.vcs.diff_hunks(&trunk_id, &monolith_tip).await?;
        let atoms = expansion::extract_atoms(&files);
        if atoms.is_empty() {
            return Err(JjkError::Msg(format!(
                "'{}' has no changes over trunk to split",
                st.monolith
            ))
            .into());
        }
        let edges = expansion::build_edges(&atoms);
        // The monolith's own commit subjects are a free grouping signal (bottom→top).
        let mut commits = self
            .vcs
            .resolve(&format!("{trunk_revset}..{}", monolith_tip.as_str()))
            .await?;
        commits.reverse();
        let commit_subjects: Vec<String> = commits
            .iter()
            .map(|c| c.subject().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // For very large changesets, split hierarchically so no single LLM call carries the whole
        // catalog; otherwise one call. Either way the LLM draws the boundaries.
        let mut plan = if atoms.len() > HIERARCHICAL_THRESHOLD {
            self.hierarchical_split(st, &atoms, &edges, &files, &commit_subjects)
                .await?
        } else {
            let input = expansion::build_split_input(
                st.mode,
                st.instruction.clone(),
                &atoms,
                &edges,
                &files,
                commit_subjects,
            );
            self.splitter().split(&input).await?
        };
        expansion::repair_completeness(&mut plan, atoms.len());
        Ok(expansion::ResolvedSplit {
            atoms,
            files,
            trunk: trunk_id,
            monolith_tip,
            plan,
        })
    }

    /// Split a large changeset in two tiers: bucket the atoms into ≤[`HIERARCHICAL_THRESHOLD`]-sized
    /// groups of whole dependency components, run the splitter on each bucket independently (bounded
    /// catalog → cheaper, cacheable calls), and concatenate the results. The LLM still decides the
    /// boundaries within each bucket.
    async fn hierarchical_split(
        &self,
        st: &ExpansionState,
        atoms: &[expansion::Atom],
        edges: &[expansion::Edge],
        files: &[crate::model::FileDiff],
        commit_subjects: &[String],
    ) -> Result<crate::llm::SplitPlan> {
        let buckets = expansion::bucket_components(atoms.len(), edges, HIERARCHICAL_THRESHOLD);
        eprintln!(
            "domain expansion: large changeset ({} atoms) — splitting in {} group(s)",
            atoms.len(),
            buckets.len()
        );
        let mut layers = Vec::new();
        for (bi, bucket) in buckets.iter().enumerate() {
            let sub_atoms: Vec<expansion::Atom> = bucket.iter().map(|&i| atoms[i].clone()).collect();
            let sub_edges = expansion::sub_edges(edges, bucket);
            let input = expansion::build_split_input(
                st.mode,
                st.instruction.clone(),
                &sub_atoms,
                &sub_edges,
                files,
                commit_subjects.to_vec(),
            );
            let mut sub = self.splitter().split(&input).await?;
            expansion::remap_and_prefix(&mut sub, bucket, bi);
            layers.extend(sub.layers);
        }
        Ok(crate::llm::SplitPlan { layers })
    }

    /// Scratch jj workspace path for reconstruction (outside the repo so it can't nest).
    fn scratch_workspace_path(&self) -> PathBuf {
        let stem = self
            .root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo");
        std::env::temp_dir().join(format!("jjk-expand-{stem}-{}", std::process::id()))
    }

    /// Rebuild the layer chain `jjk/layer/*` off trunk from `resolved.plan`, in an isolated scratch
    /// workspace (the user's working copy is never touched). Materializes each layer's cumulative
    /// file content with the patch-subset applier, then asserts `trees_equal(top, monolith)` and
    /// appends a remainder commit if anything is residual — so changes are never lost. Returns the
    /// top layer's change id (the submit/sync anchor). Updates `st.layers` and tracked state.
    async fn reconstruct_layers(
        &mut self,
        st: &mut ExpansionState,
        resolved: &expansion::ResolvedSplit,
        report: &mut Report,
    ) -> Result<ChangeId> {
        let ws_name = "jjk-expand";
        let scratch_dir = self.scratch_workspace_path();
        // Clean up any leftovers from a previous aborted run, then add a fresh workspace at trunk.
        let _ = self.vcs.forget_workspace(ws_name).await;
        let _ = std::fs::remove_dir_all(&scratch_dir);
        self.vcs
            .add_workspace(&scratch_dir, ws_name, &resolved.trunk)
            .await?;
        let scratch = crate::vcs::jj_cli::JjCli::new(&scratch_dir);

        // Reconstruct inside a guard so we always tear the scratch workspace down.
        let result = self
            .reconstruct_inner(&scratch, &scratch_dir, st, resolved, report)
            .await;

        // Teardown (best-effort) regardless of outcome.
        let _ = self.vcs.forget_workspace(ws_name).await;
        let _ = std::fs::remove_dir_all(&scratch_dir);

        result
    }

    async fn reconstruct_inner(
        &mut self,
        scratch: &crate::vcs::jj_cli::JjCli,
        scratch_dir: &Path,
        st: &mut ExpansionState,
        resolved: &expansion::ResolvedSplit,
        report: &mut Report,
    ) -> Result<ChangeId> {
        use std::collections::HashMap;
        use std::fs;

        // 1. Read each non-added touched file's trunk content (the applier's base) from the scratch
        //    working copy, which currently sits at trunk.
        let mut base: HashMap<String, String> = HashMap::new();
        for f in &resolved.files {
            let src = match f.change {
                FileChangeKind::Added => None,
                FileChangeKind::Renamed => f.old_path.clone(),
                _ => Some(f.path.clone()),
            };
            if let Some(p) = src {
                if let Ok(content) = fs::read_to_string(scratch_dir.join(&p)) {
                    base.insert(p, content);
                }
            }
        }

        // 2. Build each layer bottom→top. `included` accumulates hunks cumulatively. With --verify,
        //    a non-last layer that can't build on its own is **folded forward** into the next layer
        //    (one combined commit/bookmark/PR) — the only structural change to the LLM's plan.
        let verify_cmd = st.verify_cmd.clone();
        let mut parent = resolved.trunk.clone();
        let mut included: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut new_layers: Vec<PersistedLayer> = Vec::new();
        // (slug, title, body, atom_hashes, folded_count) accumulated but not yet sealed.
        let mut pending: Option<(String, String, String, Vec<String>, usize)> = None;
        let mut verify_failures: Vec<String> = Vec::new();
        let n_layers = resolved.plan.layers.len();

        for (li, layer) in resolved.plan.layers.iter().enumerate() {
            // Accumulate this layer's atoms into the cumulative file map and the pending group.
            let mut these_hashes = Vec::new();
            for label in &layer.atoms {
                let Some(ai) = expansion::label_index(label) else {
                    continue;
                };
                let Some(atom) = resolved.atoms.get(ai) else {
                    continue;
                };
                these_hashes.push(atom.id.clone());
                let acc = included.entry(atom.file_idx).or_default();
                for &h in &atom.hunks {
                    if !acc.contains(&h) {
                        acc.push(h);
                    }
                }
            }
            match &mut pending {
                None => {
                    pending = Some((
                        layer.slug.clone(),
                        layer.title.clone(),
                        layer.body.clone(),
                        these_hashes,
                        0,
                    ))
                }
                Some((_, _, _, hashes, folded)) => {
                    hashes.extend(these_hashes);
                    *folded += 1;
                }
            }

            // Candidate commit on `parent` with the cumulative content, snapshotted into `id`.
            let mut new_id: Option<ChangeId> = None;
            scratch.transaction(&mut |tx| {
                new_id = Some(tx.new_child(&parent)?);
                Ok(())
            })?;
            let id = new_id.expect("new_child returns an id");
            materialize_layer(scratch_dir, &resolved.files, &base, &included)?;
            scratch.snapshot().await?;

            // Verify (if configured) while this tree is `@`; fold non-last failures forward.
            let last = li + 1 == n_layers;
            let passed = match &verify_cmd {
                Some(cmd) => run_verify_cmd(scratch_dir, cmd),
                None => true,
            };
            if !passed {
                if let Some((slug, ..)) = &pending {
                    verify_failures.push(slug.clone());
                }
            }
            if passed || last {
                let (slug, title, body, hashes, folded) = pending.take().expect("pending set above");
                let title = if folded > 0 {
                    format!("{title} (+{folded} merged to build)")
                } else {
                    title
                };
                let bookmark = ExpansionState::layer_bookmark(&slug);
                let msg = if body.trim().is_empty() {
                    title.clone()
                } else {
                    format!("{title}\n\n{body}")
                };
                let bm = bookmark.clone();
                scratch.transaction(&mut |tx| {
                    tx.describe(&id, &msg)?;
                    tx.set_bookmark(&bm, &id)?;
                    Ok(())
                })?;
                self.state.branch_mut(&bookmark).tracked = true;
                new_layers.push(PersistedLayer {
                    slug,
                    bookmark,
                    atom_hashes: hashes,
                    title,
                    body,
                });
                parent = id;
            } else {
                // Folded forward: leave `parent`; the next iteration rebuilds on it with the combined
                // atoms. The orphan candidate commit is discarded with the scratch workspace.
                report.note(format!(
                    "layer '{}' didn't build alone — merging into the next layer",
                    layer.slug
                ));
            }
        }
        if let Some(cmd) = &verify_cmd {
            if verify_failures.is_empty() {
                report.note(format!("verified all layers with `{cmd}`"));
            } else {
                report.note(format!(
                    "⚠ verify (`{cmd}`) failed for {} layer(s); folded forward where possible",
                    verify_failures.len()
                ));
            }
        }

        // 3. Equivalence gate: if the reconstructed top doesn't match the monolith (binary files, a
        //    no-trailing-newline edge case, etc.), append a remainder commit whose tree == monolith.
        if !self.vcs.trees_equal(&parent, &resolved.monolith_tip).await? {
            let mut rem_id: Option<ChangeId> = None;
            scratch.transaction(&mut |tx| {
                rem_id = Some(tx.new_child(&parent)?);
                Ok(())
            })?;
            let id = rem_id.expect("new_child returns an id");
            scratch.restore_all_from(&resolved.monolith_tip).await?;
            scratch.snapshot().await?;
            let bm = ExpansionState::layer_bookmark("remainder");
            let bmc = bm.clone();
            scratch.transaction(&mut |tx| {
                tx.describe(&id, "Remaining changes\n\nResidual not captured by earlier layers.")?;
                tx.set_bookmark(&bmc, &id)?;
                Ok(())
            })?;
            self.state.branch_mut(&bm).tracked = true;
            new_layers.push(PersistedLayer {
                slug: "remainder".into(),
                bookmark: bm,
                atom_hashes: Vec::new(),
                title: "Remaining changes".into(),
                body: "Residual changes not captured by earlier layers.".into(),
            });
            parent = id;
            report.note("added a remainder layer to preserve all changes");
        }

        // 4. Hard invariant: the stack tip must equal the monolith, or we abort (never lose changes).
        if !self.vcs.trees_equal(&parent, &resolved.monolith_tip).await? {
            return Err(JjkError::Msg(
                "reconstruction did not match the monolith — aborting so no changes are lost".into(),
            )
            .into());
        }

        // 5. Forget layer bookmarks from a previous expansion that no longer exist.
        let keep: std::collections::HashSet<&str> =
            new_layers.iter().map(|l| l.bookmark.as_str()).collect();
        let stale: Vec<String> = st
            .layers
            .iter()
            .map(|l| l.bookmark.clone())
            .filter(|b| !keep.contains(b.as_str()))
            .collect();
        if !stale.is_empty() {
            self.vcs.transaction(&mut |tx| {
                for b in &stale {
                    let _ = tx.forget_bookmark(b);
                }
                Ok(())
            })?;
            for b in &stale {
                self.state.branches.remove(b);
            }
        }

        st.layers = new_layers;
        st.monolith_commit = self
            .vcs
            .resolve(resolved.monolith_tip.as_str())
            .await?
            .into_iter()
            .next()
            .map(|c| c.commit_id.0);

        report.note(format!(
            "reconstructed {} layer{} from '{}'",
            st.layers.len(),
            if st.layers.len() == 1 { "" } else { "s" },
            st.monolith
        ));
        Ok(parent)
    }

    /// If domain mode is active, (re)expand the monolith into the `jjk/layer/*` stack and return the
    /// top layer's change id (the anchor `submit`/`sync` then operate on). `None` ⇒ not in domain
    /// mode (caller proceeds with the ordinary `@`-anchored stack). Runs the review gate unless
    /// `skip_review`. Reuses any surviving layer's bookmark→PR via atom-hash matching.
    pub(in crate::engine) async fn maybe_expand_monolith(
        &mut self,
        skip_review: bool,
        report: &mut Report,
    ) -> Result<Option<ChangeId>> {
        let Some(mut st) = ExpansionState::load(&self.root)? else {
            return Ok(None);
        };
        self.ensure_fresh(report).await?;

        // Refuse to expand a conflicted monolith — reconstructing from it would build broken layers.
        let monolith_info = self
            .vcs
            .resolve(&format!("bookmarks(exact:{:?})", st.monolith))
            .await?
            .into_iter()
            .next();
        if monolith_info.as_ref().is_some_and(|c| c.has_conflict) {
            return Err(JjkError::Msg(format!(
                "'{}' has unresolved conflicts — run `jjk resolve` before expanding/submitting",
                st.monolith
            ))
            .into());
        }

        // Fast path: if the monolith hasn't moved since the last expansion and all layer bookmarks
        // still exist, reuse the existing stack — no re-split, no commit/PR churn on idempotent runs.
        let monolith_commit = monolith_info.map(|c| c.commit_id.0);
        if monolith_commit.is_some()
            && st.monolith_commit == monolith_commit
            && !st.layers.is_empty()
        {
            let bms = self.vcs.bookmarks().await?;
            let all_present = st
                .layers
                .iter()
                .all(|l| bms.iter().any(|b| b.name == l.bookmark));
            if all_present {
                if let Some(top) = self
                    .resolve_bookmark(&st.layers.last().expect("non-empty").bookmark)
                    .await?
                {
                    return Ok(Some(top));
                }
            }
        }

        let mut resolved = self.compute_split(&st).await?;
        expansion::match_layers(&mut resolved.plan, &st.layers, &resolved.atoms);
        if !skip_review {
            let changed = expansion::changed_layers(&resolved.plan, &st.layers, &resolved.atoms);
            match self.prompter.review_split(&resolved.plan, &changed)? {
                SplitReview::Accept => {}
                SplitReview::Edit(p) => {
                    resolved.plan = p;
                    expansion::repair_completeness(&mut resolved.plan, resolved.atoms.len());
                    expansion::match_layers(&mut resolved.plan, &st.layers, &resolved.atoms);
                }
                SplitReview::Abort => {
                    return Err(JjkError::Msg("split aborted; nothing changed".into()).into())
                }
            }
        }
        let top = self.reconstruct_layers(&mut st, &resolved, report).await?;
        st.save(&self.root)?;
        self.state.save(&self.root)?;
        Ok(Some(top))
    }

    /// `jjk domain expand [--preview]` — (re)build the layer stack locally for inspection (no PRs).
    /// `--preview` only prints the proposed split; otherwise the layer bookmarks are materialized so
    /// they can be inspected with `jjk ll` / `jjk branch diff` before any submit.
    pub async fn domain_expand(&mut self, preview: bool) -> Result<Report> {
        let mut report = Report::default();
        let Some(mut st) = ExpansionState::load(&self.root)? else {
            return Err(JjkError::Msg(
                "domain expansion is not active (run `jjk domain expansion` first)".into(),
            )
            .into());
        };
        self.ensure_fresh(&mut report).await?;
        let mut resolved = self.compute_split(&st).await?;
        expansion::match_layers(&mut resolved.plan, &st.layers, &resolved.atoms);
        render_split_plan(&resolved, &mut report);
        if preview {
            report.note("(preview only — no bookmarks created; run `jjk submit` to create PRs)");
            return Ok(report);
        }
        self.reconstruct_layers(&mut st, &resolved, &mut report).await?;
        st.save(&self.root)?;
        self.state.save(&self.root)?;
        report.note("inspect with `jjk ll` / `jjk domain status`; `jjk submit` creates the PRs");
        Ok(report)
    }

    /// `jjk domain explain [<layer>]` — the persisted layers with their PR-facing titles/bodies and
    /// PR numbers. With a slug, show just that layer's full body. Read-only and cheap (no LLM call).
    pub async fn domain_explain(&self, layer: Option<String>) -> Result<Report> {
        let mut report = Report::default();
        let Some(st) = ExpansionState::load(&self.root)? else {
            report.note("domain expansion is not active (run `jjk domain expansion`)");
            return Ok(report);
        };
        if st.layers.is_empty() {
            report.note("no layers yet — run `jjk submit` or `jjk domain expand`");
            return Ok(report);
        }
        for (i, l) in st.layers.iter().enumerate() {
            if let Some(sel) = &layer {
                if &l.slug != sel {
                    continue;
                }
            }
            let pr = self
                .state
                .pr_of(&l.bookmark)
                .map(|n| format!(" → #{n}"))
                .unwrap_or_default();
            report.note(format!("{}. {} [{}]{pr}", i + 1, l.title, l.slug));
            if layer.is_some() && !l.body.trim().is_empty() {
                for line in l.body.lines() {
                    report.note(format!("    {line}"));
                }
            }
        }
        Ok(report)
    }

    /// Rebase a branch's mutable chain onto the (advanced) trunk. Used by domain `sync` to shrink the
    /// monolith's diff as bottom layers land. No-op when already based on trunk.
    async fn rebase_branch_onto_trunk(&self, branch: &str, report: &mut Report) -> Result<()> {
        let (trunk_revset, trunk_id) = self.trunk_anchor().await?;
        let Some(tip) = self.resolve_bookmark(branch).await? else {
            return Ok(());
        };
        let roots = self
            .vcs
            .resolve(&format!(
                "roots(({trunk_revset}..{}) & mutable())",
                tip.as_str()
            ))
            .await?;
        let to_move: Vec<ChangeId> = roots
            .into_iter()
            .filter(|r| !r.parents.contains(&trunk_id))
            .map(|r| r.change_id)
            .collect();
        if to_move.is_empty() {
            return Ok(());
        }
        self.vcs.transaction(&mut |tx| {
            for r in &to_move {
                tx.rebase(r, &trunk_id)?;
            }
            Ok(())
        })?;
        report.note(format!("rebased '{branch}' onto trunk"));
        Ok(())
    }

    /// `jjk sync` in domain mode: fetch + advance trunk, report any merged layer PRs, rebase the
    /// monolith onto the new trunk (so landed work leaves its diff), then re-expand. With `push`,
    /// submit the regenerated stack (idempotent: updates surviving PRs, drops merged layers).
    pub(in crate::engine) async fn sync_domain(&mut self, push: bool) -> Result<Report> {
        let mut report = Report::default();
        self.ensure_fresh(&mut report).await?;
        let remote = self.state.config.remote.clone();
        let trunk = self.state.config.trunk.clone();

        let st0 = ExpansionState::load(&self.root)?.expect("domain mode (checked by caller)");
        let candidates: Vec<(String, u64)> = st0
            .layers
            .iter()
            .filter_map(|l| self.state.pr_of(&l.bookmark).map(|pr| (l.bookmark.clone(), pr)))
            .collect();

        self.vcs.fetch(&remote, Some(&trunk)).await?;
        report.note(format!("fetched {remote}"));
        self.advance_trunk_to_remote(&mut report).await?;

        // Report merged layer PRs (best-effort): they fall out of the diff after the monolith rebase.
        if !candidates.is_empty() {
            let forge = self.forge().await?;
            let flags = futures::future::try_join_all(
                candidates.iter().map(|(_, pr)| forge.is_merged(*pr)),
            )
            .await?;
            let merged: Vec<&str> = candidates
                .iter()
                .zip(flags)
                .filter(|(_, m)| *m)
                .map(|((n, _), _)| n.as_str())
                .collect();
            if !merged.is_empty() {
                report.note(format!("merged: {}", merged.join(", ")));
            }
        }

        self.rebase_branch_onto_trunk(&st0.monolith, &mut report).await?;

        // If the rebase onto the new trunk conflicted, surface it and stop *before* re-expanding —
        // building layers from a conflicted monolith would be wrong. main opens a resolve session
        // (ResumeCmd::Sync) so `jjk resolve` walks the conflict and re-runs this sync when clean.
        self.collect_conflicts(&mut report).await?;
        if !report.conflicts.is_empty() {
            report.note("monolith conflicts after rebasing onto trunk — run `jjk resolve`, then sync resumes");
            return Ok(report);
        }

        if push {
            // Re-expand and (re)submit the resulting stack in one idempotent pass.
            let sub = self
                .submit_with(
                    SubmitScope::Stack,
                    SubmitOptions {
                        draft: false,
                        no_review: true,
                    },
                )
                .await?;
            report.notes.extend(sub.notes);
            report.conflicts.extend(sub.conflicts);
            return Ok(report);
        }
        // Local-only: regenerate the layer stack without touching the remote.
        self.maybe_expand_monolith(true, &mut report).await?;
        report.note("synced local state only (--no-push)");
        Ok(report)
    }

    /// `jjk domain collapse` — deactivate. Forgets the generated layer bookmarks locally (leaving the
    /// monolith and its history untouched) and removes the sidecar. Remote PR branches are left as-is.
    pub async fn domain_collapse(&mut self) -> Result<Report> {
        let mut report = Report::default();
        let Some(st) = ExpansionState::load(&self.root)? else {
            report.note("domain expansion is not active");
            return Ok(report);
        };
        let layer_names: Vec<String> = st.layers.iter().map(|l| l.bookmark.clone()).collect();
        if !layer_names.is_empty() {
            self.vcs.transaction(&mut |tx| {
                for name in &layer_names {
                    tx.forget_bookmark(name)?;
                }
                Ok(())
            })?;
            for name in &layer_names {
                self.state.branches.remove(name);
            }
            self.state.save(&self.root)?;
        }
        ExpansionState::remove(&self.root)?;
        report.note(format!(
            "domain expansion deactivated; forgot {} layer bookmark(s)",
            layer_names.len()
        ));
        report.note(format!("your work is intact on '{}'", st.monolith));
        Ok(report)
    }
}

/// Render a proposed split into a report (layers bottom→top, with each layer's touched files,
/// rationale, and any backward-compat warning).
fn render_split_plan(r: &expansion::ResolvedSplit, report: &mut Report) {
    report.note(format!(
        "proposed split: {} layer{} (bottom→top)",
        r.plan.layers.len(),
        if r.plan.layers.len() == 1 { "" } else { "s" }
    ));
    for (i, layer) in r.plan.layers.iter().enumerate() {
        let warn = if layer.backward_compatible {
            ""
        } else {
            " ⚠ may not be self-contained"
        };
        report.note(format!("  {}. {} [{}]{warn}", i + 1, layer.title, layer.slug));
        if !layer.rationale.is_empty() {
            report.note(format!("       why: {}", layer.rationale));
        }
        let mut paths: Vec<&str> = layer
            .atoms
            .iter()
            .filter_map(|l| expansion::label_index(l))
            .filter_map(|idx| r.atoms.get(idx))
            .map(|a| a.path.as_str())
            .collect();
        paths.sort();
        paths.dedup();
        for p in paths {
            report.note(format!("       - {p}"));
        }
        if !layer.compat_notes.is_empty() {
            report.note(format!("       compat: {}", layer.compat_notes));
        }
    }
}

/// Write the cumulative content of every touched file (per `included` hunk indices) into the scratch
/// working copy, applying the patch subset onto each file's trunk `base`. Deletions remove the file;
/// binary files are skipped (the remainder commit captures them).
fn materialize_layer(
    scratch_dir: &Path,
    files: &[crate::model::FileDiff],
    base: &std::collections::HashMap<String, String>,
    included: &std::collections::HashMap<usize, Vec<usize>>,
) -> Result<()> {
    use std::fs;
    for (&fi, hidxs) in included {
        let f = &files[fi];
        let base_content = match f.change {
            FileChangeKind::Added => None,
            FileChangeKind::Renamed => f.old_path.as_deref().and_then(|p| base.get(p)),
            _ => base.get(&f.path),
        };
        match expansion::materialize_file(base_content.map(String::as_str), f, hidxs)? {
            Materialized::Write(content) => {
                let abs = scratch_dir.join(&f.path);
                if let Some(dir) = abs.parent() {
                    fs::create_dir_all(dir)?;
                }
                fs::write(&abs, content)?;
                if f.change == FileChangeKind::Renamed {
                    if let Some(old) = &f.old_path {
                        let _ = fs::remove_file(scratch_dir.join(old));
                    }
                }
            }
            Materialized::Delete => {
                let _ = fs::remove_file(scratch_dir.join(&f.path));
            }
            Materialized::Skip => {}
        }
    }
    Ok(())
}

/// Run a verify command (`sh -c <cmd>`) in `dir`; `true` on exit 0. A spawn failure counts as fail.
fn run_verify_cmd(dir: &Path, cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
