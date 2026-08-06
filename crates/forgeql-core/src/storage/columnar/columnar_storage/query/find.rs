//! `FIND symbols` / `FIND usages` / indexed-files queries for [`ColumnarStorage`].
use std::collections::{HashMap, HashSet};
use std::path::Path;

use roaring::RoaringBitmap;

use crate::ast::trigram::TRIGRAM_WIDTH;
use crate::filter::apply_clauses;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;
use crate::storage::columnar::columnar_storage::fast_paths::{
    glob_to_path_prefix, group_by_file_fast_path_eligible, group_by_kind_fast_path_eligible,
    has_any_indexed_predicate, order_by_name_desc_fast_path, order_by_name_fast_path,
    order_by_name_kind_desc_fast_path, order_by_name_kind_fast_path, passes_resolve_glob,
};
use crate::storage::columnar::columnar_storage::{ColumnarStorage, SubstringIndex};
use crate::storage::columnar::segment_builder::ZONEMAP_NUMERIC_FIELDS;

impl ColumnarStorage {
    pub(super) fn find_symbols_impl(
        &self,
        clauses: &Clauses,
        _root: &Path,
    ) -> anyhow::Result<Vec<SymbolMatch>> {
        // Query pipeline:
        //   Stage 0 — reject WHERE fields that exist neither as core fields nor
        //             as enrichment columns anywhere in this index: they can
        //             never match, and scanning millions of rows to find that
        //             out can exhaust host memory.
        //   Stage 1 — prefilter_global: intersect indexed predicates (kind
        //             bitmap, name FST, trigram / short-prefix LIKE index) into a
        //             candidate global row-ID bitmap.
        //   Stage 2 — partition by segment, then prune the survivors: IN/EXCLUDE
        //             path globs, dirty-overlay shadows, and numeric zone maps.
        //   Stage 3 — materialise the surviving rows per segment: stamp usage
        //             counts, apply residual WHERE, enforce the row budget
        //             (FORGEQL_FIND_MAX_ROWS) — then union the dirty overlay.
        //   Stage 4 — deduplicate on (name, fql_kind, path, line).
        //   Stage 5 — apply residual WHERE, ORDER BY, LIMIT, OFFSET.
        // GROUP BY and ORDER BY name fast-paths short-circuit the pipeline. The
        // count-based GROUP BY paths are only valid when source paths are unique;
        // duplicates overcount, so fall through to the deduplicating pipeline.
        self.reject_unknown_where_fields(clauses)?;
        self.reject_unknown_order_by_field(clauses)?;
        let no_dup_paths = !self.overlay.has_duplicate_paths();
        if group_by_kind_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return Ok(self.fast_group_by_kind(clauses));
        }
        if group_by_file_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return Ok(self.fast_group_by_file(clauses));
        }
        if let Some(mut results) = self.try_order_by_name_fast_paths(clauses) {
            self.stamp_usage_counts(&mut results);
            return Ok(results);
        }

        let mut by_segment = self.build_candidate_segments(clauses);
        self.prune_candidate_segments(&mut by_segment, clauses);

        let mut results = self.materialize_all(&by_segment, clauses)?;
        // Stage 3b — union dirty overlay rows (empty when the overlay is empty).
        // Persistent rows were stamped during materialisation; dirty rows still
        // need their workspace usage counts before Stage 5 evaluates them.
        if !self.dirty.is_empty() {
            let mut dirty_results = self.dirty.materialize_all(clauses);
            self.stamp_usage_counts(&mut dirty_results);
            results.append(&mut dirty_results);
        }
        dedupe_symbol_matches(&mut results);
        apply_clauses(&mut results, clauses);
        Ok(results)
    }

    /// Stage 0 — fail fast on WHERE fields that cannot match anything.
    ///
    /// A field is accepted when it is a core field, a known enrichment field
    /// of any registered language, or an enrichment column stored by at least
    /// one segment (persistent or dirty) of this index.  Anything else — a
    /// typo or an invented field — is rejected with guidance instead of
    /// silently matching nothing after a full-index scan.
    fn reject_unknown_where_fields(&self, clauses: &Clauses) -> anyhow::Result<()> {
        for pred in &clauses.where_predicates {
            let field = pred.field.as_str();
            if crate::filter::CORE_WHERE_FIELDS.contains(&field) {
                continue;
            }
            if crate::storage::legacy::is_known_enrichment_field(field) {
                continue;
            }
            if self.segments.iter().any(|s| s.has_extra_col(field)) {
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
            anyhow::bail!(
                "unknown WHERE field '{field}': it is not a core field and no \
                 indexed row carries an enrichment column with that name, so it \
                 can never match.  Core fields: name, fql_kind, path, file, \
                 line, usages, language, extension, size, depth.  To search \
                 file contents use SHOW LINES OF '<file>' WHERE text MATCHES \
                 '…' on specific files instead of FIND symbols."
            );
        }
        Ok(())
    }

    /// Reject an `ORDER BY` on a field that can never sort a symbol row.
    ///
    /// A field orders symbols only if it is a sortable core field, a known
    /// enrichment field, or a materialised extra column somewhere in this
    /// index.  Anything else (e.g. `size` / `depth`, which belong to
    /// `FIND files`) produces `None` for every row, so the comparator would
    /// silently fall back to name order and hand the agent alphabetical rows
    /// mislabelled as "top N by <field>".  Fail loudly instead — matching the
    /// legacy backend's `validate_order_by_field` contract.
    fn reject_unknown_order_by_field(&self, clauses: &Clauses) -> anyhow::Result<()> {
        let Some(ref order) = clauses.order_by else {
            return Ok(());
        };
        let field = order.field.as_str();
        if crate::filter::SORTABLE_SYMBOL_FIELDS.contains(&field) {
            return Ok(());
        }
        if crate::storage::legacy::is_known_enrichment_field(field) {
            return Ok(());
        }
        if self.segments.iter().any(|s| s.has_extra_col(field)) {
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
        anyhow::bail!(
            "unknown ORDER BY field '{field}': it is not a sortable symbol field \
             and no indexed row carries an enrichment column with that name, so \
             every symbol would tie and fall back to name order.  Sortable \
             fields: name, fql_kind, path, file, line, usages, count, plus any \
             enrichment field (lines, param_count, branch_count, …).  'size' and \
             'depth' apply to FIND files, not FIND symbols."
        );
    }

    /// Stage 4b (BUG-006 U3): overwrite each row's `usages_count` with the
    /// workspace-total usage-site count from the overlay usages aggregate.
    ///
    /// The per-segment `usages_count` column is a legacy always-zero field;
    /// the overlay FST is the source of truth. One O(log n) FST lookup per
    /// materialised row, before LIMIT — bounded by the candidate set size.
    pub(in super::super) fn stamp_usage_counts(&self, results: &mut [SymbolMatch]) {
        for row in results.iter_mut() {
            let count = self.overlay.usage_count(&row.name);
            row.usages_count = Some(usize::try_from(count).unwrap_or(usize::MAX));
        }
    }

    pub(super) fn find_usages_impl(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> (Vec<SymbolMatch>, Option<String>) {
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
        let occurrence_row = |path: &Path, line: u32, role: &str| SymbolMatch {
            name: name.to_string(),
            node_kind: None,
            fql_kind: None,
            language: None,
            path: Some(path.to_path_buf()),
            line: Some(usize::try_from(line).unwrap_or(usize::MAX)),
            usages_count: None,
            fields: HashMap::from([("role".to_owned(), role.to_owned())]),
            count: None,
            node_id: None,
            // A usage site is a line, not a node: no handle, so no rev.
            rev: None,
        };

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

        let mut sites: Vec<(std::path::PathBuf, u32, String)> = Vec::new();
        for token in &names {
            sites.extend(self.sites_of(token));
        }

        // Both tiers above can only answer with a stored token, and a name
        // spanning a separator no language continues a token over was never
        // stored whole — so it answers zero however many files hold it, which
        // reads as "there are none". Its pieces were stored, so they propose
        // candidate lines and the source line decides.
        //
        // Only when the tiers above found nothing. If they found the name it IS
        // a stored token and they are authoritative for it, so running this as
        // well would buy nothing and cost a piece enumeration on every working
        // query — `zephyr/pm/device.h` answers 383 sites from the whole-token
        // tier, and its pieces are each far too common to be worth counting.
        // The gap that leaves is a name stored whole by one language and split
        // by another in the same corpus: the split sites stay unreachable.
        let hint = if is_core_alphabet(name) || !sites.is_empty() {
            None
        } else {
            let (verified, why) = self.verify_by_pieces(name, root);
            sites.extend(verified);
            why
        };

        let mut results: Vec<SymbolMatch> = sites
            .iter()
            .map(|(path, line, role)| occurrence_row(path, *line, role))
            .collect();
        apply_clauses(&mut results, clauses);
        (results, hint)
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
                .overlay
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
    fn sites_of(&self, token: &str) -> Vec<(std::path::PathBuf, u32, String)> {
        const ROLE_CODE: &str = "code";

        let mut sites = Vec::new();
        for (idx, meta) in self.overlay.segments().iter().enumerate() {
            if self.dirty.shadows(&meta.source_path) {
                continue;
            }
            let Some(seg) = self.segments.get(idx) else {
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

    /// How many sites `token` has, without keeping any of them.
    ///
    /// The ceiling has to be decided before anything is materialised, so the
    /// pieces are counted first and collected only if the total clears it.
    fn site_count_of(&self, token: &str) -> usize {
        let mut count = 0;
        for (idx, meta) in self.overlay.segments().iter().enumerate() {
            if self.dirty.shadows(&meta.source_path) {
                continue;
            }
            if let Some(seg) = self.segments.get(idx) {
                count +=
                    seg.lookup_usage_lines(token).len() + seg.lookup_mention_sites(token).len();
            }
        }
        for ds in &self.dirty.added {
            count += ds.reader.lookup_usage_lines(token).len()
                + ds.reader.lookup_mention_sites(token).len();
        }
        count
    }

    /// Sites where `needle` occurs literally, proposed by its identifier pieces
    /// and confirmed against the source line itself.
    ///
    /// Every tier above answers with a *stored token*: exact for a name in the
    /// identifier alphabet, substring for one carrying anything else. Both miss
    /// whenever no recorder stored the needle whole — the ordinary case for a
    /// dotted or slashed name outside C, because a mention recorder continues a
    /// token only over the characters its language declares. A name like
    /// `foo-bar.frozen` is stored as `foo-bar` and `frozen`, so both tiers
    /// answer zero on a corpus holding it in dozens of files, and that zero
    /// reads exactly like "there are none".
    ///
    /// What the recorders did store is the needle's own pieces, so they propose
    /// the candidate lines and the line's text decides. Superset then verify,
    /// the same shape the trigram tier uses — and because the arbiter is the raw
    /// line, no proposal however loose can yield a false positive.
    ///
    /// All the pieces propose, not the cheapest one. Which piece a given site
    /// stored depends on the extra characters its language continues a token
    /// over, so the cheapest piece is routinely not the one that reaches the
    /// sites: for `foo-bar.frozen`, `bar` is stored nowhere the name appears
    /// while `frozen` is stored everywhere it does, and `bar` is the cheaper of
    /// the two. Together the pieces are still bounded, and when they are not —
    /// over `VERIFY_CANDIDATE_CAP` — the tier declines to run and says why,
    /// because a truncated site list would read as a complete one.
    ///
    /// Reads nothing stored in a new way, so no cache version moves.
    fn verify_by_pieces(
        &self,
        needle: &str,
        root: &Path,
    ) -> (Vec<(std::path::PathBuf, u32, String)>, Option<String>) {
        let pieces = needle_pieces(needle);
        if pieces.is_empty() {
            return (
                Vec::new(),
                Some(format!(
                    "'{needle}' holds no run of {MIN_PIECE_LEN} or more identifier characters \
                     opening on a letter, so nothing in the token index can propose a candidate \
                     line for it"
                )),
            );
        }

        // Every piece, not the cheapest one. Which piece a site stored depends
        // on the extra characters that site's language continues a token over,
        // and that varies per language and per file: `foo-bar.frozen` is stored
        // as `foo-bar` + `frozen` where `-` continues a token and as `foo` +
        // `bar` + `frozen` where it does not. Proposing from one piece would
        // silently skip every site that stored a different one — the same false
        // absence this tier exists to remove, only narrower.
        let count: usize = pieces.iter().map(|piece| self.site_count_of(piece)).sum();
        if count > VERIFY_CANDIDATE_CAP {
            return (
                Vec::new(),
                Some(format!(
                    "the parts of '{needle}' are too common to search by: together they propose \
                     {count} candidate lines, over the {VERIFY_CANDIDATE_CAP} ceiling, so the \
                     sites of '{needle}' itself were not searched for"
                )),
            );
        }

        let mut candidates: Vec<(std::path::PathBuf, u32, String)> = Vec::new();
        for piece in &pieces {
            candidates.extend(self.sites_of(piece));
        }

        // Two pieces on one line propose it twice; verify it once.
        let mut seen = HashSet::new();
        candidates.retain(|site| seen.insert(site.clone()));

        // One read per file, not one per site.
        let mut by_file: HashMap<&std::path::PathBuf, Vec<usize>> = HashMap::new();
        for (i, (path, _, _)) in candidates.iter().enumerate() {
            by_file.entry(path).or_default().push(i);
        }

        let mut verified = Vec::new();
        let mut unread = 0usize;
        for (path, rows) in by_file {
            let Ok(text) = std::fs::read_to_string(root.join(path)) else {
                // Every candidate in this file goes unchecked. Staying silent
                // about it would reintroduce, per file, exactly what the
                // ceiling exists to prevent: a short answer that reads as a
                // complete one.
                unread += 1;
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            for i in rows {
                if line_holds(&lines, candidates[i].1, needle) {
                    verified.push(candidates[i].clone());
                }
            }
        }

        let hint = (unread > 0).then(|| {
            format!(
                "{unread} candidate file(s) for '{needle}' could not be read, so any site they \
                 hold is missing from this answer"
            )
        });
        (verified, hint)
    }

    pub(super) fn indexed_files_impl(&self) -> Vec<crate::result::FileEntry> {
        let segs = self.overlay.segments();
        let file_only = self.overlay.file_entries();
        let mut entries = Vec::with_capacity(
            segs.len()
                .saturating_add(file_only.len())
                .saturating_add(self.dirty.added.len()),
        );

        // Base: persistent overlay segments with mmap-cached sizes.
        // Skip any segment shadowed (replaced or deleted) by the dirty overlay.
        for (idx, seg) in segs.iter().enumerate() {
            if self.dirty.shadows(&seg.source_path) {
                continue;
            }
            let ext = seg
                .source_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let size = u64::from(self.overlay.file_size(idx));
            let depth = Some(seg.source_path.components().count());
            entries.push(crate::result::FileEntry {
                path: seg.source_path.clone(),
                extension: ext,
                size,
                depth,
                count: None,
                error_count: None,
                parse_coverage: None,
                node_id: None,
                rev: None,
            });
        }

        // File-only entries (FQOV v8+): non-indexed workspace files tracked
        // only for path + size.  These are never shadowed by the dirty overlay
        // because the dirty overlay only holds symbol segments.
        for (rel_path, size) in file_only {
            // Session infrastructure, not source: the worktree gitfile pointer
            // and forgeql's own runtime artifacts (`.forgeql-session`, …).
            if crate::result::FileEntry::is_runtime_artifact(rel_path) {
                continue;
            }
            let ext = rel_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let depth = Some(rel_path.components().count());
            entries.push(crate::result::FileEntry {
                path: rel_path.clone(),
                extension: ext,
                size: u64::from(*size),
                depth,
                count: None,
                error_count: None,
                parse_coverage: None,
                node_id: None,
                rev: None,
            });
        }

        // Overlay: dirty segments (files changed in this session).
        // Read actual on-disk size — only 1 syscall per mutated file.
        for ds in &self.dirty.added {
            let ext = ds
                .source_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let size = self
                .worktree_root
                .join(&ds.source_path)
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0);
            let depth = Some(ds.source_path.components().count());
            entries.push(crate::result::FileEntry {
                path: ds.source_path.clone(),
                extension: ext,
                size,
                depth,
                count: None,
                error_count: None,
                parse_coverage: None,
                node_id: None,
                rev: None,
            });
        }

        dedupe_file_entries(&mut entries);
        entries
    }
}

impl ColumnarStorage {
    /// The four `ORDER BY name [DESC] [WHERE fql_kind=...] LIMIT N` fast-paths.
    ///
    /// Each streams the first `limit + offset` rows directly from the name FST
    /// in lexicographic order, materialising only those rows. All are gated on
    /// an empty dirty overlay because dirty rows are not path-sorted and could
    /// carry names that precede committed rows already streamed. Returns `None`
    /// when no name-ordered fast-path applies, so the caller runs the pipeline.
    fn try_order_by_name_fast_paths(&self, clauses: &Clauses) -> Option<Vec<SymbolMatch>> {
        if !self.dirty.is_empty() {
            return None;
        }
        let need = fast_path_need(clauses);
        let mut results = if order_by_name_fast_path(clauses) {
            self.overlay.stream_names_asc(need, &self.segments)
        } else if order_by_name_desc_fast_path(clauses) {
            self.overlay.stream_names_desc(need, &self.segments)
        } else if let Some(kind) = order_by_name_kind_fast_path(clauses) {
            let kind_bm = self.overlay.prefilter_kind(kind)?;
            self.overlay
                .stream_names_asc_kind_filtered(need, &kind_bm, &self.segments)
        } else if let Some(kind) = order_by_name_kind_desc_fast_path(clauses) {
            let kind_bm = self.overlay.prefilter_kind(kind)?;
            self.overlay
                .stream_names_desc_kind_filtered(need, &kind_bm, &self.segments)
        } else {
            return None;
        };
        dedupe_symbol_matches(&mut results);
        apply_clauses(&mut results, clauses);
        Some(results)
    }

    /// Build the initial `segment index -> local row bitmap` candidate map.
    ///
    /// Fast path (a path filter is present but no indexed predicate is
    /// available): seed every path-matching segment with all its rows, skipping
    /// the global prefilter and per-segment grouping. Normal path: global
    /// prefilter, group by segment, then IN / EXCLUDE path prune.
    fn build_candidate_segments(&self, clauses: &Clauses) -> HashMap<u32, RoaringBitmap> {
        let has_path_filter = clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty();
        if has_path_filter && !has_any_indexed_predicate(clauses, &self.overlay) {
            let mut map: HashMap<u32, RoaringBitmap> = HashMap::new();
            for (idx, meta) in self.overlay.segments().iter().enumerate() {
                if passes_resolve_glob(&meta.source_path, clauses)
                    && let (Some(seg), Ok(seg_idx)) = (self.segments.get(idx), u32::try_from(idx))
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
                    let row_range = self.overlay.path_row_range(prefix);
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
                self.overlay
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

/// Most candidate lines the verify tier will read before declining to run.
///
/// The tier is all-or-nothing on purpose: a truncated site list is
/// indistinguishable from a complete one, which is the very failure being fixed
/// here.
const VERIFY_CANDIDATE_CAP: usize = 5_000;

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

/// Does 1-based `line` of `lines` hold `needle` verbatim?
///
/// The arbiter of the verify tier, and the reason a loose proposal is safe. A
/// candidate line is proposed by ONE piece of the needle, so a line carrying
/// every piece scattered — `"foo-bar" and "frozen"` against `foo-bar.frozen` —
/// must not count. Only the needle's exact form, separators and all, does.
fn line_holds(lines: &[&str], line: u32, needle: &str) -> bool {
    usize::try_from(line)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| lines.get(n))
        .is_some_and(|text| text.contains(needle))
}

/// Deduplicate symbol results on `(name, fql_kind, path, line)`.
///
/// Mirrors the legacy backend, which deduplicates on
/// `(name_id, path_id, node_kind_id, line)`. The columnar result does not store
/// raw `node_kind`, so `fql_kind` is the closest available approximation.
fn dedupe_symbol_matches(results: &mut Vec<SymbolMatch>) {
    type DedupeKey = (
        String,
        Option<String>,
        Option<std::path::PathBuf>,
        Option<usize>,
    );
    let mut seen: HashSet<DedupeKey> = HashSet::new();
    results.retain(|r| seen.insert((r.name.clone(), r.fql_kind.clone(), r.path.clone(), r.line)));
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
    use super::{dedupe_file_entries, line_holds, needle_pieces};
    use crate::result::FileEntry;

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
    fn line_holds_demands_the_needle_verbatim_not_its_pieces() {
        // The proposal only knows that ONE piece is on the line. A line holding
        // every piece, separately, is the case that must still be rejected.
        let lines = vec![
            r#"  "use": "foo-bar.frozen","#,
            r#"  "foo-bar" and "frozen" both appear, apart"#,
        ];
        assert!(line_holds(&lines, 1, "foo-bar.frozen"));
        assert!(!line_holds(&lines, 2, "foo-bar.frozen"));
    }

    #[test]
    fn line_holds_is_1_based_and_survives_a_line_number_past_the_end() {
        let lines = vec!["first", "second"];
        assert!(line_holds(&lines, 2, "second"));
        assert!(!line_holds(&lines, 1, "second"));
        // A posting naming a line the file no longer has must answer false, not
        // panic and not wrap around to the last line.
        assert!(!line_holds(&lines, 0, "first"));
        assert!(!line_holds(&lines, 99, "first"));
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
