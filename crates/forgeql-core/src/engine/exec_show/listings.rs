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
        let indexed_opt = engine.indexed_files();
        let fast_path_ext: Option<&str> = indexed_opt.as_ref().and_then(|indexed| {
            use crate::ir::{CompareOp, PredicateValue};
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
                .filter(|ext| indexed.iter().any(|fe| fe.extension == *ext))
        });
        let mut entries: Vec<FileEntry> = if fast_path_ext.is_some() {
            #[expect(
                clippy::unwrap_used,
                reason = "fast_path_ext.is_some() implies indexed_opt.is_some() — invariant established above"
            )]
            indexed_opt.unwrap()
        } else {
            // Files and directories in one list: a directory is an addressable
            // node too, and an agent that has to run a second query to see them
            // pays a round trip for nothing.
            let mut raw = query::find_files(workspace, glob, &clauses.exclude_globs);
            raw.extend(query::find_dirs(workspace, glob, &clauses.exclude_globs));
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
        };
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
}

/// Base JSON object for a file entry: `path`, `extension`, `size`.
///
/// `node_id` and `rev` are stamped later, by `stamp_path_handles`, on the rows
/// that survive LIMIT.
fn file_entry_json(fe: &FileEntry) -> serde_json::Value {
    serde_json::json!({
        "path":      fe.path.display().to_string(),
        "extension": fe.extension,
        "size":      fe.size,
    })
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
