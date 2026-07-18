//! The FOUND set: arming, master-rev verification, and the bulk
//! `CHANGE / DELETE / MOVE / COPY NODES FOUND` verbs that act on it.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::ir::{ChangeTarget, ForgeQLIR};
use crate::result::{ForgeQLResult, MutationResult};
use crate::session::found_set::{self, FoundMember, FoundSet};
use crate::transforms::change::lines_to_byte_range;
use crate::transforms::copy_move::plan_copy_lines;
use crate::transforms::{FileEdit, TransformPlan};

use super::delete::remove_empty_dirs;
use super::plan::missing_ancestors;

impl ForgeQLEngine {
    /// Drop the armed set, in RAM and on disk, in one place.
    ///
    /// A mutation shifts line numbers, so the set no longer points at what the
    /// agent saw. Clearing only the in-memory copy would leave the file behind
    /// for the next process to restore — a stale set that looks live.
    pub(super) fn invalidate_found_set(&mut self, sid: &str) {
        let Some(session) = self.sessions.get_mut(sid) else {
            return;
        };
        // Clear the file unconditionally, not just when this process happened to
        // hold the set in RAM. A process that never restored it would otherwise
        // leave the file behind, and the next reconnect would resurrect a set
        // whose members this very mutation has already moved — stale handles,
        // offered to verbs that do not all demand a rev.
        drop(session.found_set.take());
        found_set::clear(&session.worktree_path);
    }
    /// The armed set, before any rev check: the two refusals that do not depend
    /// on what the code looks like now.
    ///
    /// Split from the rev check so a verb can reject a set it could never act on
    /// (usage sites are not nodes) before demanding a matching rev — the agent
    /// should not have to fix its rev to be told the set was the wrong shape.
    fn armed_found_set(&self, session_id: Option<&str>) -> Result<FoundSet> {
        let sid = require_session_id(session_id)?;
        let set = self
            .require_session(sid)?
            .found_set
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no FIND result is armed in this session — run FIND symbols/usages/files \
                     first, then re-issue the FOUND command"
                )
            })?;

        if !set.complete {
            bail!(
                "the previous {} was truncated, so no master rev was issued for it — a FOUND \
                 mutation would act on rows you were never shown. Re-run the FIND with a LIMIT \
                 that covers the whole result (or narrower filters), then repeat this command.",
                set.origin
            );
        }
        Ok(set)
    }
    /// Re-derive the master rev from the live members and compare.
    fn verify_found_rev(
        &self,
        session_id: Option<&str>,
        set: &FoundSet,
        if_rev: Option<&str>,
    ) -> Result<()> {
        let Some(expected) = if_rev else {
            return Ok(());
        };
        let sid = require_session_id(session_id)?;
        let current = self.master_rev_of(sid, &set.members)?;
        if expected != current {
            // No current rev in the payload, unlike the single-node gate: there,
            // the agent can see what it is about to overwrite in the returned
            // span. A set is N nodes it cannot see, so the only safe recovery is
            // to look again — re-running the FIND both re-shows the rows and
            // issues the rev that matches them.
            bail!(
                r#"{{"error":"rev_mismatch","scope":"last","expected":"{expected}","members":{},"origin":"{}","suggested_next":"the set moved since the FIND armed it — re-run the FIND to see the current rows and get a fresh master rev"}}"#,
                set.members.len(),
                set.origin
            );
        }
        Ok(())
    }
    /// Live `(key, rev)` for every member of a set.
    ///
    /// Read fresh on both sides of the gate — FIND to issue the master rev, the
    /// mutation to re-derive it — because a rev cached at FIND time proves only
    /// that FIND ran, not that the code still looks the way it did.
    ///
    /// A member that has since been deleted reads as `gone`, which flips the
    /// hash exactly as an edit would: it is a change to the set either way.
    fn found_set_revs(&self, sid: &str, members: &[FoundMember]) -> Result<Vec<(String, String)>> {
        let session = self.require_session(sid)?;
        let root = session.worktree_path.clone();
        let engine = session.engine_for(&crate::ir::Backend::Default)?;
        members
            .iter()
            .map(|m| {
                let rev = match &m.node_id {
                    Some(id) => engine
                        .find_node(id, &root)?
                        .map_or_else(|| "gone".to_string(), |n| n.rev),
                    // A usage site has no handle of its own — the file it sits
                    // in stands in for it, which also catches an edit that
                    // moved the line out from under it.
                    None => std::fs::read(root.join(&m.path)).map_or_else(
                        |_| "gone".to_string(),
                        |bytes| crate::node_id::format_rev(crate::node_id::rev_of_bytes(&bytes)),
                    ),
                };
                Ok((m.key(), rev))
            })
            .collect()
    }
    /// The master rev of a member list as it stands right now.
    ///
    /// The single place a master rev is derived, so the rev FIND issues and the
    /// rev the gate compares against can never be computed two different ways.
    pub(in crate::engine) fn master_rev_of(
        &self,
        sid: &str,
        members: &[FoundMember],
    ) -> Result<String> {
        Ok(FoundSet::master_rev(&self.found_set_revs(sid, members)?))
    }
    /// The 1-based inclusive line span each member contributes to a sweep.
    ///
    /// A handle contributes its whole node span (for a file handle, the whole
    /// file); a usage site contributes its single line. Spans are merged per
    /// file so two overlapping members cannot produce two edits over the same
    /// bytes.
    fn found_set_spans(
        &self,
        session_id: Option<&str>,
        set: &FoundSet,
    ) -> Result<std::collections::BTreeMap<String, Vec<(usize, usize)>>> {
        let mut by_file: std::collections::BTreeMap<String, Vec<(usize, usize)>> =
            std::collections::BTreeMap::new();
        for member in &set.members {
            let (path, span) = if let Some(id) = &member.node_id {
                let span = self.resolve_node_span(session_id, id, None)?;
                (span.rel_path, (span.start, span.end))
            } else {
                let line = member.line.ok_or_else(|| {
                    anyhow::anyhow!(
                        "a FOUND member has neither a node handle nor a line: {}",
                        member.path
                    )
                })?;
                (member.path.clone(), (line, line))
            };
            by_file.entry(path).or_default().push(span);
        }
        for spans in by_file.values_mut() {
            spans.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
            for (start, end) in spans.iter().copied() {
                match merged.last_mut() {
                    Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }
            *spans = merged;
        }
        Ok(by_file)
    }
    /// Every member's path, in FIND order, for the verbs that act on whole
    /// files. A usage site addresses a line, not a file: it is not a thing that
    /// can be deleted or moved, and saying so beats silently deleting the file
    /// that happened to contain it.
    fn found_set_paths(set: &FoundSet, verb: &str) -> Result<Vec<String>> {
        if let Some(site) = set.members.iter().find(|m| m.node_id.is_none()) {
            bail!(
                "{verb} NODES FOUND needs addressable nodes, but the set came from {} — its rows \
                 are call sites (a line in {}), not nodes. Re-run as FIND files (or FIND symbols) \
                 to arm a set of handles.",
                set.origin,
                site.path
            );
        }
        Ok(set.members.iter().map(|m| m.path.clone()).collect())
    }
    pub(in crate::engine) fn exec_change_nodes_found(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (pattern, replacement, word_boundary, if_rev) = match op {
            ForgeQLIR::ChangeNodesFound {
                pattern,
                replacement,
                word_boundary,
                if_rev,
            } => (
                pattern.as_str(),
                replacement.as_str(),
                *word_boundary,
                if_rev.as_deref(),
            ),
            _ => bail!("exec_change_nodes_found called with wrong IR variant"),
        };
        let sid = require_session_id(session_id)?;
        // Set first, then the gate: "you have nothing armed" is a more useful
        // thing to be told than "you forgot a rev" when there is no set at all.
        let set = self.armed_found_set(session_id)?;
        let if_rev = require_found_rev(if_rev, "CHANGE")?;
        self.verify_found_rev(session_id, &set, Some(if_rev))?;
        let member_count = set.members.len();
        let spans = self.found_set_spans(session_id, &set)?;
        let mut removed_spans: Vec<(String, usize, usize)> = Vec::new();

        let plan = {
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            let mut file_edits = Vec::new();
            for (rel_path, ranges) in spans {
                let abs_path = workspace.safe_path(&rel_path)?;
                let file_bytes = crate::workspace::file_io::read_bytes(&abs_path)?;
                let mut edits = Vec::new();
                for (start, end) in ranges {
                    let (span_start, span_end) = lines_to_byte_range(&file_bytes, start, end)?;
                    let range_edits = crate::transforms::matching_edits_in_range(
                        &file_bytes,
                        pattern,
                        replacement,
                        word_boundary,
                        span_start..span_end,
                    )?;
                    // A member whose whole node span is blanked by this sweep is
                    // removed, not edited: stage its freed ordinal so a byte-identical
                    // sibling cannot adopt the dead handle (the IF REV blind spot,
                    // otherwise reached through this verb). A non-empty or partial
                    // replacement leaves a node behind, so it keeps its handle.
                    if !range_edits.is_empty()
                        && span_becomes_blank(&file_bytes, span_start, span_end, &range_edits)
                    {
                        removed_spans.push((rel_path.clone(), start, end));
                    }
                    edits.extend(range_edits);
                }
                if edits.is_empty() {
                    continue;
                }
                file_edits.push(FileEdit {
                    path: abs_path,
                    edits,
                    delete: false,
                });
            }
            if file_edits.is_empty() {
                bail!(
                    "no occurrences of '{pattern}' within the {member_count} node(s) of the \
                     previous {} result",
                    set.origin
                );
            }
            TransformPlan {
                file_edits,
                suggestions: Vec::new(),
            }
        };

        for (rel_path, start, end) in &removed_spans {
            self.stage_removed_span(session_id, rel_path, *start, *end)?;
        }
        self.apply_plan(sid, plan, "change_nodes_found", None)
    }
    /// `DELETE NODES FOUND IF REV 'master'` — unlink every member of the set.
    ///
    /// Lowered to the same whole-path delete as `DELETE NODE`, but as one plan:
    /// a half-applied bulk delete is not something an agent can reason about.
    pub(in crate::engine) fn exec_delete_nodes_found(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let if_rev = match op {
            ForgeQLIR::DeleteNodesFound { if_rev } => if_rev.as_deref(),
            _ => bail!("exec_delete_nodes_found called with wrong IR variant"),
        };
        let if_rev = require_found_rev(if_rev, "DELETE")?;
        let set = self.armed_found_set(session_id)?;
        let paths = Self::found_set_paths(&set, "DELETE")?;
        self.verify_found_rev(session_id, &set, Some(if_rev))?;

        // A directory member expands to the files under it, exactly as the
        // single-node recursive delete does.
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let root = workspace.root().to_path_buf();
        let mut files: Vec<String> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for rel in &paths {
            let abs = workspace.safe_path(rel)?;
            if abs.is_dir() {
                dirs.push(abs.clone());
                files.extend(
                    workspace
                        .files()
                        .filter(|p| !crate::result::FileEntry::is_runtime_artifact(p))
                        .filter(|p| p.starts_with(&abs))
                        .map(|p| {
                            p.strip_prefix(&root)
                                .unwrap_or(&p)
                                .to_string_lossy()
                                .into_owned()
                        }),
                );
            } else {
                files.push(rel.clone());
            }
        }
        files.sort_unstable();
        files.dedup();

        let mut result = if files.is_empty() {
            ForgeQLResult::Mutation(MutationResult {
                op: "delete_nodes_found".to_string(),
                applied: true,
                structural_errors: Vec::new(),
                files_changed: Vec::new(),
                edit_count: 0,
                lines_written: 0,
                lines_removed: 0,
                diff: None,
                suggestions: Vec::new(),
                new_node_id: None,
                new_rev: None,
            })
        } else {
            let ir = ForgeQLIR::ChangeContent {
                files,
                target: ChangeTarget::Delete,
                clauses: crate::ir::Clauses::default(),
            };
            self.exec_mutation(session_id, &ir, false)?
        };
        for abs in &dirs {
            remove_empty_dirs(abs);
        }
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.op = "delete_nodes_found".to_string();
        }
        Ok(result)
    }
    /// `MOVE|COPY NODES FOUND … TO 'dir/'` — relocate every member into one
    /// directory, each keeping its basename.
    ///
    /// Unlike the single-node form the destination cannot be a rename: N sources
    /// cannot share one new name.
    pub(in crate::engine) fn exec_move_nodes_found_to(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
        is_move: bool,
    ) -> Result<ForgeQLResult> {
        let (dst, if_rev) = match op {
            ForgeQLIR::MoveNodesFoundTo { dst, if_rev } => (dst.as_str(), if_rev.as_deref()),
            ForgeQLIR::CopyNodesFoundTo { dst } => (dst.as_str(), None),
            _ => bail!("exec_move_nodes_found_to called with wrong IR variant"),
        };
        let verb = if is_move { "MOVE" } else { "COPY" };
        // A COPY creates; it cannot destroy what it did not read, so it needs no
        // gate. A MOVE unlinks the sources, so it does.
        let if_rev = if is_move {
            Some(require_found_rev(if_rev, verb)?)
        } else {
            None
        };
        let sid = require_session_id(session_id)?;
        let set = self.armed_found_set(session_id)?;
        let paths = Self::found_set_paths(&set, verb)?;
        self.verify_found_rev(session_id, &set, if_rev)?;

        let dst_dir = self.resolve_last_destination(session_id, dst, verb)?;

        let mut plan = TransformPlan {
            file_edits: Vec::new(),
            suggestions: Vec::new(),
        };
        let mut created_dirs: Vec<PathBuf> = Vec::new();
        {
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            let root = workspace.root().to_path_buf();
            for rel in &paths {
                let src_abs = workspace.safe_path(rel)?;
                if src_abs.is_dir() {
                    bail!(
                        "{verb} NODES FOUND: '{rel}' is a directory — move the files individually, \
                         or arm a set of files"
                    );
                }
                let basename = Path::new(rel).file_name().ok_or_else(|| {
                    anyhow::anyhow!("{verb} NODES FOUND: '{rel}' has no basename")
                })?;
                let dst_rel = Path::new(&dst_dir).join(basename);
                let dst_abs = workspace.safe_path(&dst_rel.to_string_lossy())?;
                if dst_abs == src_abs {
                    bail!("{verb} NODES FOUND: destination is the source ({rel})");
                }
                if dst_abs.exists() {
                    bail!(
                        "{verb} NODES FOUND: destination '{}' already exists — the engine will not \
                         clobber it. DELETE NODE it first, or choose another directory.",
                        dst_rel.display()
                    );
                }
                created_dirs.extend(missing_ancestors(&dst_abs, &root));
                if let Some(parent) = dst_abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // A whole-file move is a rename: copy the bytes and unlink the
                // source in the SAME plan, so the file is never briefly in two
                // places or in none.
                #[allow(clippy::naive_bytecount)]
                let line_count = crate::workspace::file_io::read_bytes(&src_abs)
                    .map_or(1, |bytes| {
                        bytes.iter().filter(|b| **b == b'\n').count().max(1)
                    });
                plan.file_edits
                    .extend(plan_copy_lines(rel, &src_abs, 1, line_count, &dst_abs)?.file_edits);
                if is_move {
                    plan.file_edits.push(FileEdit {
                        path: src_abs,
                        edits: Vec::new(),
                        delete: true,
                    });
                }
            }
        }
        self.record_created(sid, &created_dirs);

        let op_name = if is_move {
            "move_nodes_found_to"
        } else {
            "copy_nodes_found_to"
        };
        self.apply_plan(sid, plan, op_name, None)
    }
    /// Where a bulk `TO` points. Always a directory: every member keeps its own
    /// basename, so a file path would name one destination for N sources.
    fn resolve_last_destination(
        &self,
        session_id: Option<&str>,
        dst: &str,
        verb: &str,
    ) -> Result<String> {
        // A handle must resolve to a directory node.
        if let Ok(node) = self.resolve_node(session_id, dst, None) {
            if node.kind != "dir" {
                bail!(
                    "{verb} NODES FOUND: destination '{dst}' is a {} — a set moves into a \
                     directory, so every member can keep its basename",
                    node.kind
                );
            }
            return Ok(node.rel_path);
        }
        // Otherwise it is a path. It does not have to exist yet (that is the one
        // thing a handle cannot express), but it must name a directory.
        let rel = dst.trim_end_matches('/');
        if rel.is_empty() {
            bail!("{verb} NODES FOUND: destination is empty");
        }
        if Path::new(rel).extension().is_some() {
            bail!(
                "{verb} NODES FOUND: destination '{dst}' looks like a file — a set moves into a \
                 directory. Add a trailing '/' if you meant a new directory."
            );
        }
        Ok(rel.to_string())
    }
}
/// The bulk FOUND verbs that destroy require `IF REV`.
///
/// The grammar accepts the clause as optional so that a missing one lands here
/// rather than falling through to the single-node verb, which would report an
/// `invalid node_id: FOUND` — an error about the wrong thing entirely.
fn require_found_rev<'a>(if_rev: Option<&'a str>, verb: &str) -> Result<&'a str> {
    if_rev.ok_or_else(|| {
        anyhow::anyhow!(
            "{verb} NODES FOUND requires IF REV '<master rev>' — it acts on every member of the \
             set at once. Re-run the FIND: its response carries the master rev to quote here."
        )
    })
}

/// True when applying `edits` to `source[span_start..span_end]` leaves only
/// whitespace — the sweep blanked the member's whole node span, so the construct
/// is gone and its freed ordinal must be retired rather than adopted by a
/// byte-identical sibling. A non-empty or partial replacement leaves content
/// behind, so the node survives and keeps its handle.
fn span_becomes_blank(
    source: &[u8],
    span_start: usize,
    span_end: usize,
    edits: &[crate::transforms::ByteRangeEdit],
) -> bool {
    let mut sorted: Vec<&crate::transforms::ByteRangeEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start);
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = span_start;
    for e in sorted {
        if e.start > cursor {
            out.extend_from_slice(&source[cursor..e.start]);
        }
        out.extend_from_slice(e.replacement.as_bytes());
        cursor = e.end.max(cursor);
    }
    if cursor < span_end {
        out.extend_from_slice(&source[cursor..span_end]);
    }
    out.iter().all(u8::is_ascii_whitespace)
}
