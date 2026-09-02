//! `FIND symbols` / `FIND usages` / indexed-files queries for [`ColumnarStorage`].
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use roaring::RoaringBitmap;

use crate::ast::trigram::TRIGRAM_WIDTH;
use crate::filter::{ClauseTarget as _, apply_clauses_counted};
use crate::ir::{Clauses, CompareOp, GroupBy, PredicateValue};
use crate::result::SymbolMatch;
use crate::storage::FindPage;
use crate::storage::columnar::columnar_storage::fast_paths::{
    bare_limit_name_fast_path, find_max_rows, glob_to_path_prefix,
    group_by_enrichment_fast_path_field, group_by_file_fast_path_eligible,
    group_by_kind_fast_path_eligible, has_any_indexed_predicate, order_by_name_desc_fast_path,
    order_by_name_fast_path, order_by_name_kind_desc_fast_path, order_by_name_kind_fast_path,
    passes_resolve_glob,
};
use crate::storage::columnar::columnar_storage::{ColumnarStorage, SubstringIndex};
use crate::storage::columnar::segment_builder::ZONEMAP_NUMERIC_FIELDS;

/// Render an accepted-field list for a refusal message, straight from the
/// declaration that decides acceptance.
///
/// A refusal that names the alternatives is only useful while the names are
/// the real ones, and a hand-written list drifts the moment a field is added.
/// Fields this backend refuses outright are dropped: offering one would point
/// the agent at another error. `count` survives the filter, because a refusal
/// about `ORDER BY` should still offer the field `ORDER BY` accepts.
fn accepted_list<'a>(fields: impl IntoIterator<Item = &'a str>) -> String {
    fields
        .into_iter()
        .filter(|f| {
            !crate::field_tiers::lookup(f)
                .is_some_and(crate::field_tiers::FieldTier::is_refused_everywhere)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every field name a symbol row resolves, in one sequence.
///
/// Both accessors, because a clause reaches a row through either: `line` and
/// `usages` are numbers and `name` is a string, and all three sort.
fn symbol_row_fields() -> impl Iterator<Item = &'static str> {
    SymbolMatch::STR_FIELDS
        .iter()
        .chain(SymbolMatch::NUM_FIELDS)
        .copied()
}

/// Whether a symbol row resolves `field` through either accessor.
fn resolves_on_symbol_row(field: &str) -> bool {
    symbol_row_fields().any(|f| f == field)
}

impl ColumnarStorage {
    pub(super) fn find_symbols_impl(
        &self,
        clauses: &Clauses,
        _root: &Path,
    ) -> anyhow::Result<FindPage> {
        // Query pipeline:
        //   Stage 0 — reject WHERE fields that exist neither as core fields nor
        //             as enrichment columns anywhere in this index: they can
        //             never match, and scanning millions of rows to find that
        //             out can exhaust host memory.
        //   Stage 1 — prefilter_global: intersect indexed predicates (kind
        //             bitmap, name FST, trigram / short-prefix LIKE index) into a
        //             candidate global row-ID bitmap.
        //   Stage 2 — partition by segment, then prune the survivors: IN/EXCLUDE
        //             path globs, dirty-overlay shadows, and numeric zone maps,
        //             then bound the candidate set itself
        //             (FORGEQL_FIND_MAX_ROW_IDS) before a row is built.
        //   Stage 3 — materialise the surviving rows per segment: stamp usage
        //             counts, apply residual WHERE, collapse each segment's own
        //             duplicates before anything sheds on rank, enforce the row
        //             budget (FORGEQL_FIND_MAX_ROWS) — then union the dirty
        //             overlay, re-checking the same budget over the union.
        //   Stage 4 — deduplicate on (name, fql_kind, path, line). Stage 3 has
        //             already done this within each segment; what is left for
        //             here is a pair split across two segments built from one
        //             source path, and the dirty overlay.
        //   Stage 5 — apply residual WHERE, ORDER BY, LIMIT, OFFSET, and count
        //             the answer before the last two cut a page out of it.
        // GROUP BY and name-stream fast-paths (ORDER BY name, and a bare LIMIT
        // whose default ordering starts with name) short-circuit the pipeline.
        // Every count-serving fast path is only valid when source paths are
        // unique; duplicates overcount, so fall through to the deduplicating
        // pipeline.
        crate::filter::reject_refused_fields::<SymbolMatch>("FIND symbols", clauses)?;
        crate::filter::reject_depth("FIND symbols", clauses)?;
        self.reject_unknown_where_fields(clauses)?;
        self.reject_unknown_order_by_field(clauses)?;
        self.reject_unknown_group_by_field(clauses)?;
        let no_dup_paths = !self.overlay().has_duplicate_paths();
        // Each counted grouping hands the query back — to the pipeline below,
        // unchanged — wherever the stored counts would not be the collapsed ones.
        if group_by_kind_fast_path_eligible(clauses, self.dirty.is_empty())
            && no_dup_paths
            && let Some(page) = self.fast_group_by_kind(clauses)
        {
            return Ok(page);
        }
        if group_by_file_fast_path_eligible(clauses, self.dirty.is_empty())
            && no_dup_paths
            && let Some(page) = self.fast_group_by_file(clauses)
        {
            return Ok(page);
        }
        // A `GROUP BY` over an enrichment field the segments post per value is
        // a handful of stored cardinalities; without this it built every
        // matching row to count them, which on a multi-million-symbol corpus
        // spends the whole result budget to answer eight rows. The count path
        // hands the query back — to the pipeline below, unchanged — wherever
        // the stored counts would not be the collapsed ones.
        if let Some(page) = group_by_enrichment_fast_path_field(clauses, self.dirty.is_empty())
            .filter(|_| no_dup_paths)
            .and_then(|field| self.fast_group_by_enrichment(field, clauses))
        {
            return Ok(page);
        }
        if let Some(mut page) = self.try_order_by_name_fast_paths(clauses) {
            self.stamp_usage_counts(&mut page.rows);
            return Ok(page);
        }

        let mut by_segment = self.build_candidate_segments(clauses);
        self.prune_candidate_segments(&mut by_segment, clauses);

        let (mut results, shed) = self.materialize_all(&by_segment, clauses)?;
        // Stage 3b — union dirty overlay rows (empty when the overlay is empty).
        // Persistent rows were stamped during materialisation; dirty rows still
        // need their workspace usage counts before Stage 5 evaluates them.
        if !self.dirty.is_empty() {
            let mut dirty_results = self.dirty.materialize_all(clauses);
            self.stamp_usage_counts(&mut dirty_results);
            // The budget in materialize_all ran before this union and cannot
            // see the rows it adds: enforce it over the union too, or a
            // session's uncommitted half is unbounded exactly where its
            // committed half is not.
            let max_rows = super::super::fast_paths::find_max_rows();
            if results.len().saturating_add(dirty_results.len()) > max_rows {
                return Err(super::super::fast_paths::row_budget_exceeded(max_rows));
            }
            results.append(&mut dirty_results);
        }
        // Stage 4 — deduplicate on (name, fql_kind, path, line). This runs
        // unconditionally, and an earlier attempt to skip it was wrong: the
        // dirty overlay and two segments sharing a source path are NOT the only
        // sources of duplicates. A single file's row table can already hold two
        // rows sharing all four key fields. Skipping the pass when the overlay
        // reports unique paths and no dirty rows makes `overlay_query_parity`
        // return 266 rows where its baseline returns 264, on a fixture whose two
        // segments have different paths and no dirty overlay. The mechanism
        // behind those rows is not established here; the counterexample is
        // enough to keep the pass unconditional.
        dedupe_symbol_matches(&mut results);
        // Rows the running trim discarded matched and were distinct, so they
        // are part of the answer's size even though no page can hold them.
        let total = apply_clauses_counted(&mut results, clauses).saturating_add(shed);
        Ok(FindPage::of(results, total))
    }

    /// Stage 0 — fail fast on WHERE fields that cannot match anything.
    ///
    /// A field is accepted when a symbol row resolves it
    /// ([`ClauseTarget::STR_FIELDS`](crate::filter::ClauseTarget::STR_FIELDS)
    /// and its numeric twin), when it is a known enrichment field of any
    /// registered language, or when at least one segment of this index
    /// (persistent or dirty) stores an enrichment column with that name.
    /// Anything else — a typo, an invented field, or a real field belonging to
    /// another row shape — is rejected with guidance instead of silently
    /// matching nothing after a full-index scan.
    ///
    /// The accepted set is the row's own declaration, not `CORE_WHERE_FIELDS`.
    /// That list is a union across every result shape, so gating on it let
    /// `size`, `depth`, `extension`, `signature`, `marker` and `declaration`
    /// through to answer a confident zero on a row that carries none of them.
    /// [`reject_refused_fields`] runs first and gives those six, and
    /// `node_kind`, a message that says where they ARE answered.
    pub(super) fn reject_unknown_where_fields(&self, clauses: &Clauses) -> anyhow::Result<()> {
        for pred in &clauses.where_predicates {
            let field = crate::field_tiers::canonical(&pred.field);
            if resolves_on_symbol_row(field) {
                continue;
            }
            if crate::storage::legacy::is_known_enrichment_field(field) {
                continue;
            }
            if self.segments().iter().any(|s| s.has_extra_col(field)) {
                continue;
            }
            if self
                .dirty
                .added
                .iter()
                .any(|ds| ds.reader.has_extra_col(field))
            {
                continue;
            }
            let core = accepted_list(symbol_row_fields());
            anyhow::bail!(
                "unknown WHERE field '{field}': it is not a field a symbol row \
                 carries and no indexed row carries an enrichment column with \
                 that name, so it can never match.  Symbol-row fields: {core}, \
                 plus any enrichment field.  To search file contents use SHOW \
                 LINES OF '<file>' WHERE text MATCHES '…' on specific files \
                 instead of FIND symbols."
            );
        }
        Ok(())
    }

    /// Reject an `ORDER BY` on a field that can never sort a symbol row.
    ///
    /// A field orders symbols only if the row resolves it through either
    /// accessor, it is a known enrichment field, or it is a materialised extra
    /// column somewhere in this index.  Anything else (e.g. `size` / `depth`,
    /// which belong to `FIND files`) produces `None` for every row, so the
    /// comparator would silently fall back to name order and hand the agent
    /// alphabetical rows mislabelled as "top N by <field>".  Fail loudly
    /// instead — matching the legacy backend's `validate_order_by_field`
    /// contract.
    pub(super) fn reject_unknown_order_by_field(&self, clauses: &Clauses) -> anyhow::Result<()> {
        let Some(ref order) = clauses.order_by else {
            return Ok(());
        };
        let field = crate::field_tiers::canonical(&order.field);
        if resolves_on_symbol_row(field) {
            return Ok(());
        }
        if crate::storage::legacy::is_known_enrichment_field(field) {
            return Ok(());
        }
        if self.segments().iter().any(|s| s.has_extra_col(field)) {
            return Ok(());
        }
        if self
            .dirty
            .added
            .iter()
            .any(|ds| ds.reader.has_extra_col(field))
        {
            return Ok(());
        }
        // Listed from the declaration rather than restated, so the set an agent
        // is told about cannot drift from the set actually accepted.
        let sortable = accepted_list(symbol_row_fields());
        anyhow::bail!(
            "unknown ORDER BY field '{field}': it is not a field a symbol row \
             carries and no indexed row carries an enrichment column with that \
             name, so every symbol would tie and fall back to name order.  \
             Sortable fields: {sortable}, plus any enrichment field (lines, \
             param_count, branch_count, …).  'size' and 'depth' apply to FIND \
             files, not FIND symbols."
        );
    }

    /// Stage 0 — fail fast on a GROUP BY field that no row can resolve.
    ///
    /// `apply_group_by` keys a row it cannot resolve to the empty string, so
    /// an unknown field does not produce no groups: it produces exactly one,
    /// named `(empty)`, whose count is the entire result set. That reads like
    /// an answer. Grouping keys a row through `field_str` alone, so the
    /// accepted core set is exactly
    /// [`ClauseTarget::STR_FIELDS`](crate::filter::ClauseTarget::STR_FIELDS) —
    /// `line`, `usages` and `count` are numeric and are not groupable — plus a
    /// known enrichment field or an extra column stored by some segment.
    pub(super) fn reject_unknown_group_by_field(&self, clauses: &Clauses) -> anyhow::Result<()> {
        let Some(GroupBy::Field(ref field)) = clauses.group_by else {
            return Ok(());
        };
        let field = crate::field_tiers::canonical(field);
        if SymbolMatch::STR_FIELDS.contains(&field)
            || crate::storage::legacy::is_known_enrichment_field(field)
            || self.segments().iter().any(|s| s.has_extra_col(field))
            || self
                .dirty
                .added
                .iter()
                .any(|ds| ds.reader.has_extra_col(field))
        {
            return Ok(());
        }
        let groupable = accepted_list(SymbolMatch::STR_FIELDS.iter().copied());
        anyhow::bail!(
            "GROUP BY '{field}' cannot be answered: a symbol row does not \
             resolve that name, so every symbol would fall into one \
             empty-named group holding the whole result.  Group by \
             {groupable}, or by any enrichment field.  'line', 'usages' and \
             'count' are numeric and are not groupable; 'size', 'depth' and \
             'extension' belong to FIND files."
        );
    }

    /// Overwrite each row's `usages_count` with the workspace-total usage-site
    /// count from the overlay usages aggregate — on every row except one the
    /// aggregate cannot speak for, which is dropped to `None` here rather than
    /// stamped (see [`SymbolMatch::drop_meaningless_usage_count`]).
    ///
    /// The per-segment `usages_count` column is a legacy always-zero field;
    /// the overlay FST is the source of truth. One O(log n) FST lookup per
    /// materialised row, before LIMIT — bounded by the candidate set size.
    /// On a session with dirty rows the aggregate is the master's count, and
    /// each row's value is corrected for the sites the session shadowed and
    /// added — one hash lookup per row on top, over a table built once per
    /// dirty state (see the `usage_adjust` module).
    ///
    /// This is the ONE funnel every columnar route stamps through — the
    /// persistent-row page, the row-view page, the per-segment build, the
    /// order-by-name fast path, the dirty-overlay union, and the symbol
    /// lookup that serves the `SHOW` verbs — which is what makes the
    /// suppression above complete on this backend rather than merely applied
    /// in the places somebody remembered.
    pub(in super::super) fn stamp_usage_counts(&self, results: &mut [SymbolMatch]) {
        let adjust = self.usage_stamper();
        self.stamp_usage_counts_with(adjust.as_deref(), results);
    }

    /// The correction `stamp_usage_counts_with` needs on this session: `None`
    /// on a clean one, whose aggregate is already the answer. Fetched once
    /// per query rather than once per segment — the check that the cached
    /// table still matches the dirty overlay walks that overlay, and a scan
    /// stamps thousands of segments.
    pub(in super::super) fn usage_stamper(
        &self,
    ) -> Option<std::sync::Arc<super::super::usage_adjust::UsageAdjust>> {
        (!self.dirty.is_empty()).then(|| self.usage_adjust())
    }

    /// Stamp with a correction fetched by [`Self::usage_stamper`].
    pub(in super::super) fn stamp_usage_counts_with(
        &self,
        adjust: Option<&super::super::usage_adjust::UsageAdjust>,
        results: &mut [SymbolMatch],
    ) {
        for row in results.iter_mut() {
            let aggregate = self.overlay().usage_count(&row.name);
            row.usages_count = Some(adjust.map_or_else(
                || usize::try_from(aggregate).unwrap_or(usize::MAX),
                |a| a.corrected(&row.name, aggregate),
            ));
            // Stamped first and dropped after, deliberately: the rule is a
            // property of the finished row, so it is read from the row here
            // exactly as the in-memory backend reads it, and neither backend
            // can grow a private idea of which rows are excluded.
            row.drop_meaningless_usage_count();
        }
    }

    pub(super) fn find_usages_impl(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
        bound: Option<crate::storage::UsageBound>,
    ) -> anyhow::Result<crate::storage::UsagePage> {
        // BUG-006 U2: read the per-segment usage postings written at index
        // time (`usages_fst` / `usages_postings`, ENRICH_VER 23) instead of
        // the definitions name-FST, which only ever yielded definition rows.
        // Row shape matches the legacy backend's find_usages: name + path +
        // line, everything else empty — the agent interprets the sites.
        //
        // Beside them come the mention postings (`mentions_<role>_*`): the same
        // name written as prose rather than resolved as code. Every row carries
        // a `role` so the two are told apart and filtered separately. A usage
        // site is `code`, tagged here rather than stored, because a posting in
        // the usages blob can be nothing else.

        // A name written only in the identifier alphabet is matched token-exact.
        // A name carrying anything else — a path, a dotted name, a fragment — is
        // *also* matched as a substring of the stored tokens, so
        // `OF 'zephyr/pm/device.h'` reaches the include sites that wrote it and
        // `OF 'net/core/'` reaches all of them. The queried name is always among
        // the candidates, so the exact behaviour of every role is preserved and
        // substring reach is added on top.
        let names = if is_core_alphabet(name) {
            vec![name.to_owned()]
        } else {
            self.substring_names(name)
        };

        let mut sites: Vec<Site> = Vec::new();
        for token in &names {
            sites.extend(self.sites_of(token));
        }

        // The row budget, enforced on the sites while they are gathered — one
        // site becomes exactly one result row. Checked between tiers, so the
        // peak can overshoot by one tier's finds; never after a LIMIT, which
        // selects whole files out of the computed answer and bounds nothing
        // here.
        let max_rows = super::super::fast_paths::find_max_rows();
        if sites.len() > max_rows {
            return Err(super::super::fast_paths::usages_budget_exceeded(
                sites.len(),
                max_rows,
            ));
        }

        // Every tier above answers out of a posting, and a posting exists only
        // where some recorder tokenised the line. Lines exist that no recorder
        // tokenised at all — the body of an `.rst` literal block posts nothing,
        // so a name written inside one was invisible to all of them — and no
        // arrangement of posting-fed tiers can see a line none of them
        // recorded. Only the file itself can.
        //
        // So the files answer too, for every name rather than only for the ones
        // the token index cannot address. The line's own text decides what is a
        // site; a posting only ever adds a role to a site the bytes already
        // proved. That is what lets the contract say `complete` with no
        // qualifier after it.
        let (scanned, settled, hint) = self.literal_sites(name, clauses, root);

        // A posting is a claim about the bytes as they were when the file was
        // indexed, and a file can change without ForgeQL: a build step writes
        // it, a checkout replaces it, an editor saves it. Where the read did
        // examine the bytes, they are the authority and the scan already holds
        // every site they carry — so a posted site the scan did not confirm is
        // a segment that has drifted, and reporting it would hand back a line
        // that no longer holds the name, with a file handle and a rev that both
        // still resolve. Postings survive only for files the read could not
        // open or could not decode as text, where they are the only evidence
        // there is.
        {
            let confirmed: HashSet<(&std::path::PathBuf, u32)> = scanned
                .iter()
                .map(|(path, line, _)| (path, *line))
                .collect();
            sites.retain(|(path, line, _)| {
                !settled.contains(path) || confirmed.contains(&(path, *line))
            });
        }

        // A site a posting already reported keeps that posting's role, which
        // says more than `text` does. Matched on `(path, line)` and not on the
        // whole tuple: the same occurrence under two names for what it is would
        // be two rows for one site.
        let fresh: Vec<Site> = {
            let posted: HashSet<(&std::path::PathBuf, u32)> =
                sites.iter().map(|(path, line, _)| (path, *line)).collect();
            scanned
                .into_iter()
                .filter(|(path, line, _)| !posted.contains(&(path, *line)))
                .collect()
        };
        sites.extend(fresh);

        // The same bound again, now that the file-reading tier has merged in
        // everything the postings could not see.
        if sites.len() > max_rows {
            return Err(super::super::fast_paths::usages_budget_exceeded(
                sites.len(),
                max_rows,
            ));
        }

        // The page is cut from the sites, so a site outside it is never built.
        // Every one of them was still read, matched and counted: `LIMIT` bounds
        // what is delivered and never what is searched.
        let views: Vec<crate::storage::SiteView<'_>> = sites
            .iter()
            .map(|(path, line, role)| crate::storage::SiteView {
                name,
                path,
                line: usize::try_from(*line).unwrap_or(usize::MAX),
                role: Some(role),
            })
            .collect();
        Ok(crate::storage::usage_page_from_sites(
            views, clauses, bound, hint,
        ))
    }

    /// Every stored usage token containing `needle`, plus `needle` itself.
    ///
    /// Only reached when `needle` carries a character outside `[A-Za-z0-9_]`,
    /// and the dictionary holds only tokens that do — in practice the
    /// whole-text tokens, such as include paths. The two halves are the same
    /// test on purpose: a token containing `needle` must contain every
    /// character `needle` does, so restricting the dictionary that way loses
    /// nothing reachable while keeping it to a few thousand paths instead of
    /// every identifier in the workspace.
    ///
    /// Superset-then-verify: the trigram tier proposes candidates and each is
    /// confirmed with a real `contains` before it is searched for.
    /// `Some(empty)` means "definitively no match".
    ///
    /// The tier is ASCII case-insensitive, the verify is not: substring
    /// matching is case-sensitive, like the exact lookup it extends.
    fn substring_names(&self, needle: &str) -> Vec<String> {
        let mut names = vec![needle.to_owned()];

        // Below the trigram width the tier cannot narrow anything, and every
        // token trivially contains a shorter-than-3 needle (an empty one most
        // of all). Bail before building the dictionary rather than after: the
        // answer would be this same one line, and the build is the whole cost.
        if needle.len() < TRIGRAM_WIDTH {
            return names;
        }

        let index = self.substring_index.get_or_init(|| {
            let mut tokens = self
                .overlay()
                .usage_tokens_where(|token| !is_core_alphabet(token));
            tokens.sort_unstable();
            tokens.dedup();
            let mut trigrams = crate::ast::trigram::TrigramIndex::default();
            for (row, token) in tokens.iter().enumerate() {
                trigrams.insert(row, token);
            }
            SubstringIndex { tokens, trigrams }
        });

        let Some(rows) = index.trigrams.candidates(needle) else {
            return names;
        };

        let mut keep = |token: &str| {
            if token != needle && token.contains(needle) {
                names.push(token.to_owned());
            }
        };
        for row in rows {
            if let Some(token) = index.tokens.get(row) {
                keep(token);
            }
        }

        // The dirty overlay is not in the cached dictionary — it changes as
        // files are edited — and holds only the handful of segments reindexed
        // this session, so scan it directly.
        for ds in &self.dirty.added {
            for token in ds.reader.usage_tokens_where(|t| !is_core_alphabet(t)) {
                keep(&token);
            }
        }

        names.sort_unstable();
        names.dedup();
        names
    }

    /// Every site of `token` across the workspace, each tagged with its role.
    ///
    /// `code` for a posting in the usages blob — a posting there can be nothing
    /// else — and its stored role for a mention.
    fn sites_of(&self, token: &str) -> Vec<Site> {
        // Named through `field_tiers` so the role this pass mints and the set a
        // `WHERE role = …` is refused against cannot drift apart: the universe
        // is built from these constants, so renaming one moves both ends.
        const ROLE_CODE: &str = crate::field_tiers::ROLE_CODE;

        let mut sites = Vec::new();
        for (idx, meta) in self.overlay().segments().iter().enumerate() {
            if self.dirty.shadows(&meta.source_path) {
                continue;
            }
            let Some(seg) = self.segments().get(idx) else {
                continue;
            };
            for line in seg.lookup_usage_lines(token) {
                sites.push((meta.source_path.clone(), line, ROLE_CODE.to_owned()));
            }
            for (role, line) in seg.lookup_mention_sites(token) {
                sites.push((meta.source_path.clone(), line, role.to_owned()));
            }
        }
        for ds in &self.dirty.added {
            for line in ds.reader.lookup_usage_lines(token) {
                sites.push((ds.source_path.clone(), line, ROLE_CODE.to_owned()));
            }
            for (role, line) in ds.reader.lookup_mention_sites(token) {
                sites.push((ds.source_path.clone(), line, role.to_owned()));
            }
        }
        sites
    }

    /// Every site of `needle`, read out of the files themselves.
    ///
    /// The authoritative tier, and the reason `FIND usages` can promise a
    /// complete answer. Every other tier answers out of a posting, and a
    /// posting exists only where some recorder tokenised the line — so each of
    /// them is bounded by what the index happens to hold, in two ways that
    /// compound:
    ///
    /// - a name no recorder stored *whole* is unreachable by name, which is the
    ///   ordinary case for a dotted or slashed one outside C: a mention
    ///   recorder continues a token only over the characters its language
    ///   declares, so `foo-bar.frozen` is stored as `foo-bar` and `frozen`;
    /// - and a *line* no recorder tokenised at all is unreachable by anything.
    ///   The body of an `.rst` literal block posts nothing, so a name written
    ///   inside one answered zero however plainly the file contained it.
    ///
    /// Neither can be fixed by another posting-fed tier, and both vanish if the
    /// files are simply read. The line's text is the authority; the postings
    /// are consulted only to *label* what the bytes already found, so that a
    /// site a recorder did see keeps the role it recorded rather than being
    /// flattened to `text`. Every piece of the name contributes to that map,
    /// not the cheapest one: which piece a site stored varies by language, so
    /// the cheapest is routinely stored nowhere the name appears.
    ///
    /// The files read are every file the workspace knows about — the ones that
    /// produced symbols, the ones the index tracks by path and size alone, and
    /// anything added in this session — which is the same set `FIND files`
    /// lists, minus ForgeQL's own runtime artifacts. A `.gitignore` or an
    /// extension no plugin claims holds text like any other file, and skipping
    /// it would answer a confident zero for a name written there. Binary — a
    /// NUL byte near the start, the line `grep` draws — is not searched, since
    /// an object file or an index blob embeds symbol names it would be wrong to
    /// hand to a sweep. Everything else decodes leniently, so a file that is
    /// text apart from a stray legacy byte still answers on every line that
    /// holds the name.
    ///
    /// Cost is one read per in-scope file per query, bounded by `IN`/`EXCLUDE`.
    /// Reads nothing stored in a new way, so no cache version moves.
    /// Read the in-scope files and return every site their bytes hold, the set
    /// of paths whose site set is now settled, and a hint counting what could
    /// not be read.
    ///
    /// A path is settled when its bytes were decoded — every site they hold is
    /// in the first value — or when the file is not there, which settles it at
    /// none. The second value is what lets the caller tell "these bytes do not
    /// hold the name" from "these bytes were never looked at", and so which
    /// postings it may drop.
    fn literal_sites(
        &self,
        needle: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> (Vec<Site>, HashSet<std::path::PathBuf>, Option<String>) {
        const ROLE_TEXT: &str = crate::field_tiers::ROLE_TEXT;

        // What the postings can say about the lines this is about to read:
        // which of them a recorder tokenised, and as what. It is a lookup, not
        // a proposal — a scanned line the map does not mention is still a site,
        // it simply has no role beyond `text`.
        //
        // Every piece contributes, not the cheapest one. Which piece a site
        // stored depends on the extra characters that site's language continues
        // a token over, and that varies per language and per file:
        // `foo-bar.frozen` is stored as `foo-bar` + `frozen` where `-`
        // continues a token and as `foo` + `bar` + `frozen` where it does not.
        //
        // A name written only in the identifier alphabet needs no map at all:
        // the exact tier above already reported every posted site of it, with
        // the role the recorder gave it.
        let mut roles: HashMap<(std::path::PathBuf, u32), String> = HashMap::new();
        if !is_core_alphabet(needle) {
            for piece in needle_pieces(needle) {
                for (path, line, role) in self.sites_of(piece) {
                    let _ = roles.entry((path, line)).or_insert(role);
                }
            }
        }

        // Every file the workspace knows about, not only the ones that produced
        // symbols. Four sources, and a name can live in any of them: a
        // committed segment; a file the index tracks by path and size alone (a
        // `.gitignore`, an extension no plugin claims); a file this session
        // reindexed; and a file this session created whose extension no plugin
        // claims — that last one is in no committed structure at all until the
        // next commit, so the dirty overlay records its path and nothing else.
        // `FIND files` lists exactly these four, minus ForgeQL's own runtime
        // artifacts; the search universe is the same set, for the same reason.
        //
        // A file outside the query's own `IN`/`EXCLUDE` scope can only produce
        // rows the clause pipeline will drop, so it is never read.
        let mut paths: Vec<std::path::PathBuf> = self
            .overlay()
            .segments()
            .iter()
            .filter(|meta| !self.dirty.shadows(&meta.source_path))
            .map(|meta| meta.source_path.clone())
            .chain(
                self.overlay()
                    .file_entries()
                    .iter()
                    .map(|(path, _)| path.clone())
                    .filter(|path| !crate::result::FileEntry::is_runtime_artifact(path)),
            )
            .chain(self.dirty.added.iter().map(|ds| ds.source_path.clone()))
            .chain(
                self.dirty
                    .added_paths
                    .iter()
                    .filter(|path| !crate::result::FileEntry::is_runtime_artifact(path))
                    .cloned(),
            )
            .filter(|path| in_scope(path, clauses))
            .collect();
        paths.sort_unstable();
        paths.dedup();

        // An identifier is matched as a whole token, the way the exact tier
        // matches it and the way `MATCHING WORD` rewrites it — `256` must not
        // answer inside `sha256`. Anything carrying a character outside that
        // alphabet is matched literally, which is what lets an include path or
        // a dotted name be asked for at all.
        let whole_token = is_core_alphabet(needle);

        let mut sites = Vec::new();
        let mut unread = 0usize;
        // Paths whose site set is now known: the bytes were decoded and every
        // site they hold is in `sites`, or the file is not there at all and
        // holds none. Both settle the question, and both let the caller drop a
        // posting the bytes do not back. A file that could not be opened for
        // any other reason, or could not be decoded as text, settles nothing.
        let mut settled: HashSet<std::path::PathBuf> = HashSet::new();
        for path in &paths {
            let bytes = match std::fs::read(root.join(path)) {
                Ok(bytes) => bytes,
                // A path the index still lists but the worktree no longer has:
                // deleted in this session, or removed behind ForgeQL's back by
                // a build step or a checkout. There are no bytes, so there are
                // no sites, and the answer over it is complete rather than
                // short — which is exactly why it is marked settled below
                // rather than skipped. A committed segment for it may still
                // hold postings, and leaving them unsettled would let a file
                // that is not there report `code` sites with its handle on
                // them.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = settled.insert(path.clone());
                    continue;
                }
                // Anything else — a permission, an I/O fault — leaves the sites
                // this file holds absent from the answer, and nothing else in
                // the response would show that.
                Err(_) => {
                    unread += 1;
                    continue;
                }
            };
            // Bytes that are not text hold no sites. `decode_text` draws that
            // line and applies the file's own declared encoding; see its doc.
            let Some(text) = decode_text(&bytes) else {
                continue;
            };
            // Recorded only once the bytes have actually been decoded as text.
            // A file that was opened and then skipped as binary proves nothing
            // about what it holds, so it must not be counted as examined.
            let _ = settled.insert(path.clone());

            // Cheap reject before splitting into lines: a whole-token match is
            // a substring match too.
            if !text.contains(needle) {
                continue;
            }
            for (i, line) in text.lines().enumerate() {
                if !holds(line, needle, whole_token) {
                    continue;
                }
                let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
                let role = roles
                    .get(&(path.clone(), line_no))
                    .cloned()
                    .unwrap_or_else(|| ROLE_TEXT.to_owned());
                sites.push((path.clone(), line_no, role));
            }
        }

        (sites, settled, unread_hint(unread, needle))
    }

    pub(super) fn indexed_files_impl(&self) -> Vec<crate::result::FileEntry> {
        let segs = self.overlay().segments();
        let file_only = self.overlay().file_entries();
        let mut entries = Vec::with_capacity(
            segs.len()
                .saturating_add(file_only.len())
                .saturating_add(self.dirty.added.len())
                .saturating_add(self.dirty.added_paths.len()),
        );

        // Base: persistent overlay segments with mmap-cached sizes.
        // Skip any segment shadowed (replaced or deleted) by the dirty overlay.
        for (idx, seg) in segs.iter().enumerate() {
            if self.dirty.shadows(&seg.source_path) {
                continue;
            }
            let size = u64::from(self.overlay().file_size(idx));
            entries.push(plain_file_entry(&seg.source_path, size));
        }

        // File-only entries (FQOV v8+): non-indexed workspace files tracked
        // only for path + size.  A dirty segment never replaces one (segments
        // hold symbols), but a session can still DELETE the file — an entry
        // whose path sits in removed_paths is gone from the worktree and must
        // not be listed.
        for (rel_path, size) in file_only {
            // Session infrastructure, not source: the worktree gitfile pointer
            // and forgeql's own runtime artifacts (`.forgeql-session`, …).
            if crate::result::FileEntry::is_runtime_artifact(rel_path) {
                continue;
            }
            if self.dirty.removed_paths.contains(rel_path) {
                continue;
            }
            entries.push(plain_file_entry(rel_path, u64::from(*size)));
        }

        // Overlay: dirty segments (files changed in this session).
        // Read actual on-disk size — only 1 syscall per mutated file.
        for ds in &self.dirty.added {
            let size = self.on_disk_size(&ds.source_path);
            entries.push(plain_file_entry(&ds.source_path, size));
        }

        // Overlay: files this session touched whose extension no plugin claims,
        // so they produced no segment. One created in-session is in no
        // committed structure at all, so without this it is listed by nothing.
        for rel_path in &self.dirty.added_paths {
            if crate::result::FileEntry::is_runtime_artifact(rel_path) {
                continue;
            }
            if self.dirty.removed_paths.contains(rel_path) {
                continue;
            }
            let size = self.on_disk_size(rel_path);
            entries.push(plain_file_entry(rel_path, size));
        }

        dedupe_file_entries(&mut entries);
        entries
    }

    /// On-disk byte length of a workspace-relative path, `0` when it cannot be
    /// stat'd. One syscall.
    fn on_disk_size(&self, rel_path: &Path) -> u64 {
        self.worktree_root
            .join(rel_path)
            .metadata()
            .map_or(0, |m| m.len())
    }
}

impl ColumnarStorage {
    /// The name-stream fast-paths: `ORDER BY name [DESC] [WHERE fql_kind=...]
    /// LIMIT N`, and a bare `LIMIT N` with no ORDER BY at all, whose default
    /// ordering starts with `name` ascending — the order the stream serves.
    ///
    /// Each streams the first `limit + offset` rows directly from the name FST
    /// in lexicographic order, materialising only those rows. The ascending
    /// arms (bare LIMIT and `ORDER BY name ASC`) also serve a session with
    /// dirty rows: every dirty segment carries its own sorted name FST, so the
    /// overlay stream and the dirty streams merge name by name, overlay rows
    /// under a shadowed path are skipped, and the total becomes the masked
    /// stored counts plus the dirty segments' distinct counts. The descending
    /// and kind-filtered arms still decline on a dirty session — they have not
    /// learned the merge. All arms require unique source paths, because the
    /// honest `total` below sums stored per-segment counts that a duplicated
    /// path would double-count — on the dirty side the same holds for two
    /// dirty segments over one path. Returns `None` when no name-ordered
    /// fast-path applies, so the caller runs the pipeline.
    ///
    /// **A page these return is whole or they decline it.** Duplicates are
    /// collapsed after the stream, so a window holding two rows that are one
    /// row leaves the page short — and asking for more means the walk the
    /// stream exists to avoid. So a stream that stopped at `need` and then
    /// collapsed hands the query back to the pipeline instead, which reads
    /// every segment and collapses before it pages. A stream that returned
    /// fewer than `need` ran out of index rather than stopping, and its answer
    /// is whole however many of its rows collapsed.
    ///
    /// **The `total` beside the page is the answer's size, not the page's.**
    /// The stream reads `need` rows and stops, but the index already knows how
    /// many rows match: each segment's deduplicated row count is written at
    /// overlay build time, and the merged kind postings hold only canonical
    /// rows — so the whole-corpus total is the sum of the stored counts, and a
    /// kind-filtered total is its bitmap's cardinality, neither of which walks
    /// the FST. These paths were the last place a query's `total` was the size
    /// of its page; the golden case that pinned that as an expected failure is
    /// now enforced.
    fn try_order_by_name_fast_paths(&self, clauses: &Clauses) -> Option<FindPage> {
        // Dirty rows sit outside the overlay's name index, but every dirty
        // segment carries its own sorted name FST, so the ascending arms
        // merge them with the overlay stream and mask the shadowed paths.
        // The descending and kind-filtered arms have not learned the merge
        // yet and still decline on a dirty session.
        let dirty_active = !self.dirty.is_empty();
        if dirty_active {
            // Two dirty segments over one source path would double-count in
            // the merged total exactly like the committed twin case below.
            // Reindex keeps one segment per path; this guard keeps the total
            // honest if that invariant ever moves.
            let mut dirty_paths: std::collections::HashSet<&std::path::Path> =
                std::collections::HashSet::new();
            for ds in &self.dirty.added {
                if !dirty_paths.insert(ds.source_path.as_path()) {
                    return None;
                }
            }
        }
        // Two segments built from one source path can hold the same
        // (name, fql_kind, path, line) row, and the stored per-segment
        // deduplicated counts would count it twice. The honest total is not
        // derivable here, so every stream arm declines and the pipeline —
        // which collapses before it counts — answers instead. Same gate as
        // the count-based GROUP BY fast paths, for the same reason.
        if self.overlay().has_duplicate_paths() {
            return None;
        }
        let need = fast_path_need(clauses);
        // A stream materialises `need` rows bounded by nothing but the LIMIT
        // the caller wrote, so an oversized ask must not ride past the row
        // budget the pipeline enforces: decline, and let the scan either
        // complete under the budget or be refused by the error that names it.
        if need > find_max_rows() {
            return None;
        }
        let (mut results, matched) =
            if bare_limit_name_fast_path(clauses) || order_by_name_fast_path(clauses) {
                if dirty_active {
                    let shadowed = &self.dirty.removed_paths;
                    let masked_total: usize = self
                        .overlay()
                        .segments()
                        .iter()
                        .filter(|m| !shadowed.contains(&m.source_path))
                        .map(|m| m.dedup_row_count as usize)
                        .sum();
                    let mut rows = self.overlay().stream_names_asc_merged(
                        need,
                        self.segments(),
                        shadowed,
                        &self.dirty.added,
                    );
                    // The pipeline stamps workspace usage totals onto every
                    // row it serves for a dirty session; the merged stream
                    // serves the same rows, so it stamps the same way.
                    self.stamp_usage_counts(&mut rows);
                    (rows, masked_total + self.dirty_distinct_total())
                } else {
                    (
                        self.overlay().stream_names_asc(need, self.segments()),
                        self.overlay().dedup_total(),
                    )
                }
            } else if order_by_name_desc_fast_path(clauses) {
                if dirty_active {
                    return None;
                }
                (
                    self.overlay().stream_names_desc(need, self.segments()),
                    self.overlay().dedup_total(),
                )
            } else if let Some(kind) = order_by_name_kind_fast_path(clauses) {
                if dirty_active {
                    return None;
                }
                let kind_bm = self.overlay().prefilter_kind(kind)?;
                let matched = usize::try_from(kind_bm.len()).unwrap_or(usize::MAX);
                (
                    self.overlay()
                        .stream_names_asc_kind_filtered(need, &kind_bm, self.segments()),
                    matched,
                )
            } else if let Some(kind) = order_by_name_kind_desc_fast_path(clauses) {
                if dirty_active {
                    return None;
                }
                let kind_bm = self.overlay().prefilter_kind(kind)?;
                let matched = usize::try_from(kind_bm.len()).unwrap_or(usize::MAX);
                (
                    self.overlay()
                        .stream_names_desc_kind_filtered(need, &kind_bm, self.segments()),
                    matched,
                )
            } else {
                return None;
            };
        // Collapse before deciding whether the page is whole. A stream stops
        // after `need` rows, so if collapsing those leaves fewer, the rows the
        // page is now short of are still unread — and this path cannot ask for
        // more without the walk it exists to avoid. Hand the query back to the
        // pipeline, which reads every segment and collapses before it pages.
        // A stream that returned fewer than `need` ran out of index instead,
        // so its answer is whole however many of them collapsed.
        let streamed = results.len();
        dedupe_symbol_matches(&mut results);
        if results.len() < streamed && streamed >= need {
            return None;
        }
        // The page is cut from the streamed superset; the total is the stored
        // deduplicated count of everything that matched, which the stream
        // never had to read — see the note above this function.
        crate::filter::apply_clauses(&mut results, clauses);
        Some(FindPage::of(results, matched))
    }

    /// The number of distinct answer rows the dirty segments contribute:
    /// distinct `(name, fql_kind, line)` per segment, summed. Paths are
    /// unique across dirty segments (guarded where the merge is armed) and
    /// every dirty path is masked out of the overlay side, so this sum joins
    /// the masked stored counts without double-counting any row.
    fn dirty_distinct_total(&self) -> usize {
        let mut total = 0usize;
        for ds in &self.dirty.added {
            let all: RoaringBitmap = (0..ds.reader.row_count).collect();
            let rows = ds.reader.materialize_rows(&all, Some(&ds.source_path));
            let mut seen: std::collections::HashSet<(String, Option<String>, Option<usize>)> =
                std::collections::HashSet::new();
            for r in rows {
                let _ = seen.insert((r.name, r.fql_kind, r.line));
            }
            total += seen.len();
        }
        total
    }

    /// Build the initial `segment index -> local row bitmap` candidate map.
    ///
    /// Fast path (a path filter is present but no indexed predicate is
    /// available): seed every path-matching segment with all its rows, skipping
    /// the global prefilter and per-segment grouping. Normal path: global
    /// prefilter, group by segment, then IN / EXCLUDE path prune.
    fn build_candidate_segments(&self, clauses: &Clauses) -> HashMap<u32, RoaringBitmap> {
        let has_path_filter = clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty();
        if has_path_filter && !has_any_indexed_predicate(clauses, self.overlay()) {
            let mut map: HashMap<u32, RoaringBitmap> = HashMap::new();
            for (idx, meta) in self.overlay().segments().iter().enumerate() {
                if passes_resolve_glob(&meta.source_path, clauses)
                    && let (Some(seg), Ok(seg_idx)) = (self.segments().get(idx), u32::try_from(idx))
                {
                    let _ = map.insert(seg_idx, (0..seg.row_count).collect());
                }
            }
            map
        } else {
            // Phase 6 — build path_floor before prefilter_global so it can serve
            // as the baseline universe: when no indexed predicate matches it is
            // returned directly; when one does, the result is already intersected
            // with the path range.
            let path_floor = clauses
                .in_glob
                .as_deref()
                .and_then(glob_to_path_prefix)
                .map(|prefix| {
                    let row_range = self.overlay().path_row_range(prefix);
                    row_range.collect::<RoaringBitmap>()
                });
            let candidates = self.prefilter_global(clauses, path_floor);
            let mut map = self.group_by_segment(&candidates);
            if let Some(allowed) = self.segments_passing_path_filter(clauses) {
                map.retain(|seg_idx, _| allowed.contains(seg_idx));
            }
            map
        }
    }

    /// Prune candidate segments that cannot contribute rows.
    ///
    /// Stage 2d drops persistent segments shadowed by the dirty overlay (a file
    /// changed or deleted this session keeps only its dirty version). Stage 2c
    /// drops segments whose zone maps rule out a numeric WHERE predicate
    /// (`line > N`, `usages >= N`, ...). Both steps are additive: segments
    /// lacking the relevant metadata are always kept.
    fn prune_candidate_segments(
        &self,
        by_segment: &mut HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
    ) {
        if !self.dirty.is_empty() {
            by_segment.retain(|&seg_idx, _| {
                self.overlay()
                    .segments()
                    .get(seg_idx as usize)
                    .is_none_or(|meta| !self.dirty.shadows(&meta.source_path))
            });
        }
        for pred in &clauses.where_predicates {
            if let PredicateValue::Number(val_i64) = &pred.value {
                let col = pred.field.as_str();
                // BUG-006 U3: `usages` is stamped at query time from the
                // overlay usages aggregate; the per-segment `usages_count`
                // zone map is a stale all-zeros column and must NOT prune
                // candidates. All other numeric columns keep zone-map pruning.
                if col == "usages" || col == "usages_count" {
                    continue;
                }
                // Impossible-predicate short-circuit for u32 columns: no stored
                // value satisfies col < 0, col <= negative, or col = negative.
                let impossible = ZONEMAP_NUMERIC_FIELDS.iter().any(|(f, _)| *f == col)
                    && match pred.op {
                        CompareOp::Lt => *val_i64 <= 0,
                        CompareOp::Lte | CompareOp::Eq => *val_i64 < 0,
                        _ => false,
                    };
                if impossible {
                    by_segment.clear();
                    return;
                }
                if let Ok(val_u32) = u32::try_from(*val_i64)
                    && let Some(allowed) = self.segments_passing_zone_map(col, pred.op, val_u32)
                {
                    by_segment.retain(|seg_idx, _| allowed.contains(seg_idx));
                }
            }
        }
    }
}

/// Rows to stream from an ordered fast-path: `limit + offset`, at least 1.
fn fast_path_need(clauses: &Clauses) -> usize {
    clauses
        .limit
        .unwrap_or(0)
        .saturating_add(clauses.offset.unwrap_or(0))
        .max(1)
}

/// Is every byte of `text` inside `[A-Za-z0-9_]`?
///
/// One predicate decides both halves of the matching rule for
/// `FIND usages OF '<name>'`, which is what makes the rule symmetric: a name
/// written only in this alphabet is matched token-exact, and the substring
/// dictionary holds only tokens that are *not*.
///
/// That pairing is what makes the search complete. A token containing the query
/// must contain every character the query does, so once the query carries a
/// character from outside this alphabet, every token that could match carries
/// one too — and the dictionary stays the whole-text tokens (the include paths)
/// instead of every identifier in the workspace.
///
/// The alphabet is deliberately the *core* one, not a language's widened token
/// alphabet (`mention_token_extra_chars`). A name a plugin has widened into one
/// token — `ubuntu-latest`, say — therefore takes the substring path, where the
/// exact lookup still finds it and the widening adds nothing. Deriving the test
/// per language would make the routing depend on which languages happen to be
/// registered.
fn is_core_alphabet(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Shortest identifier run worth proposing a candidate line from.
///
/// A recorder drops a one-character token, so anything shorter is never in the
/// index and could only ever propose nothing.
const MIN_PIECE_LEN: usize = 2;

/// How much of a file is inspected before calling it binary.
///
/// The rule `grep` and `git` use: a NUL byte in the first few kilobytes means
/// these bytes are not text. Reading further buys nothing — a file that is text
/// for 8 kB and binary afterwards does not exist in practice, and the check
/// runs once per file per query. A file that declared UTF-16 with a byte-order
/// mark never reaches this check — see [`decode_text`].
const BINARY_SNIFF: usize = 8000;

/// A single occurrence: the file it sits in, its 1-based line, and the role it
/// was recorded under — `code` for a resolved identifier, the stored mention
/// role (`comment`, `string`, `config`, `doc`) for a written one, and `text`
/// for a site found by reading the file rather than by a posting.
type Site = (std::path::PathBuf, u32, String);

/// The identifier runs inside `needle` that a recorder could have stored.
///
/// A token opens on a letter or `_` and continues over `[A-Za-z0-9_]` plus the
/// extra characters its language declares, so every stored token is built out
/// of runs like these. A needle spanning a separator that no language continues
/// over is therefore never a token itself while its runs are — which is exactly
/// what makes them usable as a proposal. Runs that cannot open a token (a
/// leading digit) are dropped: they are not stored, so they would propose an
/// empty candidate set and make a real match look like an absence.
fn needle_pieces(needle: &str) -> Vec<&str> {
    needle
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|piece| {
            piece.len() >= MIN_PIECE_LEN
                && piece
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        })
        .collect()
}

/// Does `line` hold `needle` — as a whole token when `whole_token` is set?
///
/// The arbiter of the whole search, and the reason reading the files can be
/// trusted over anything the index says. Two modes, because the two shapes of
/// name mean different things:
///
/// - **A name in the identifier alphabet is a token.** `FIND usages OF '256'`
///   asks about the identifier `256`, not about the digits inside `sha256`, and
///   the exact tier has always answered it that way. This is the same boundary
///   `MATCHING WORD` rewrites on, so a `FIND` and the sweep it arms agree about
///   what counts as an occurrence.
/// - **A name carrying anything else is matched literally**, separators and
///   all. That is what makes an include path or a dotted name askable, and it
///   is why a line holding every piece of `foo-bar.frozen` scattered is not a
///   site.
fn holds(line: &str, needle: &str, whole_token: bool) -> bool {
    if !whole_token {
        return line.contains(needle);
    }
    // A token ends at anything that is not a letter, digit or underscore, in
    // any script — `is_alphanumeric` is Unicode-aware, so `k_sleep` inside
    // `ék_sleep` is not a site any more than inside `my_k_sleep`. An ASCII-only
    // test would call `é` a separator and report that line, and `MATCHING WORD`
    // — which rewrites on the `regex` crate's `\b` — would then decline to
    // rewrite a site the query had listed.
    //
    // The two agree on every alphabet a name is written in, not on every code
    // point: `regex`'s `\w` and `is_alphanumeric` part company over combining
    // marks, non-decimal numerals and connector punctuation other than `_`. A
    // name built from those would be listed here and declined by the sweep, or
    // the reverse. No site is lost either way — the disagreement is about
    // where a token ends, not about which files were read.
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(needle).any(|(start, _)| {
        let before = line[..start].chars().next_back();
        let after = line[start + needle.len()..].chars().next();
        before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c))
    })
}

/// Decode a workspace file for searching, or `None` when these bytes are not
/// text and hold no sites.
///
/// A byte-order mark is the file declaring its own encoding, so it is believed
/// before anything is guessed. That is what makes UTF-16 searchable at all:
/// every ASCII character in it carries a NUL byte, so the sniff below — the
/// line `grep` and `git` draw — would otherwise call a plain UTF-16 document a
/// compiled object and answer zero over lines it plainly holds.
///
/// Without a mark there is nothing separating UTF-16 from an object file, and
/// the sniff decides. BOM-less UTF-16 is therefore not searched; that is a
/// boundary, not a finding about the file. Refusing to guess is deliberate:
/// treating NUL-heavy bytes as text would put a compiled object's embedded
/// symbol names into a `FOUND` set and arm a sweep on bytes no editor should
/// rewrite.
fn decode_text(bytes: &[u8]) -> Option<Cow<'_, str>> {
    match bytes {
        // UTF-32 declares itself with a UTF-16 mark and two more NULs. Nothing
        // here decodes it, and reading it as UTF-16 would invent text.
        [0xFF, 0xFE, 0x00, 0x00, ..] | [0x00, 0x00, 0xFE, 0xFF, ..] => None,
        [0xFF, 0xFE, rest @ ..] => Some(Cow::Owned(decode_utf16(rest, u16::from_le_bytes))),
        [0xFE, 0xFF, rest @ ..] => Some(Cow::Owned(decode_utf16(rest, u16::from_be_bytes))),
        // A UTF-8 mark is dropped rather than kept, so a name at the very start
        // of the first line is not preceded by a stray U+FEFF.
        [0xEF, 0xBB, 0xBF, rest @ ..] => Some(String::from_utf8_lossy(rest)),
        _ if bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0) => None,
        // Everything else is decoded leniently rather than strictly. A file
        // that is text apart from one byte in a legacy encoding is still text,
        // and every other line in it can hold the name verbatim; rejecting the
        // whole file for that byte would answer a confident zero over bytes
        // that do contain it, and silently, since the file read fine. Valid
        // UTF-8 — every source file — is borrowed here, not copied.
        _ => Some(String::from_utf8_lossy(bytes)),
    }
}

/// Decode UTF-16 code units of one endianness.
///
/// A trailing odd byte and an unpaired surrogate both become U+FFFD instead of
/// ending the read: one malformed unit must not stop the rest of the file from
/// answering, for the same reason the lossy UTF-8 path exists.
fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks_exact(2).map(|pair| unit([pair[0], pair[1]]));
    let mut out: String = char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !bytes.len().is_multiple_of(2) {
        out.push(char::REPLACEMENT_CHARACTER);
    }
    out
}

/// What to say when a candidate file could not be read.
///
/// Whatever sites it holds are missing from the answer, and nothing else in the
/// response would show that — so the count is stated rather than swallowed.
fn unread_hint(unread: usize, needle: &str) -> Option<String> {
    (unread > 0).then(|| {
        format!(
            "{unread} candidate file(s) for '{needle}' could not be read, so any site they \
             hold is missing from this answer"
        )
    })
}

/// Is `path` inside what the query's own `IN` / `EXCLUDE` globs select?
///
/// The clause pipeline drops everything outside them anyway, so a candidate
/// outside is a file read to produce a row that will not be returned. Pruning
/// here is free in rows and saves the read.
///
/// It must stay `ast::query::glob_matches` — the very function the pipeline
/// applies later, whose `normalize_glob` expands a bare directory into
/// `dir/**`. A look-alike predicate that skips that expansion filters out
/// candidates the pipeline would have kept, and the query answers a confident
/// zero: the exact failure this tier exists to remove, reintroduced in the
/// name of speed.
fn in_scope(path: &Path, clauses: &Clauses) -> bool {
    clauses
        .in_glob
        .as_ref()
        .is_none_or(|glob| crate::ast::query::glob_matches(path, glob))
        && !clauses
            .exclude_globs
            .iter()
            .any(|glob| crate::ast::query::glob_matches(path, glob))
}

/// Deduplicate symbol results on `(name, fql_kind, path, line)`.
///
/// The legacy backend deduplicates on the same identity —
/// `(name_id, path_id, fql_kind_id, line)` — so the two backends agree on what
/// a duplicate is, and the `(name, line, path, fql_kind)` tie-break is total
/// on the rows either backend retains.
///
/// The key is hashed rather than cloned. An owned-key set copied `name`,
/// `fql_kind` and a whole `PathBuf` alongside `line` for every candidate row,
/// and held all four for the whole deduplication pass, on top of the results
/// vector it was scanning. It now stores a 64-bit hash of those four fields
/// instead, and confirms every hash hit against the fields themselves, so the
/// output is exactly what the owned-key set produced: a collision costs a
/// field comparison against each row already kept under that hash, never a
/// wrong answer in either direction.
pub(in crate::storage::columnar::columnar_storage) fn dedupe_symbol_matches(
    results: &mut Vec<SymbolMatch>,
) {
    fn key_hash(r: &SymbolMatch) -> u64 {
        use std::hash::{Hash as _, Hasher as _};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        r.name.hash(&mut hasher);
        r.fql_kind.hash(&mut hasher);
        r.path.hash(&mut hasher);
        r.line.hash(&mut hasher);
        hasher.finish()
    }

    dedupe_with(results, key_hash);
}

/// The deduplication itself, over a caller-supplied key hash.
///
/// Split out only so a test can drive it with a hash that collides on purpose.
/// No row can ever be dropped by a collision -- `same_key` gates every drop and
/// compares all four fields exactly -- so what the `collided` list protects is
/// the other direction: without it, a duplicate whose hash collides with an
/// earlier row carrying a different key is not recognised as already kept, and
/// is emitted twice. That branch is unreachable from real data, so without a
/// way to force a collision it would be pinned by nothing.
fn dedupe_with(results: &mut Vec<SymbolMatch>, hash_of: impl Fn(&SymbolMatch) -> u64) {
    use std::collections::hash_map::Entry;

    fn same_key(a: &SymbolMatch, b: &SymbolMatch) -> bool {
        a.line == b.line && a.name == b.name && a.fql_kind == b.fql_kind && a.path == b.path
    }

    // `first` maps a key hash to the first row kept under it. `collided` holds
    // the rare row whose hash matched an earlier one carrying a different key;
    // it stays empty unless a collision actually happens, and it is what stops
    // a later duplicate of that row from being emitted a second time.
    let mut first: HashMap<u64, usize> = HashMap::with_capacity(results.len());
    let mut collided: Vec<(u64, usize)> = Vec::new();
    let mut keep = vec![false; results.len()];

    for (i, row) in results.iter().enumerate() {
        let hash = hash_of(row);
        let already_kept = first
            .get(&hash)
            .and_then(|&j| results.get(j))
            .is_some_and(|prev| same_key(prev, row))
            || collided
                .iter()
                .any(|&(h, ci)| h == hash && results.get(ci).is_some_and(|p| same_key(p, row)));
        if already_kept {
            continue;
        }
        match first.entry(hash) {
            Entry::Vacant(slot) => {
                let _first_under_this_hash = slot.insert(i);
            }
            Entry::Occupied(_) => collided.push((hash, i)),
        }
        if let Some(slot) = keep.get_mut(i) {
            *slot = true;
        }
    }

    let mut keep_iter = keep.into_iter();
    results.retain(|_| keep_iter.next().unwrap_or(true));
}

/// A [`FileEntry`] carrying only what a path and a size can say: no symbol
/// count, no parse coverage, no handle.
///
/// [`FileEntry`]: crate::result::FileEntry
fn plain_file_entry(path: &Path, size: u64) -> crate::result::FileEntry {
    crate::result::FileEntry {
        path: path.to_path_buf(),
        extension: path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string(),
        size,
        depth: Some(path.components().count()),
        count: None,
        error_count: None,
        parse_coverage: None,
        node_id: None,
        rev: None,
    }
}

/// Deduplicate file entries on path, keeping the freshest occurrence.
///
/// The list is built persistent → file-only → dirty, so for a duplicated
/// path the later entry carries the newer size.  Duplicate paths can enter
/// the overlay through commit/promote turbulence (the symbol pipeline guards
/// its GROUP BY fast paths with `has_duplicate_paths` for the same reason);
/// without this pass every affected file lists twice in `FIND files`.
fn dedupe_file_entries(entries: &mut Vec<crate::result::FileEntry>) {
    let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
    entries.reverse();
    entries.retain(|e| seen.insert(e.path.clone()));
    entries.reverse();
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Clauses, decode_text, dedupe_file_entries, dedupe_symbol_matches, dedupe_with, holds,
        in_scope, needle_pieces,
    };
    use crate::result::{FileEntry, SymbolMatch};

    #[test]
    fn a_hash_collision_does_not_let_a_duplicate_through() {
        // Every row lands in one bucket, so every candidate is decided through
        // the collision path -- the branch real data cannot reach. The repeats
        // are `a`, which `first` holds, AND `b`, which lives only in `collided`:
        // without the `b` repeat the test passes whether or not `collided`
        // exists, because `first` alone catches the `a`. Delete either
        // `collided.push` or the `collided` scan and the trailing `b` is not
        // recognised as already kept, so it is emitted twice and this fails.
        let mut rows = vec![
            sym("a", "function", "src/x.rs", 1),
            sym("b", "function", "src/x.rs", 1),
            sym("c", "function", "src/x.rs", 1),
            sym("a", "function", "src/x.rs", 1),
            sym("b", "function", "src/x.rs", 1),
        ];
        dedupe_with(&mut rows, |_| 0);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    fn sym(name: &str, kind: &str, path: &str, line: usize) -> SymbolMatch {
        SymbolMatch {
            name: name.to_owned(),
            fql_kind: Some(kind.to_owned()),
            path: Some(std::path::PathBuf::from(path)),
            line: Some(line),
            ..SymbolMatch::default()
        }
    }

    #[test]
    fn an_exact_duplicate_collapses_to_its_first_occurrence() {
        let mut rows = vec![
            sym("a", "function", "src/x.rs", 1),
            sym("b", "function", "src/x.rs", 2),
            sym("a", "function", "src/x.rs", 1),
        ];
        dedupe_symbol_matches(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn two_rows_differing_in_any_single_key_field_both_survive() {
        // Each pair is checked twice. Under the real hash it proves the two
        // rows stay apart end to end. Under a forced collision it proves the
        // field list in `same_key` is what keeps them apart -- and only that
        // second pass can: with the real hash, deleting a field from `same_key`
        // still leaves the rows in different buckets, so `first` never compares
        // them and the pair survives anyway. Narrowing `key_hash` stays
        // unpinnable by output -- dropping a field from it only creates a
        // collision that `same_key` then resolves correctly. Widening it IS
        // pinned: add `language` to the hash and
        // `a_field_outside_the_key_does_not_keep_a_duplicate_alive` fails,
        // because its two rows then land in different buckets and both survive.
        let base = sym("a", "function", "src/x.rs", 1);
        for (field, other) in [
            ("name", sym("b", "function", "src/x.rs", 1)),
            ("fql_kind", sym("a", "struct", "src/x.rs", 1)),
            ("path", sym("a", "function", "src/y.rs", 1)),
            ("line", sym("a", "function", "src/x.rs", 2)),
        ] {
            let mut rows = vec![base.clone(), other.clone()];
            dedupe_symbol_matches(&mut rows);
            assert_eq!(
                rows.len(),
                2,
                "rows differing only in `{field}` were collapsed under the real hash"
            );

            let mut rows = vec![base.clone(), other];
            dedupe_with(&mut rows, |_| 0);
            assert_eq!(
                rows.len(),
                2,
                "`same_key` stopped separating rows that differ only in `{field}`"
            );
        }
    }

    #[test]
    fn a_field_outside_the_key_does_not_keep_a_duplicate_alive() {
        // `language` is not part of the key: rows alike in all four key fields
        // are one row however much else differs. Pins the key's membership from
        // the other side, so widening it silently is a failure too.
        let mut first = sym("a", "function", "src/x.rs", 1);
        let mut second = sym("a", "function", "src/x.rs", 1);
        first.language = Some("rust".to_owned());
        second.language = Some("cpp".to_owned());
        let mut rows = vec![first, second];
        dedupe_symbol_matches(&mut rows);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn an_absent_key_field_is_a_value_not_a_wildcard() {
        // `fql_kind`, `path` and `line` are Options. A row missing one must
        // match only another row missing the same one -- never every row.
        //
        // Checked twice for the same reason as the field-list test: under the
        // real hash the present-fields row lands in its own bucket, so a
        // `same_key` treating `None` as a wildcard is never consulted for it
        // and this passes regardless. Only the forced collision puts the
        // absent-field rows and the present-field row through `same_key`.
        let rows = || {
            vec![
                SymbolMatch {
                    name: "a".to_owned(),
                    ..SymbolMatch::default()
                },
                SymbolMatch {
                    name: "a".to_owned(),
                    ..SymbolMatch::default()
                },
                sym("a", "function", "src/x.rs", 1),
            ]
        };

        let mut real = rows();
        dedupe_symbol_matches(&mut real);
        assert_eq!(real.len(), 2, "the two all-absent rows did not collapse");

        let mut collided = rows();
        dedupe_with(&mut collided, |_| 0);
        assert_eq!(
            collided.len(),
            2,
            "`same_key` treated an absent field as a wildcard"
        );
    }

    #[test]
    fn needle_pieces_splits_at_every_non_identifier_character() {
        assert_eq!(
            needle_pieces("foo-bar.frozen"),
            vec!["foo", "bar", "frozen"]
        );
        assert_eq!(
            needle_pieces("zephyr/pm/device.h"),
            vec!["zephyr", "pm", "device"]
        );
        assert_eq!(needle_pieces("golden.json"), vec!["golden", "json"]);
    }

    #[test]
    fn needle_pieces_drops_runs_no_recorder_could_have_stored() {
        // One character is below the recorder's own floor, and a run opening on
        // a digit cannot start a token — both would propose an empty candidate
        // set, which is exactly the false absence this tier exists to prevent.
        assert!(needle_pieces("a.b").is_empty());
        assert_eq!(needle_pieces("9f.frozen"), vec!["frozen"]);
        assert_eq!(needle_pieces("_x9.2b"), vec!["_x9"]);
        assert!(needle_pieces("...").is_empty());
    }

    #[test]
    fn a_literal_needle_demands_its_exact_form_not_its_pieces() {
        // A line holding every piece of the name, separately, is the case that
        // must still be rejected.
        assert!(holds(
            r#"  "use": "foo-bar.frozen","#,
            "foo-bar.frozen",
            false
        ));
        assert!(!holds(
            r#"  "foo-bar" and "frozen" both appear, apart"#,
            "foo-bar.frozen",
            false
        ));
    }

    /// An identifier answers as a token, not as a run of characters.
    ///
    /// This is the half that keeps the universal scan from changing what
    /// `FIND usages` means: reading every file would otherwise make `256`
    /// answer inside `sha256`, turning a precise query into a substring search
    /// over the corpus. The boundary is the one `MATCHING WORD` uses, so the
    /// sweep a `FIND` arms rewrites exactly the sites the `FIND` listed.
    #[test]
    fn an_identifier_needle_matches_only_on_token_boundaries() {
        assert!(holds("    k_sleep(K_MSEC(10));", "k_sleep", true));
        assert!(holds("#define TIMEOUT 256", "256", true));

        assert!(!holds("    hash = sha256(buf);", "256", true));
        assert!(!holds("    k_sleep_forever();", "k_sleep", true));
        assert!(!holds("    my_k_sleep();", "k_sleep", true));

        // Punctuation, quotes and line ends are all boundaries.
        assert!(holds("k_sleep", "k_sleep", true));
        assert!(holds(r#"log("k_sleep")"#, "k_sleep", true));

        // A token boundary is not an ASCII question. A letter in any script
        // continues the token, so these are not sites — and the sweep the query
        // arms, which rewrites on a Unicode word boundary, would refuse them.
        assert!(!holds("    ék_sleep();", "k_sleep", true));
        assert!(!holds("    k_sleepé();", "k_sleep", true));
        assert!(!holds("    привет_k_sleep();", "k_sleep", true));
        // A non-letter neighbour still ends the token, whatever its script.
        assert!(holds("    «k_sleep»", "k_sleep", true));

        // The same haystack, asked literally, does not care about boundaries.
        assert!(holds("    hash = sha256(buf);", "256", false));
    }

    /// UTF-16 encodes every ASCII character with a NUL byte, so the sniff that
    /// keeps object files out would keep a plain document out with them. The
    /// mark is what tells the two apart.
    #[test]
    fn a_declared_utf16_document_is_text_and_an_undeclared_one_is_not() {
        let little_endian = |s: &str| {
            let mut out = vec![0xFF, 0xFE];
            for unit in s.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            out
        };
        let big_endian = |s: &str| {
            let mut out = vec![0xFE, 0xFF];
            for unit in s.encode_utf16() {
                out.extend_from_slice(&unit.to_be_bytes());
            }
            out
        };

        assert_eq!(
            decode_text(&little_endian("CONFIG_IDLE=y\n")).as_deref(),
            Some("CONFIG_IDLE=y\n"),
            "a little-endian mark is the file declaring its own encoding"
        );
        assert_eq!(
            decode_text(&big_endian("CONFIG_IDLE=y\n")).as_deref(),
            Some("CONFIG_IDLE=y\n"),
            "and so is a big-endian one"
        );

        // Same bytes, mark removed: nothing separates this from an object file.
        let mut undeclared = little_endian("CONFIG_IDLE=y\n");
        let _: Vec<u8> = undeclared.drain(..2).collect();
        assert!(
            decode_text(&undeclared).is_none(),
            "without a declaration the NUL bytes decide, and guessing would let \
             a compiled object arm a sweep"
        );

        // A UTF-16 mark followed by two NULs is UTF-32, which nothing here
        // decodes; reading it as UTF-16 would invent text.
        assert!(decode_text(&[0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00]).is_none());
    }

    /// The other side of the same line, and the two halves of the lenient
    /// decode that sits behind it.
    #[test]
    fn a_nul_means_binary_but_a_stray_legacy_byte_does_not() {
        let mut object = vec![0x00, 0x01, 0x02];
        object.extend_from_slice(b"k_sleep\n");
        assert!(
            decode_text(&object).is_none(),
            "an object file embeds the ASCII of its symbol names"
        );

        let mut legacy = b"// caf".to_vec();
        legacy.push(0xE9);
        legacy.extend_from_slice(b"\nk_sleep();\n");
        let text = decode_text(&legacy).expect("one legacy byte does not make a file binary");
        assert!(
            text.contains("k_sleep"),
            "the other lines are plain ASCII and must still answer: {text:?}"
        );

        // A UTF-8 mark is dropped, so a name at the very start of the file is
        // not preceded by a stray U+FEFF.
        let mut marked = vec![0xEF, 0xBB, 0xBF];
        marked.extend_from_slice(b"k_sleep\n");
        assert_eq!(decode_text(&marked).as_deref(), Some("k_sleep\n"));
    }

    /// The prune must expand a bare directory the way the clause pipeline does.
    ///
    /// This is the shape that already shipped a silent zero once: a predicate
    /// that treats `IN 'crates/forgeql-core'` as a literal path matches no file
    /// at all, so every candidate is discarded before it is read and the query
    /// reports a confident absence.
    #[test]
    fn in_scope_expands_a_bare_directory_into_its_subtree() {
        let clauses = Clauses {
            in_glob: Some("crates/forgeql-core".to_owned()),
            ..Clauses::default()
        };

        assert!(
            in_scope(Path::new("crates/forgeql-core/src/lib.rs"), &clauses),
            "a bare directory selects its subtree, exactly as `dir/**` would"
        );
        assert!(
            !in_scope(Path::new("crates/forgeql/src/main.rs"), &clauses),
            "and still excludes what is outside it"
        );
    }

    /// `EXCLUDE` removes a candidate the `IN` glob selected, same as downstream.
    #[test]
    fn in_scope_honours_exclude_over_include() {
        let clauses = Clauses {
            in_glob: Some("crates/**".to_owned()),
            exclude_globs: vec!["crates/forgeql-core/tests/**".to_owned()],
            ..Clauses::default()
        };

        assert!(in_scope(
            Path::new("crates/forgeql-core/src/lib.rs"),
            &clauses
        ));
        assert!(!in_scope(
            Path::new("crates/forgeql-core/tests/common/mod.rs"),
            &clauses
        ));
    }

    /// No scope clause means every file stays a candidate.
    #[test]
    fn in_scope_keeps_everything_when_the_query_named_no_scope() {
        assert!(in_scope(
            Path::new("anywhere/at/all.rs"),
            &Clauses::default()
        ));
    }

    fn entry(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.into(),
            extension: "rs".to_owned(),
            size,
            depth: None,
            count: None,
            error_count: None,
            parse_coverage: None,
            node_id: None,
            rev: None,
        }
    }

    #[test]
    fn dedupe_keeps_last_entry_per_path_and_preserves_order() {
        let mut entries = vec![
            entry("a.rs", 10),
            entry("b.rs", 20),
            entry("a.rs", 30),
            entry("c.rs", 40),
        ];
        dedupe_file_entries(&mut entries);
        let got: Vec<(String, u64)> = entries
            .iter()
            .map(|e| (e.path.display().to_string(), e.size))
            .collect();
        assert_eq!(
            got,
            vec![
                ("b.rs".to_owned(), 20),
                ("a.rs".to_owned(), 30),
                ("c.rs".to_owned(), 40)
            ]
        );
    }
}
