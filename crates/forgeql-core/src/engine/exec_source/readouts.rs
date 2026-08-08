//! Readouts of the engine's own state: `SHOW SOURCES`, `SHOW BRANCHES`,
//! `SHOW COMMITS`, `SHOW VERSION` and `SHOW STATS`.
//!
//! Between them these read the source registry, the session table, the
//! session's git history and the build's own version — four different places,
//! none of which is the AST index. That is the line between this module and
//! `exec_show`, which owns the `SHOW` verbs that expose *code*: body, context,
//! outline, members, callees.

use anyhow::Result;

use crate::result::{
    CommitRow, ForgeQLResult, QueryResult, SessionStats, ShowContent, SourceOpResult, SymbolMatch,
};

use crate::engine::{ForgeQLEngine, require_session_id};

impl ForgeQLEngine {
    /// `SHOW SOURCES` — list all registered sources.
    #[allow(clippy::unnecessary_wraps)] // uniform Result return across all ops
    pub(in crate::engine) fn show_sources(&self) -> Result<ForgeQLResult> {
        let mut results: Vec<SymbolMatch> = self
            .registry
            .names()
            .iter()
            .filter_map(|name| {
                self.registry.get(name).map(|source| SymbolMatch {
                    name: source.name().to_string(),
                    node_kind: Some("source".to_string()),
                    fql_kind: None,
                    language: None,
                    path: Some(source.path().to_path_buf()),
                    line: None,
                    usages_count: None,
                    fields: source
                        .origin_url()
                        .map(|url| {
                            std::collections::HashMap::from([("url".to_string(), url.to_string())])
                        })
                        .unwrap_or_default(),
                    count: None,
                    node_id: None,
                    rev: None,
                })
            })
            .collect();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        let total = results.len();

        Ok(ForgeQLResult::Query(QueryResult {
            op: "show_sources".to_string(),
            results,
            total,
            metric_hint: None,
            group_by_field: None,
            hint: None,
            found_rev: None,
        }))
    }

    /// `SHOW BRANCHES [OF 'source']` — list branches of a source.
    pub(in crate::engine) fn show_branches(
        &self,
        session_id: Option<&str>,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let source_name = session.source_name.clone();

        let source_ref = self
            .registry
            .get(&source_name)
            .ok_or_else(|| anyhow::anyhow!("source {source_name} not found"))?;
        let branches = source_ref.branches().unwrap_or_default();

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "show_branches".to_string(),
            source_name: Some(source_name),
            session_id: None,
            branches,
            symbols_indexed: None,
            resumed: false,
            base_commit: None,
            message: None,
        }))
    }

    pub(in crate::engine) fn exec_show_commits(
        &self,
        session_id: Option<&str>,
        clauses: &crate::ir::Clauses,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let worktree = session.worktree_path.clone();
        let base_ref = session.branch.clone();
        let find_limit = session.output_config().find_limit;

        // A commit is not a symbol. Filtered through the open symbol shape
        // these rows accepted `WHERE path LIKE '%src%'` — a name no commit row
        // carries — and answered a confident zero; `CommitRow` is the closed
        // shape that refuses it instead.
        //
        // Before the history is read, so the refusal does not depend on there
        // being a readable worktree to read it from.
        crate::filter::reject_unresolvable_fields::<CommitRow>("SHOW COMMITS", clauses)?;
        // And a glob for the same reason: a commit row has no path, and no name
        // was resolved for one to scope, so `IN 'crates/**'` could only drop
        // every commit — the confident zero `CommitRow` exists to stop.
        Self::reject_globs("SHOW COMMITS", clauses)?;
        crate::filter::reject_depth("SHOW COMMITS", clauses)?;

        let commits = crate::git::commits_since(&worktree, &base_ref)?;
        let mut rows: Vec<CommitRow> = commits
            .into_iter()
            .map(|(hash, subject)| {
                CommitRow(SymbolMatch {
                    name: hash,
                    node_kind: Some("commit".to_string()),
                    fields: std::collections::HashMap::from([("subject".to_string(), subject)]),
                    ..SymbolMatch::default()
                })
            })
            .collect();

        crate::filter::apply_clauses(&mut rows, clauses);
        let mut results: Vec<SymbolMatch> = rows.into_iter().map(|row| row.0).collect();
        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(find_limit);
        }

        Ok(ForgeQLResult::Query(QueryResult {
            op: "show_commits".to_string(),
            results,
            total,
            metric_hint: None,
            group_by_field: None,
            hint: None,
            found_rev: None,
        }))
    }

    /// `SHOW VERSION` — report the crate version compiled into the running
    /// binary. Session-independent: reads no AST data, so it needs no active
    /// session.
    pub(in crate::engine) fn show_version() -> ForgeQLResult {
        ForgeQLResult::SourceOp(SourceOpResult {
            op: "show_version".to_string(),
            source_name: None,
            session_id: None,
            branches: Vec::new(),
            symbols_indexed: None,
            resumed: false,
            base_commit: None,
            message: Some(env!("CARGO_PKG_VERSION").to_string()),
        })
    }

    /// `SHOW STATS [FOR 'session_id']` — emit internal diagnostics for one or
    /// all active sessions.
    ///
    /// When `for_session` is `Some(sid)`, only that session is included.
    /// When `None`, all sessions with a ready index are reported.
    #[allow(clippy::unnecessary_wraps)]
    pub(in crate::engine) fn show_stats(&self, for_session: Option<&str>) -> Result<ForgeQLResult> {
        let sessions: Vec<SessionStats> = self
            .sessions
            .iter()
            .filter(|(id, _)| for_session.is_none_or(|s| *id == s))
            .filter_map(|(id, session)| {
                // PhaseFT5: two-arm path — columnar sessions have no legacy table.
                if session.has_columnar() {
                    let rows = session.engine().index_stats().map_or(0, |s| s.rows);
                    return Some(SessionStats {
                        session_id: id.clone(),
                        source: session.source_name.clone(),
                        branch: session.branch.clone(),
                        rows,
                        distinct_names: 0,
                        distinct_paths: 0,
                        usage_symbols: 0,
                        usage_sites: 0,
                        trigram_distinct: 0,
                        mem_total_bytes: 0,
                        mem_rows_bytes: 0,
                        mem_usages_bytes: 0,
                        mem_indexes_bytes: 0,
                        mem_trigram_bytes: 0,
                        mem_strings_bytes: 0,
                        by_language: std::collections::HashMap::new(),
                        by_fql_kind: std::collections::HashMap::new(),
                    });
                }
                // Legacy path.
                let index = session.index()?;
                let mem = index.mem_estimate();
                Some(SessionStats {
                    session_id: id.clone(),
                    source: session.source_name.clone(),
                    branch: session.branch.clone(),
                    rows: index.rows.len(),
                    distinct_names: mem.strings_names,
                    distinct_paths: mem.strings_paths,
                    usage_symbols: mem.usages_symbols,
                    usage_sites: mem.usages_sites,
                    trigram_distinct: mem.trigram_entries,
                    mem_total_bytes: mem.total_bytes(),
                    mem_rows_bytes: mem.rows_bytes,
                    mem_usages_bytes: mem.usages_bytes,
                    mem_indexes_bytes: mem.name_index_bytes
                        + mem.kind_index_bytes
                        + mem.fql_kind_index_bytes,
                    mem_trigram_bytes: mem.trigram_bytes,
                    mem_strings_bytes: mem.strings_bytes,
                    // Resolve interned u32 IDs to string keys for the output DTO.
                    by_language: index.stats.resolved_by_language(&index.strings),
                    by_fql_kind: index.stats.resolved_by_fql_kind(&index.strings),
                })
            })
            .collect();

        Ok(ForgeQLResult::Show(crate::result::ShowResult {
            op: "show_stats".to_string(),
            symbol: None,
            file: None,
            content: ShowContent::Stats { sessions },
            start_line: None,
            end_line: None,
            total_lines: None,
            hint: None,
            metadata: None,
        }))
    }
}
