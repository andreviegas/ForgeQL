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

        let mut sites: Vec<Site> = Vec::new();
        for token in &names {
            sites.extend(self.sites_of(token));
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
        let (scanned, hint) = self.literal_sites(name, clauses, root);

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
    fn sites_of(&self, token: &str) -> Vec<Site> {
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
    fn literal_sites(
        &self,
        needle: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> (Vec<Site>, Option<String>) {
        const ROLE_TEXT: &str = "text";

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
        // symbols. A file the index tracks by path and size alone — a
        // `.gitignore`, an extension no plugin claims — holds text like any
        // other, and leaving it out would answer a confident zero for a name
        // written in it. `FIND files` lists exactly these three sources, minus
        // ForgeQL's own runtime artifacts; the search universe is the same set,
        // for the same reason.
        //
        // A file outside the query's own `IN`/`EXCLUDE` scope can only produce
        // rows the clause pipeline will drop, so it is never read.
        let mut paths: Vec<std::path::PathBuf> = self
            .overlay
            .segments()
            .iter()
            .filter(|meta| !self.dirty.shadows(&meta.source_path))
            .map(|meta| meta.source_path.clone())
            .chain(
                self.overlay
                    .file_entries()
                    .iter()
                    .map(|(path, _)| path.clone())
                    .filter(|path| !crate::result::FileEntry::is_runtime_artifact(path)),
            )
            .chain(self.dirty.added.iter().map(|ds| ds.source_path.clone()))
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
        for path in &paths {
            let Ok(bytes) = std::fs::read(root.join(path)) else {
                // The sites this file holds are absent from the answer and
                // nothing else in the response would show that.
                unread += 1;
                continue;
            };
            // Binary is not searched, on the line `grep` and `git` draw: a NUL
            // byte near the start means these bytes are not a text file. It
            // matters here because a compiled object or an index blob embeds
            // the ASCII of a symbol name, so reporting one as a usage site
            // would put bytes no editor should rewrite into a `FOUND` set and
            // arm a sweep on them.
            if bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0) {
                continue;
            }
            // Everything else is decoded leniently rather than strictly. A file
            // that is text apart from one byte in a legacy encoding is still
            // text, and every other line in it can hold the name verbatim;
            // rejecting the whole file for that byte would answer a confident
            // zero over bytes that do contain it, and silently, since the file
            // read fine. Valid UTF-8 — every source file — is borrowed here,
            // not copied.
            let text = String::from_utf8_lossy(&bytes);
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

        (sites, unread_hint(unread, needle))
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

/// How much of a file is inspected before calling it binary.
///
/// The rule `grep` and `git` use: a NUL byte in the first few kilobytes means
/// these bytes are not text. Reading further buys nothing — a file that is text
/// for 8 kB and binary afterwards does not exist in practice, and the check
/// runs once per file per query.
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
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(needle).any(|(start, _)| {
        let before = line[..start].chars().next_back();
        let after = line[start + needle.len()..].chars().next();
        before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c))
    })
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
    use std::path::Path;

    use super::{Clauses, dedupe_file_entries, holds, in_scope, needle_pieces};
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
