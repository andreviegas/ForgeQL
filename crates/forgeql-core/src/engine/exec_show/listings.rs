//! The `SHOW` verbs that list structure rather than quote source text —
//! `SHOW outline`, `SHOW members`, `SHOW callees` — together with `FIND files`.
//!
//! What sets these apart from the reading verbs is that their rows are built
//! rather than quoted: each produces a set that the clause pipeline then
//! filters, sorts and caps, and only the survivors are worth stamping handles
//! onto.

use std::path::PathBuf;

use anyhow::Result;

use crate::{
    ast::{query, show},
    engine::ForgeQLEngine,
    ir::{Clauses, SortDirection},
    result::FileEntry,
    storage::StorageEngine,
    workspace::Workspace,
};

use super::stamps::{stamp_error_counts, stamp_member_handles, stamp_path_handles};

impl ForgeQLEngine {
    pub(super) fn exec_show_outline(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        file: &str,
        all: bool,
    ) -> Result<serde_json::Value> {
        engine.show_outline_for_file(workspace, file, all)
    }

    pub(super) fn exec_show_members(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> Result<serde_json::Value> {
        // One clause, two consumers: the lookup that decides which `Foo` this
        // is, and the members rows it returns. Each predicate goes to the one
        // that can answer it, and `WHERE language = 'cpp'` reaches the lookup.
        let lookup = crate::filter::clauses_for_lookup::<crate::result::MemberEntry>(clauses);
        let loc = engine
            .resolve_type_symbol(symbol, &lookup, workspace.root())?
            .ok_or_else(|| super::lookup_missed(symbol, &lookup))?;
        let cached = self.get_or_parse_for_show(session_id, workspace, &loc)?;
        let req = show::ShowRequest {
            cached: &cached,
            path: &loc.path,
            byte_range_start: loc.byte_range.start,
            hint_line: Some(loc.line).filter(|&l| l > 0),
            workspace,
            symbol,
            lang_registry: &self.lang_registry,
            ordinal: None,
        };
        let mut json = show::show_members(&req)?;
        stamp_member_handles(engine, workspace, &loc.path, &mut json);
        Ok(json)
    }

    pub(super) fn exec_show_callees(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> Result<serde_json::Value> {
        // Same split as `SHOW members`: `WHERE name LIKE '%alloc%'` names a
        // callee row and filters the call list, `WHERE language = 'rust'` names
        // no callee row and so is about the function being read.
        let lookup = crate::filter::clauses_for_lookup::<crate::result::CallGraphEntry>(clauses);
        let loc = engine
            .resolve_body_symbol(symbol, &lookup, workspace.root())?
            .ok_or_else(|| super::lookup_missed(symbol, &lookup))?;
        let cached = self.get_or_parse_for_show(session_id, workspace, &loc)?;
        let req = show::ShowRequest {
            cached: &cached,
            path: &loc.path,
            byte_range_start: loc.byte_range.start,
            hint_line: Some(loc.line).filter(|&l| l > 0),
            workspace,
            symbol,
            lang_registry: &self.lang_registry,
            ordinal: None,
        };
        show::show_callees(&req)
    }

    pub(super) fn exec_show_find_files(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        clauses: &Clauses,
    ) -> Result<serde_json::Value> {
        // Fields were refused before dispatch, against the closed file-row
        // shape: `node_kind` (a file row has no kind at all), `lang`,
        // `fql_kind`, `usages` and every other symbol-row name.
        let glob = clauses.in_glob.as_deref().unwrap_or("**");
        let mut entries: Vec<FileEntry> = engine.indexed_files().map_or_else(
            || Self::walked_workspace_entries(workspace, glob, &clauses.exclude_globs),
            |stored| {
                // The stored union IS the workspace: overlay segments minus
                // the ones the dirty overlay shadows, file-only entries for
                // non-indexed files, and this session's dirty adds. Since the
                // overlay format began carrying file-only entries that list
                // is complete, and an overlay in an older format never
                // reaches a running server (it is rebuilt on open) — so no
                // query shape needs the filesystem walk for correctness. The
                // walk survives only for backends with no stored list at all.
                //
                // Under DEPTH (without GROUP BY) the grouping pass below runs
                // WHERE first and then derives its directory aggregates from
                // the surviving files itself, so standing directory rows
                // would be double-counted there; every other shape lists
                // them.
                let with_dirs = clauses.depth.is_none() || clauses.group_by.is_some();
                Self::stored_workspace_entries(stored, glob, &clauses.exclude_globs, with_dirs)
            },
        );
        let max_depth = clauses.depth.unwrap_or(usize::MAX);
        stamp_error_counts(engine, workspace.root(), clauses, &mut entries)?;

        // Rows that survive the filters but not the LIMIT. `count` alone cannot
        // tell a complete result from a capped one, and `LAST` has to know:
        // a set armed from rows the agent never saw is not a set it chose.
        let total = {
            let mut unbounded = clauses.clone();
            unbounded.limit = None;
            unbounded.offset = None;
            let mut probe = entries.clone();
            crate::filter::apply_clauses(&mut probe, &unbounded);
            probe.len()
        };

        let mut results = format_file_results(&mut entries, clauses, max_depth);
        stamp_path_handles(workspace, &mut results);
        let count = results.len();
        Ok(serde_json::json!({
            "op":      "find_files",
            "glob":    glob,
            "depth":   max_depth,
            "results": results,
            "count":   count,
            "total":   total,
        }))
    }

    /// The walking fallback, for storage backends that expose no stored file
    /// list: enumerate the worktree and convert each row. Files and
    /// directories in one list — a directory is an addressable node too, and
    /// an agent that has to run a second query to see them pays a round trip
    /// for nothing.
    fn walked_workspace_entries(
        workspace: &Workspace,
        glob: &str,
        exclude: &[String],
    ) -> Vec<FileEntry> {
        let mut raw = query::find_files(workspace, glob, exclude);
        raw.extend(query::find_dirs(workspace, glob, exclude));
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
                    error_count: None,
                    parse_coverage: None,
                    node_id: None,
                    rev: None,
                })
            })
            .collect()
    }
    /// The workspace file list served from the stored union, with directory
    /// rows derived from it — no filesystem walk, no per-file stat.
    ///
    /// When `with_dirs` is set, a directory is a row when at least one
    /// workspace file lies beneath it; its `size` is the total bytes of those
    /// files and its `count` how many they are — one source and one meaning,
    /// where the walked path used to emit the same directory twice, `size`
    /// meaning child count in one row and summed bytes in the other. An
    /// empty directory (possible only when created this session — git cannot
    /// commit one) is addressable by the handle its creation returned but is
    /// not listed here. The DEPTH shape passes `with_dirs = false`: its
    /// grouping pass applies WHERE first and then derives directory
    /// aggregates from the surviving files itself, so standing rows would be
    /// double-counted there.
    fn stored_workspace_entries(
        stored: Vec<FileEntry>,
        glob: &str,
        exclude: &[String],
        with_dirs: bool,
    ) -> Vec<FileEntry> {
        let mut files: Vec<FileEntry> = stored
            .into_iter()
            .filter(|e| query::glob_matches(&e.path, glob))
            .filter(|e| !exclude.iter().any(|ex| query::glob_matches(&e.path, ex)))
            .collect();
        if !with_dirs {
            return files;
        }

        let mut dirs: std::collections::BTreeMap<PathBuf, (u64, usize)> =
            std::collections::BTreeMap::new();
        for f in &files {
            for anc in f.path.ancestors().skip(1) {
                if anc.as_os_str().is_empty() {
                    break;
                }
                let slot = dirs.entry(anc.to_path_buf()).or_default();
                slot.0 = slot.0.saturating_add(f.size);
                slot.1 += 1;
            }
        }
        let dir_entries = dirs
            .into_iter()
            .filter(|(p, _)| query::glob_matches(p, glob))
            .filter(|(p, _)| !exclude.iter().any(|ex| query::glob_matches(p, ex)))
            .map(|(p, (bytes, files_beneath))| FileEntry {
                depth: Some(p.components().count()),
                path: PathBuf::from(format!("{}/", p.display())),
                extension: String::new(),
                size: bytes,
                count: Some(files_beneath),
                error_count: None,
                parse_coverage: None,
                node_id: None,
                rev: None,
            });
        files.extend(dir_entries);
        files
    }
}

/// Base JSON object for a file entry: `path`, `extension`, `size`.
///
/// `node_id` and `rev` are stamped later, by `stamp_path_handles`, on the rows
/// that survive LIMIT.
fn file_entry_json(fe: &FileEntry) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "path":      fe.path.display().to_string(),
        "extension": fe.extension,
        "size":      fe.size,
    });
    // A directory row carries the number of files beneath it.
    if let Some(count) = fe.count {
        obj["count"] = serde_json::Value::from(count);
    }
    obj
}

/// Format file entries into result JSON, applying the clause-dependent shape:
/// depth-grouping (with its own sort/skip/limit), GROUP BY (carrying `count`),
/// or the plain filtered list.
fn format_file_results(
    entries: &mut Vec<FileEntry>,
    clauses: &Clauses,
    max_depth: usize,
) -> Vec<serde_json::Value> {
    if clauses.depth.is_some() && clauses.group_by.is_none() {
        // Depth grouping handles its own ordering/paging, so strip those here.
        let mut filter_clauses = clauses.clone();
        filter_clauses.order_by = None;
        filter_clauses.limit = None;
        filter_clauses.offset = None;
        crate::filter::apply_clauses(entries, &filter_clauses);

        let file_json: Vec<serde_json::Value> = entries.iter().map(file_entry_json).collect();
        let mut grouped = query::group_files_by_depth(&file_json, max_depth);

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

        let skip = clauses.offset.unwrap_or(0);
        if skip > 0 {
            drop(grouped.drain(..skip.min(grouped.len())));
        }
        if let Some(max) = clauses.limit {
            grouped.truncate(max);
        }
        grouped
    } else if clauses.group_by.is_some() {
        crate::filter::apply_clauses(entries, clauses);
        entries
            .iter()
            .map(|fe| {
                let mut obj = file_entry_json(fe);
                if let Some(n) = fe.count {
                    obj["count"] = serde_json::Value::from(n);
                }
                obj
            })
            .collect()
    } else {
        crate::filter::apply_clauses(entries, clauses);
        entries.iter().map(file_entry_json).collect()
    }
}
