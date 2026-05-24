use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    ast::{
        parse_cache::{CachedParse, sha1_of_bytes},
        query, show,
    },
    ir::{Backend, Clauses, ForgeQLIR, SortDirection},
    result::{FileEntry, ForgeQLResult, ShowContent},
    session::Session,
    storage::SymbolLocation,
    workspace::Workspace,
};

use super::ForgeQLEngine;
use super::{DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, convert_show_json, reject_text_filter};

/// Read the bytes for a symbol's source file, with a bare-repository fallback.
///
/// On normal working trees `file_io::read_bytes` succeeds.  On reconnected
/// bare clones (or detached worktrees where checked-out files are absent),
/// the regular read fails and we fall back to fetching the blob content
/// directly from git using the SHA-1 stored in `SymbolLocation::blob_sha`.
///
/// # Errors
/// - I/O error on a non-bare workspace.
/// - Bare workspace with no `blob_sha` available.
/// - Git object-store lookup failure.
fn read_bytes_for_show(workspace: &Workspace, location: &SymbolLocation) -> Result<Vec<u8>> {
    match crate::workspace::file_io::read_bytes(&location.path) {
        Ok(b) => Ok(b),
        Err(_) if workspace.is_bare() => {
            let sha = location.blob_sha.ok_or_else(|| {
                anyhow::anyhow!(
                    "file not on disk and no blob SHA available for '{}'",
                    location.path.display()
                )
            })?;
            workspace.read_blob_by_sha(&sha)
        }
        Err(e) => Err(e),
    }
}

/// Extract the `backend` selector from any supported SHOW / `FindFiles` op.
///
/// Returns `Backend::Default` for any op that does not carry a backend field
/// (e.g. mutation ops, which should never be dispatched here).
const fn backend_for_show_op(op: &ForgeQLIR) -> &Backend {
    match op {
        ForgeQLIR::ShowContext { backend, .. }
        | ForgeQLIR::ShowSignature { backend, .. }
        | ForgeQLIR::ShowOutline { backend, .. }
        | ForgeQLIR::ShowMembers { backend, .. }
        | ForgeQLIR::ShowBody { backend, .. }
        | ForgeQLIR::ShowCallees { backend, .. }
        | ForgeQLIR::ShowLines { backend, .. }
        | ForgeQLIR::FindFiles { backend, .. } => backend,
        _ => &Backend::Default,
    }
}

impl ForgeQLEngine {
    #[expect(
        clippy::too_many_lines,
        reason = "dispatches all SHOW variants; splitting would require significant restructuring"
    )]
    pub(super) fn exec_show(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let backend = backend_for_show_op(op);
        let (workspace, engine) = self.require_workspace_and_engine_for(session_id, backend)?;
        let root = workspace.root();

        let json = match op {
            ForgeQLIR::ShowContext {
                symbol, clauses, ..
            } => {
                let context_lines = clauses.depth.unwrap_or(DEFAULT_CONTEXT_LINES);
                engine
                    .resolve_symbol(symbol, clauses, root)
                    .and_then(|opt| {
                        opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found"))
                    })
                    .and_then(|loc| {
                        let bytes = read_bytes_for_show(&workspace, &loc)?;
                        show::show_context(
                            &bytes,
                            &loc.path,
                            loc.byte_range.start,
                            &workspace,
                            symbol,
                            context_lines,
                        )
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowSignature {
                symbol, clauses, ..
            } => engine
                .resolve_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached = self.get_or_parse_for_show(session_id, &workspace, &loc)?;
                    show::show_signature(
                        &cached,
                        &loc.path,
                        loc.byte_range.start,
                        &loc.node_kind,
                        &workspace,
                        symbol,
                        &self.lang_registry,
                    )
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowOutline { file, .. } => engine
                .show_outline_for_file(&workspace, file)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowMembers {
                symbol, clauses, ..
            } => engine
                .resolve_type_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached = self.get_or_parse_for_show(session_id, &workspace, &loc)?;
                    show::show_members(&cached, &loc.path, &workspace, symbol, &self.lang_registry)
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowBody {
                symbol, clauses, ..
            } => engine
                .resolve_body_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached = self.get_or_parse_for_show(session_id, &workspace, &loc)?;
                    show::show_body(
                        &cached,
                        &loc.path,
                        loc.byte_range.start,
                        Some(loc.line).filter(|&l| l > 0),
                        &loc.enrichment,
                        &workspace,
                        symbol,
                        Some(clauses.depth.unwrap_or(DEFAULT_BODY_DEPTH)),
                        &self.lang_registry,
                    )
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowCallees {
                symbol, clauses, ..
            } => engine
                .resolve_body_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached = self.get_or_parse_for_show(session_id, &workspace, &loc)?;
                    show::show_callees(
                        &cached,
                        &loc.path,
                        loc.byte_range.start,
                        Some(loc.line).filter(|&l| l > 0),
                        &workspace,
                        symbol,
                        &self.lang_registry,
                    )
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => show::show_lines(&workspace, file, *start_line, *end_line)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::FindFiles { clauses, .. } => {
                reject_text_filter(clauses)?;
                let glob = clauses.in_glob.as_deref().unwrap_or("**");
                // Fast path: when the columnar backend is active, use the
                // mmap-backed overlay segment list instead of walking the
                // filesystem.  On Zephyr (~35 K files) this avoids ~35 000
                // stat(2) syscalls and reduces latency from ~1–2 s to < 5 ms.
                //
                // Guard: only engage the fast path when the query has a
                // `WHERE extension = 'X'` equality predicate AND the overlay
                // already contains files with that extension.
                //
                // Overlays built with this version of ForgeQL track ALL
                // workspace files (source, docs, images, build artefacts) with
                // path + size information, so the fast path naturally covers
                // extensions like `.cmake`, `.rst`, `.png`, and `.elf` once the
                // overlay is rebuilt for the current commit.
                //
                // Overlays built with older code only contain source files;
                // for those, the `.filter(|ext| indexed.iter().any(...))` guard
                // below correctly falls back to the filesystem walk for any
                // extension that is absent from the overlay.
                //
                // Queries with no extension predicate (e.g. ORDER BY size DESC)
                // always use the filesystem walk since we cannot assert that
                // an old overlay covers every file type.
                let indexed_opt = engine.indexed_files();
                let fast_path_ext: Option<&str> = indexed_opt.as_ref().and_then(|indexed| {
                    use crate::ir::{CompareOp, PredicateValue};
                    // Find the first equality extension predicate.
                    clauses
                        .where_predicates
                        .iter()
                        .find_map(|p| {
                            if (p.field == "extension" || p.field == "ext")
                                && p.op == CompareOp::Eq
                                && let PredicateValue::String(s) = &p.value
                            {
                                return Some(s.as_str());
                            }
                            None
                        })
                        // Only use the fast path if the extension is in the overlay.
                        .filter(|ext| indexed.iter().any(|fe| fe.extension == *ext))
                });
                let mut entries: Vec<FileEntry> = if fast_path_ext.is_some() {
                    // SAFETY: fast_path_ext.is_some() implies indexed_opt.is_some().
                    #[expect(
                        clippy::unwrap_used,
                        reason = "fast_path_ext.is_some() implies indexed_opt.is_some() — invariant established above"
                    )]
                    indexed_opt.unwrap()
                } else {
                    // Filesystem walk fallback — used for non-indexed extensions
                    // (e.g. `.rst`, `.png`, `.pat`) and for size-only queries.
                    let raw = query::find_files(&workspace, glob, clauses.exclude_glob.as_deref());
                    raw.iter()
                        .filter_map(|v| {
                            let path = v.get("path").and_then(|p| p.as_str()).map(PathBuf::from)?;
                            let extension = v
                                .get("extension")
                                .and_then(|e| e.as_str())
                                .unwrap_or("")
                                .to_string();
                            let size = v
                                .get("size")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let depth = Some(path.components().count());
                            Some(FileEntry {
                                path,
                                extension,
                                size,
                                depth,
                                count: None,
                            })
                        })
                        .collect()
                };
                // Apply the full clause pipeline (IN, EXCLUDE, WHERE, GROUP BY,
                // ORDER BY, LIMIT, OFFSET).  For the filesystem walk fallback,
                // IN/EXCLUDE were already applied by find_files() and become no-ops.
                //
                // When DEPTH is requested (and no GROUP BY), we must apply ORDER BY
                // and LIMIT *after* depth-grouping, not before.  Running ORDER BY
                // size DESC + LIMIT N first would select N deeply-nested files that
                // all share a long common prefix; common_prefix_depth() would then
                // report a high prefix depth, making every file appear shallower
                // than it really is and defeating the depth filter entirely.
                // The correct pipeline is: filter (WHERE) → group by depth → sort →
                // limit.
                let max_depth = clauses.depth.unwrap_or(usize::MAX);
                let results: Vec<serde_json::Value> =
                    if clauses.depth.is_some() && clauses.group_by.is_none() {
                        // Step 1: apply IN/EXCLUDE/WHERE on the full entry set —
                        // deliberately skip ORDER BY, OFFSET, and LIMIT so every
                        // matching file reaches the depth-grouping step.
                        let mut filter_clauses = clauses.clone();
                        filter_clauses.order_by = None;
                        filter_clauses.limit = None;
                        filter_clauses.offset = None;
                        crate::filter::apply_clauses(&mut entries, &filter_clauses);

                        // Step 2: group the complete filtered set by depth.
                        let file_json: Vec<serde_json::Value> = entries
                            .iter()
                            .map(|fe| {
                                serde_json::json!({
                                    "path":      fe.path.display().to_string(),
                                    "extension": fe.extension,
                                    "size":      fe.size,
                                })
                            })
                            .collect();
                        let mut grouped = query::group_files_by_depth(&file_json, max_depth);

                        // Step 3: sort the grouped results (individual files and
                        // directory summaries both carry a "size" field).
                        if let Some(ref order_by) = clauses.order_by {
                            let dir = order_by.direction;
                            let field = order_by.field.clone();
                            grouped.sort_by(|a, b| {
                                let cmp = if let (Some(va), Some(vb)) = (
                                    a.get(&field).and_then(serde_json::Value::as_u64),
                                    b.get(&field).and_then(serde_json::Value::as_u64),
                                ) {
                                    va.cmp(&vb)
                                } else {
                                    let sa = a
                                        .get(&field)
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or("");
                                    let sb = b
                                        .get(&field)
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or("");
                                    sa.cmp(sb)
                                };
                                match dir {
                                    SortDirection::Desc => cmp.reverse(),
                                    SortDirection::Asc => cmp,
                                }
                            });
                        }

                        // Step 4: apply OFFSET then LIMIT.
                        let skip = clauses.offset.unwrap_or(0);
                        if skip > 0 {
                            drop(grouped.drain(..skip.min(grouped.len())));
                        }
                        if let Some(max) = clauses.limit {
                            grouped.truncate(max);
                        }
                        grouped
                    } else if clauses.group_by.is_some() {
                        // When GROUP BY was requested the pipeline has already
                        // aggregated entries and stored per-group counts; skip the
                        // depth-grouping step so those results are not disturbed.
                        crate::filter::apply_clauses(&mut entries, clauses);
                        entries
                            .iter()
                            .map(|fe| {
                                let mut obj = serde_json::json!({
                                    "path":      fe.path.display().to_string(),
                                    "extension": fe.extension,
                                    "size":      fe.size,
                                });
                                if let Some(n) = fe.count {
                                    obj["count"] = serde_json::Value::from(n);
                                }
                                obj
                            })
                            .collect()
                    } else {
                        // No DEPTH, no GROUP BY: standard apply_clauses pipeline.
                        // Always show individual files — the LIMIT clause (default 20)
                        // already caps how many entries are returned.
                        crate::filter::apply_clauses(&mut entries, clauses);
                        entries
                            .iter()
                            .map(|fe| {
                                serde_json::json!({
                                    "path":      fe.path.display().to_string(),
                                    "extension": fe.extension,
                                    "size":      fe.size,
                                })
                            })
                            .collect()
                    };
                let count = results.len();
                serde_json::json!({
                    "op":      "find_files",
                    "glob":    glob,
                    "depth":   max_depth,
                    "results": results,
                    "count":   count,
                })
            }
            other => serde_json::json!({ "error": format!("not a show op: {other:?}") }),
        };

        // Check for error responses.
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            bail!("{err}");
        }

        // Convert the JSON value to a typed ShowResult.
        let mut show_result = convert_show_json(op, &json)?;

        // Apply the full clause pipeline (WHERE, ORDER BY, LIMIT, OFFSET, …)
        // to structured list results: outline, members, and call graph entries.
        match (&mut show_result.content, op) {
            (ShowContent::Outline { entries }, ForgeQLIR::ShowOutline { clauses, .. }) => {
                crate::filter::apply_clauses(entries, clauses);
            }
            (ShowContent::Members { members, .. }, ForgeQLIR::ShowMembers { clauses, .. }) => {
                crate::filter::apply_clauses(members, clauses);
            }
            (ShowContent::CallGraph { entries, .. }, ForgeQLIR::ShowCallees { clauses, .. }) => {
                // Default sort for callees is by call-site line (ascending) so
                // the output reflects call order.  If the user supplied an
                // explicit ORDER BY, respect it instead.
                if clauses.order_by.is_none() {
                    let mut effective = clauses.clone();
                    effective.order_by = Some(crate::ir::OrderBy {
                        field: "line".to_string(),
                        direction: crate::ir::SortDirection::Asc,
                    });
                    crate::filter::apply_clauses(entries, &effective);
                } else {
                    crate::filter::apply_clauses(entries, clauses);
                }
            }
            _ => {}
        }

        // Extract clauses for ShowContent::Lines variants.
        let show_clauses: Option<&Clauses> = match op {
            ForgeQLIR::ShowBody { clauses, .. }
            | ForgeQLIR::ShowLines { clauses, .. }
            | ForgeQLIR::ShowContext { clauses, .. } => Some(clauses),
            _ => None,
        };

        // SHOW LINES n-m has a user-specified range — the implicit 40-line cap
        // should NOT block it.  Only SHOW body / SHOW context are subject to
        // the implicit cap (they can produce unbounded output).
        let is_explicit_range = matches!(op, ForgeQLIR::ShowLines { .. });

        // Apply WHERE predicates BEFORE the line caps.
        // This lets queries like `SHOW body OF 'fn' WHERE text MATCHES 'TODO'`
        // filter over the full function body, not just the first N lines.
        if let (ShowContent::Lines { lines, .. }, Some(clauses)) =
            (&mut show_result.content, show_clauses)
        {
            for predicate in &clauses.where_predicates {
                let pred = predicate.clone();
                lines.retain(|line| crate::filter::eval_predicate(line, &pred));
            }
        }

        // Apply all caps: explicit LIMIT/OFFSET, implicit DEFAULT_SHOW_LINE_LIMIT,
        // and budget critical cap.
        let budget_max = session_id
            .and_then(|sid| self.sessions.get(sid))
            .and_then(Session::budget_critical_max_lines);
        Self::apply_show_lines_cap(
            &mut show_result,
            show_clauses,
            budget_max,
            is_explicit_range,
        );

        Ok(ForgeQLResult::Show(show_result))
    }

    /// Get a cached parse for a symbol location, with bare-repo fallback.
    ///
    /// Uses the session's `ParseCache` (capacity 32) when a session is active;
    /// falls back to a one-shot parse when no session is available.
    ///
    /// Reading strategy:
    /// - **Cache hit** (by `blob_sha`): returns immediately — no file or git read.
    /// - **Cache miss**: calls `read_bytes_for_show` which transparently falls back
    ///   to `Workspace::read_blob_by_sha` on bare repos where the file is absent.
    ///
    /// If `blob_sha` is `None`, bytes are read from disk and the SHA-1 is
    /// computed from the content (legacy backend behaviour).
    fn get_or_parse_for_show(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        loc: &SymbolLocation,
    ) -> Result<Arc<CachedParse>> {
        use crate::ast::parse_cache::ParseCache;

        if let Some(sid) = session_id
            && let Some(session) = self.sessions.get(sid)
        {
            let mut guard = session
                .parse_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // Fast path: cache hit by blob SHA — no I/O of any kind.
            if let Some(sha) = loc.blob_sha.as_ref()
                && let Some(hit) = guard.get(sha)
            {
                return Ok(hit);
            }

            // Miss (or no SHA hint): read bytes with bare-repo fallback, then parse.
            let bytes = read_bytes_for_show(workspace, loc)?;
            let hash = loc.blob_sha.unwrap_or_else(|| sha1_of_bytes(&bytes));
            return guard.get_or_parse_with_bytes(hash, &loc.path, bytes, &self.lang_registry);
        }

        // No active session — one-shot parse with bare-repo fallback.
        let bytes = read_bytes_for_show(workspace, loc)?;
        let hash = loc.blob_sha.unwrap_or_else(|| sha1_of_bytes(&bytes));
        ParseCache::with_capacity(1).get_or_parse_with_bytes(
            hash,
            &loc.path,
            bytes,
            &self.lang_registry,
        )
    }
}
