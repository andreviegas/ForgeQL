//! Phase 9 fast-path query methods and module-level helpers for [`super::ColumnarStorage`].
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use roaring::RoaringBitmap;

use crate::ast::query::glob_matches;
use crate::filter::{
    TOPK_THRESHOLD, apply_clauses, collect_top_k, eval_predicate, eval_predicate_on, order_cmp,
};
use crate::ir::{Clauses, CompareOp, GroupBy, OrderBy, PredicateValue, SortDirection};
use crate::result::SymbolMatch;

use super::super::overlay::{Overlay, RowPtr};
use super::super::segment_reader::{SegRowRef, SegmentReader};
use super::ColumnarStorage;

/// Over-fetch factor for the running top-K trim in [`ColumnarStorage::materialize_all`].
const TOPK_OVER_FETCH: usize = 4;

/// Default hard bound on rows one FIND may materialise before
/// ORDER/GROUP/LIMIT apply.  At a few hundred bytes per row this keeps peak
/// query memory in the low gigabytes even on multi-ten-million-symbol indexes.
const DEFAULT_FIND_MAX_ROWS: usize = 5_000_000;

/// Row budget for [`ColumnarStorage::materialize_all`], read per query.
/// `FORGEQL_FIND_MAX_ROWS` overrides the default; `0` disables the bound.
fn find_max_rows() -> usize {
    match std::env::var("FORGEQL_FIND_MAX_ROWS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_FIND_MAX_ROWS,
    }
}

/// Split a segment's residual `WHERE` into the predicates a row view answers
/// from the columns and the ones that have to wait for materialised rows.
///
/// The split is by construction, so every predicate is in exactly one half and
/// none can be lost between them. A predicate lands on the late side when
/// [`SegmentReader::answers_field`] declines its field — `usages`, which is
/// stamped from the workspace overlay only after materialisation; `node_id`,
/// which is built during it; or a field no column of this segment holds — and
/// also when the operator is a regex, because `apply_where_predicates`
/// compiles the pattern once for a whole batch while a per-row evaluation
/// would recompile it for every row.
fn split_seg_predicates<'p>(
    seg: &SegmentReader,
    predicates: &'p [crate::ir::Predicate],
    has_path: bool,
) -> (
    Vec<(&'p str, &'p crate::ir::Predicate)>,
    Vec<crate::ir::Predicate>,
) {
    let mut early = Vec::new();
    let mut late = Vec::new();
    for predicate in predicates {
        let field = crate::field_tiers::canonical(&predicate.field);
        if matches!(predicate.op, CompareOp::Matches | CompareOp::NotMatches)
            || !seg.answers_field(field, has_path)
        {
            late.push(predicate.clone());
        } else {
            early.push((field, predicate));
        }
    }
    (early, late)
}

impl ColumnarStorage {
    // ─────────────────────────────────────────────────────────────────────
    // Phase 9 — GROUP BY / ORDER BY fast-path methods
    // ─────────────────────────────────────────────────────────────────────

    /// Fast-path for `FIND symbols GROUP BY file ORDER BY count DESC LIMIT N`
    /// when there are no WHERE predicates.
    ///
    /// Sums `SegmentMeta.row_count` per source path in O(segments) time.
    /// No individual symbol rows are materialised.
    pub(super) fn fast_group_by_file(&self, clauses: &Clauses) -> Vec<SymbolMatch> {
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();

        let path_floor = clauses
            .in_glob
            .as_deref()
            .and_then(glob_to_path_prefix)
            .map(|prefix| {
                let row_range = self.overlay().path_row_range(prefix);
                row_range.collect::<RoaringBitmap>()
            });

        // Compute candidates if there are filters
        let has_filters = !clauses.where_predicates.is_empty();
        let candidates = if has_filters {
            Some(self.prefilter_global(clauses, path_floor))
        } else {
            None
        };

        for (idx, meta) in self.overlay().segments().iter().enumerate() {
            if !passes_resolve_glob(&meta.source_path, clauses) {
                continue;
            }
            let count = candidates
                .as_ref()
                .map_or(meta.dedup_row_count as usize, |cand| {
                    let range = self.overlay().segment_row_range(idx);
                    usize::try_from(cand.range_cardinality(range)).unwrap_or(usize::MAX)
                });
            if count > 0 {
                *counts.entry(meta.source_path.clone()).or_insert(0) += count;
            }
        }
        let mut results: Vec<SymbolMatch> = counts
            .into_iter()
            .map(|(path, count)| SymbolMatch {
                name: String::new(),
                path: Some(path),
                count: Some(count),
                ..SymbolMatch::default()
            })
            .collect();
        // HAVING (count-based filtering)
        // HAVING, ORDER BY, LIMIT (skip GROUP BY — already grouped; IN/EXCLUDE already applied
        // during segment iteration so strip them here to avoid re-filtering by path.
        // Also clear where_predicates since we already filtered the roaring bitmaps by them.)
        let mut no_group = clauses.clone();
        no_group.group_by = None;
        no_group.in_glob = None;
        no_group.exclude_globs.clear();
        no_group.where_predicates.clear();
        apply_clauses(&mut results, &no_group);
        results
    }

    /// Fast-path for `FIND symbols GROUP BY fql_kind ORDER BY count DESC LIMIT N`
    /// when there are no WHERE predicates.
    ///
    /// Deserialises each kind bitmap and reads its cardinality in O(n_kinds) time.
    /// For IN-glob queries, intersects each kind bitmap with the path range bitmap.
    pub(super) fn fast_group_by_kind(&self, clauses: &Clauses) -> Vec<SymbolMatch> {
        // Build an optional path mask for IN/EXCLUDE glob filtering.
        let path_mask: Option<RoaringBitmap> =
            if clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty() {
                let bm: RoaringBitmap = self
                    .overlay()
                    .segments()
                    .iter()
                    .enumerate()
                    .filter(|(_, meta)| passes_resolve_glob(&meta.source_path, clauses))
                    .flat_map(|(seg_idx, _)| self.overlay().segment_row_range(seg_idx))
                    .collect();
                Some(bm)
            } else {
                None
            };

        let kind_counts = self.overlay().kind_global_counts(path_mask.as_ref());
        let mut results: Vec<SymbolMatch> = kind_counts
            .into_iter()
            .map(|(kind, count)| SymbolMatch {
                name: kind.clone(),
                fql_kind: Some(kind),
                count: Some(count),
                ..SymbolMatch::default()
            })
            .collect();
        for pred in &clauses.having_predicates {
            let p = pred.clone();
            results.retain(|item| eval_predicate(item, &p));
        }
        // IN/EXCLUDE already applied via path_mask — strip to avoid re-filtering.
        let mut no_group = clauses.clone();
        no_group.group_by = None;
        no_group.in_glob = None;
        no_group.exclude_globs.clear();
        apply_clauses(&mut results, &no_group);
        results
    }

    // ─────────────────────────────────────────────────────────────────────
    // Query helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Stage 1 — build a candidate global-row-id bitmap using indexed predicates.
    ///
    /// Handles `WHERE fql_kind = 'X'`, `WHERE name = 'Y'` (exact match), and
    /// `WHERE name LIKE 'pattern'` / `WHERE name MATCHES 'regex'` via the
    /// trigram index when the pattern contains a literal substring of \u22653 chars.
    /// Other predicates are handled later by `apply_clauses`.
    /// `path_floor` — when the caller knows a contiguous path row-range,
    /// it passes it here so that (a) the fallback universe is bounded to
    /// that range instead of the full table, and (b) every per-predicate
    /// bitmap is intersected with the path range immediately, keeping
    /// intermediate results small.
    pub(super) fn prefilter_global(
        &self,
        clauses: &Clauses,
        path_floor: Option<RoaringBitmap>,
    ) -> RoaringBitmap {
        let mut result: Option<RoaringBitmap> = path_floor;

        for pred in &clauses.where_predicates {
            // Canonical, so an alias reaches the same structure the canonical
            // spelling does. `WHERE kind = 'x'` used to fall past the kind
            // bitmap into the enrichment arms, which answer a core row field
            // by asking whether any segment stores an enrichment column of
            // that name — a question whose "no" is not absence.
            let field = crate::field_tiers::canonical(&pred.field);
            let Some(kind_bm) = (match (field, &pred.op, &pred.value) {
                ("fql_kind", CompareOp::Eq, PredicateValue::String(val)) => {
                    // When the kind is absent in every segment, return an empty
                    // bitmap immediately rather than None.  None would fall
                    // through to the full-table scan, causing ~8 s regressions
                    // for unknown-kind queries.  See Phase 06d, Root cause 1.
                    Some(self.overlay().prefilter_kind(val).unwrap_or_default())
                }
                ("name", CompareOp::Eq, PredicateValue::String(val)) => {
                    let bm = self.overlay().lookup_name_bitmap(val);
                    if bm.is_empty() {
                        Some(RoaringBitmap::new())
                    } else {
                        Some(bm)
                    }
                }
                ("name", CompareOp::Like, PredicateValue::String(val)) => {
                    // For 1-2 char leading literals, use the per-segment name
                    // prefix index (faster than trigrams for very short keys).
                    // For 3+ char literals, fall through to the trigram index.
                    pattern_as_prefix(val).map_or_else(
                        || self.trigram_prefilter_for_pattern(val),
                        |prefix| self.short_prefix_global_bitmap(&prefix),
                    )
                }
                ("name", CompareOp::Matches, PredicateValue::String(val)) => self
                    .name_regex_bitmap(val)
                    .or_else(|| self.trigram_prefilter_for_regex(val)),

                // `language` is a stored per-row column, so it is served from
                // the column itself rather than from any posting index. It is
                // matched here, ahead of the enrichment arms, because those
                // refuse core row fields by name.
                ("language", op, PredicateValue::String(val)) => {
                    self.core_language_bitmap(*op, val.as_str())
                }
                // Phase 5: enrichment bitmap prefilter (FQOV v7).
                // Look up global bitmaps for enrichment predicates.
                (field, CompareOp::Eq, PredicateValue::String(val))
                    if field != "fql_kind" && field != "name" =>
                {
                    self.enrichment_eq_bitmap(field, val.as_str())
                }
                (field, CompareOp::Eq, PredicateValue::Bool(b)) => {
                    let val_str = if *b { "true" } else { "false" };
                    self.enrichment_eq_bitmap(field, val_str)
                }
                // Pattern predicates read the field's distinct values, never
                // its rows. `name` never reaches here — its own arms are above.
                (
                    field,
                    CompareOp::Like
                    | CompareOp::Matches
                    | CompareOp::NotLike
                    | CompareOp::NotMatches,
                    PredicateValue::String(val),
                ) => self.enrichment_pattern_bitmap(field, pred.op, val.as_str()),
                // The numeric arms need the same over-budget compensation the
                // Eq and pattern arms apply. `guard_group_id` is decimal, so
                // `guard_group_id > N` reaches here — and it is keyed from
                // postings now rather than from the row walk, which means a
                // file over its budget contributes no keys and its rows would
                // be intersected away. `rows_missing_field_postings` is empty
                // for every field that is not posted, so this costs nothing
                // there.
                (field, CompareOp::Gte, PredicateValue::Number(v)) => self
                    .overlay()
                    .prefilter_enrichment_ge(field, *v)
                    .map(|bm| bm | self.rows_missing_field_postings(field)),
                (field, CompareOp::Gt, PredicateValue::Number(v)) => self
                    .overlay()
                    .prefilter_enrichment_ge(field, v + 1)
                    .map(|bm| bm | self.rows_missing_field_postings(field)),
                (field, CompareOp::Lte, PredicateValue::Number(v)) => self
                    .overlay()
                    .prefilter_enrichment_le(field, *v)
                    .map(|bm| bm | self.rows_missing_field_postings(field)),
                (field, CompareOp::Lt, PredicateValue::Number(v)) => self
                    .overlay()
                    .prefilter_enrichment_le(field, v - 1)
                    .map(|bm| bm | self.rows_missing_field_postings(field)),
                _ => None,
            }) else {
                continue;
            };
            result = Some(match result {
                Some(prev) => prev & kind_bm,
                None => kind_bm,
            });
        }

        result.unwrap_or_else(|| (0..self.overlay().row_count()).collect())
    }

    /// Global candidate bitmap for an `Eq` predicate on an enrichment field.
    ///
    /// A hit returns the value's stored row bitmap. A MISS is the interesting
    /// case: the overlay holds no bitmap for `field=value`. Returning `None`
    /// there makes the caller skip the prefilter entirely and materialise
    /// every row in the corpus, so asking for a value that simply does not
    /// exist costs a full scan — `guard_kind = 'ifdef'` measured 7.48 s on a
    /// 3M-symbol corpus to produce an answer that is always empty, because
    /// `guard_kind` is only ever `preprocessor`, `attribute` or `heuristic`.
    ///
    /// So a miss returns an EMPTY bitmap instead — but only once the segments
    /// have confirmed the value really is absent, since an empty bitmap
    /// asserts "no such row exists" and a wrong assertion there is a silent
    /// false negative rather than a slow query. The same reasoning already
    /// governs the `fql_kind` arm above.
    fn enrichment_eq_bitmap(&self, field: &str, value: &str) -> Option<RoaringBitmap> {
        if let Some(bm) = self.overlay().prefilter_enrichment_eq(field, value) {
            // The stored bitmap accounts only for segments that posted this
            // field; one that did not still stores the column, so its rows can
            // carry the value and must stay candidates.
            return Some(bm | self.rows_missing_field_postings(field));
        }
        // The arm routing here matches ANY field name, including core row
        // metadata the enrichment index never stores (`language`, `path`,
        // `node_kind`). For one of those, "no segment stores a column for it"
        // is not evidence of absence — it is the field being served somewhere
        // else entirely — and reading it as absence returned zero rows
        // corpus-wide for `WHERE language = '<lang>'`. Core fields are refused
        // by name first: the two universes are disjoint today, but only this
        // check keeps that true the day an enricher writes a field named like
        // a core one.
        if crate::filter::CORE_WHERE_FIELDS.contains(&field) || !self.is_enrichment_field(field) {
            return None;
        }
        self.no_segment_carries_enrichment_value(field, value)
            .then(RoaringBitmap::new)
    }

    /// Rows the overlay's per-value bitmaps for `field` cannot speak for.
    ///
    /// Only the fields in `POSTING_ENRICHMENT_FIELDS` are keyed FROM postings,
    /// and a segment whose distinct-value count for one of them exceeded the
    /// per-segment budget contributes none — while still storing the column, so
    /// its rows do carry values. Intersecting a candidate set with such a
    /// bitmap drops them from the answer, so they stay candidates and the
    /// residual filter decides.
    ///
    /// Every OTHER enrichment field is keyed by walking each segment's rows
    /// (`overlay_builder::collect_numeric_enrichment`), so its key set is
    /// already complete and needs nothing added. Backfilling one anyway would
    /// be sound but ruinous: no segment posts those fields, so the condition
    /// below holds for every segment carrying the column and the candidate set
    /// becomes the corpus — turning `is_magic = 'true'` and `num_format = 'dec'`
    /// back into the full scan the bitmap exists to avoid.
    fn rows_missing_field_postings(&self, field: &str) -> RoaringBitmap {
        let mut rows = RoaringBitmap::new();
        if !super::super::segment_builder::POSTING_ENRICHMENT_FIELDS.contains(&field) {
            return rows;
        }
        for (idx, seg) in self.segments().iter().enumerate() {
            if !seg.posts_field(field) && seg.has_extra_col(field) {
                let _ = rows.insert_range(self.overlay().segment_row_range(idx));
            }
        }
        rows
    }

    /// Global candidate bitmap for a `LIKE` / `MATCHES` predicate (or either
    /// negation) on an enrichment field.
    ///
    /// The pattern is evaluated against the field's DISTINCT VALUES and the
    /// matching values' bitmaps are unioned, so the cost scales with how many
    /// values the field has rather than with how many rows the corpus holds.
    /// Returning `None` skips the prefilter and materialises every row.
    fn enrichment_pattern_bitmap(
        &self,
        field: &str,
        op: CompareOp,
        pattern: &str,
    ) -> Option<RoaringBitmap> {
        // Same refusal as the `Eq` arm above: this routing matches ANY field
        // name, and core row metadata is served somewhere else entirely. No
        // second "is this an enrichment field?" probe is needed here — the
        // value walk below answers `None` for a field it holds no key for,
        // which is already the fall-back-to-the-scan signal — and skipping it
        // keeps an unserved field's pattern off a scan of every segment's
        // column list, once per query.
        if crate::filter::CORE_WHERE_FIELDS.contains(&field) {
            return None;
        }

        let negated = matches!(op, CompareOp::NotLike | CompareOp::NotMatches);
        let re = if matches!(op, CompareOp::Matches | CompareOp::NotMatches) {
            Some(regex::Regex::new(pattern).ok()?)
        } else {
            None
        };
        // Decide values exactly as `filter::eval_predicate` decides rows. This
        // tier proposes candidates that the filter then verifies, so a matcher
        // that disagrees by one value drops rows the filter would have kept.
        let accepts = |value: &str| {
            re.as_ref().map_or_else(
                || crate::filter::like_match(value, pattern),
                |r| r.is_match(value),
            )
        };

        // An empty value is never keyed in the enrichment index, so for a
        // pattern that accepts one the value universe is not a complete account
        // of what can match. The negated form needs no such guard: it is the
        // universe MINUS the matches, and a match the walk missed only leaves
        // its row a candidate.
        if !negated && accepts("") {
            return None;
        }

        let matched = self
            .overlay()
            .prefilter_enrichment_values(field, &accepts)?;
        Some(if negated {
            let mut all = RoaringBitmap::new();
            let _ = all.insert_range(0..self.overlay().row_count());
            all - matched
        } else {
            matched | self.rows_missing_field_postings(field)
        })
    }

    /// Global candidate bitmap for `name MATCHES`, evaluated against the name
    /// FST's keys — the workspace's distinct names — rather than its rows.
    ///
    /// The literal-trigram prefilter this precedes cannot serve an alternation
    /// at all: a match needs the literals of ONE branch, so intersecting the
    /// branches' candidate sets drops every real match. It correctly declines
    /// there, and declining means materialising the whole corpus. Walking the
    /// FST instead treats the pattern as opaque — asked about each name, once.
    fn name_regex_bitmap(&self, pattern: &str) -> Option<RoaringBitmap> {
        let re = regex::Regex::new(pattern).ok()?;
        // A row with no name is not a key in the FST, so a pattern that accepts
        // the empty string cannot be answered from the key set.
        if re.is_match("") {
            return None;
        }
        self.overlay()
            .prefilter_name_values(&|name| re.is_match(name))
    }

    /// Global candidate bitmap for a predicate on `language` / `lang`.
    ///
    /// `language` is a core row column, not an enrichment posting, so no index
    /// tier served it: the query materialised every row in the corpus — full
    /// `SymbolMatch` and enrichment map apiece — so the residual filter could
    /// read a field the row already carried. That measured 4.6 s per query on
    /// a 3M-symbol corpus. Comparing the stored ids instead touches one `u32`
    /// per row and builds nothing.
    ///
    /// The bitmap is EXACT, not merely a superset: every row of every segment
    /// is decided against its own stored value, and a row whose language is
    /// empty is skipped because `SymbolMatch::field_str` reports `None` there
    /// and `filter::eval_predicate` fails every operator — negations included —
    /// on a field that is absent. So this may also answer an absence: an empty
    /// bitmap here really does mean no such row exists. A segment whose column
    /// does not cover its rows one-for-one takes the whole query back to the
    /// complete scan instead.
    fn core_language_bitmap(&self, op: CompareOp, value: &str) -> Option<RoaringBitmap> {
        // Only the string operators are decidable from a stored string; a
        // numeric comparison on `language` falls through to the scan.
        let negated = match op {
            CompareOp::Eq | CompareOp::Like | CompareOp::Matches => false,
            CompareOp::NotEq | CompareOp::NotLike | CompareOp::NotMatches => true,
            _ => return None,
        };
        let re = if matches!(op, CompareOp::Matches | CompareOp::NotMatches) {
            Some(regex::Regex::new(value).ok()?)
        } else {
            None
        };
        // Decide a language exactly as `filter::eval_predicate` decides a row.
        let accepts = |stored: &str| {
            let hit = match op {
                CompareOp::Like | CompareOp::NotLike => crate::filter::like_match(stored, value),
                CompareOp::Matches | CompareOp::NotMatches => {
                    re.as_ref().is_some_and(|r| r.is_match(stored))
                }
                _ => stored == value,
            };
            hit != negated
        };

        let mut rows = RoaringBitmap::new();
        for (idx, seg) in self.segments().iter().enumerate() {
            let local = seg.rows_with_language_matching(&accepts)?;
            let base = self.overlay().segment_row_range(idx).start;
            rows.extend(local.iter().map(|row| row + base));
        }
        Some(rows)
    }

    /// `true` when `field` is served by the enrichment tier at all: the overlay
    /// holds a key for it, or some segment stores a column for it.
    ///
    /// A core row field is served by neither, and the segments' silence about
    /// it must never be read as absence.
    fn is_enrichment_field(&self, field: &str) -> bool {
        self.overlay().has_enrichment_field(field)
            || self.segments().iter().any(|seg| seg.has_extra_col(field))
    }

    /// Whether no persistent row anywhere carries `field = value`.
    ///
    /// Asked of the segments rather than of the overlay's key set, because the
    /// overlay is not in a position to answer it. Its keys are built from
    /// `local_bm & canonical_bm`, so a value carried only by rows that lost the
    /// per-segment `(name, fql_kind, line)` dedup — or that sit in a shadowed
    /// duplicate path — is keyed nowhere, even though the rows exist and the
    /// scan would return them. A missing key also cannot be told apart from a
    /// key whose bitmap failed to read. Per-segment postings have neither
    /// problem: they are keyed by raw row id and read per segment.
    ///
    /// A segment holding the column with no postings blob for it (the builder
    /// skips the blob past its per-segment cardinality cap) cannot prove
    /// anything, and one such segment is enough to keep the complete scan.
    ///
    /// Runs only on the miss path, whose current cost is a full-corpus scan,
    /// and short-circuits on the first segment that carries the value.
    fn no_segment_carries_enrichment_value(&self, field: &str, value: &str) -> bool {
        self.segments()
            .iter()
            .all(|seg| seg.proves_enrichment_value_absent(field, value))
    }

    /// Compute a trigram-based candidate bitmap from a SQL `LIKE` pattern.
    ///
    /// Returns `None` when no usable literal trigram can be extracted
    /// (caller should skip the prefilter for this predicate).
    fn trigram_prefilter_for_pattern(&self, pattern: &str) -> Option<RoaringBitmap> {
        let literals = crate::filter::like_pattern_literals(pattern);
        self.intersect_literal_trigrams(&literals)
    }

    /// Compute a trigram-based candidate bitmap from a regex.
    ///
    /// Conservatively only uses literal-character runs that don't contain
    /// any regex metacharacter.  Returns `None` when no run is \u2265 3 chars.
    fn trigram_prefilter_for_regex(&self, pattern: &str) -> Option<RoaringBitmap> {
        const META: &[char] = &[
            '\\', '.', '+', '*', '?', '(', ')', '[', ']', '{', '}', '|', '^', '$',
        ];
        // Alternation makes literal-run intersection unsound: a match needs the
        // literals of only ONE branch, not all of them. Splitting `A|B` at `|`
        // and intersecting the per-branch candidate sets requires a name to
        // contain every branch's text at once — which nothing does — so all
        // real matches are dropped. Bail to a full scan here; the real regex
        // still runs in `apply_clauses`. (BUG-007)
        if pattern.contains('|') {
            return None;
        }
        let mut literals: Vec<String> = Vec::new();
        let mut cur = String::new();
        for ch in pattern.chars() {
            if META.contains(&ch) {
                if !cur.is_empty() {
                    literals.push(std::mem::take(&mut cur));
                }
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            literals.push(cur);
        }
        self.intersect_literal_trigrams(&literals)
    }

    fn intersect_literal_trigrams(&self, literals: &[String]) -> Option<RoaringBitmap> {
        let mut acc: Option<RoaringBitmap> = None;
        for lit in literals {
            if lit.len() < 3 {
                continue;
            }
            let Some(bm) = self.overlay().name_substring_candidates(lit) else {
                continue;
            };
            acc = Some(match acc {
                Some(prev) => prev & bm,
                None => bm,
            });
        }
        acc
    }

    /// Build a global candidate bitmap using per-segment name prefix indexes.
    ///
    /// For each segment:
    /// - If the segment has a `name_prefix` index, look up `prefix` in it
    ///   and map the resulting local row IDs to global row IDs.
    /// - If the segment has no prefix index (old format), include ALL its
    ///   rows as candidates (cannot prune).
    ///
    /// Returns `None` when NO segment has a prefix index (caller should
    /// fall through to a different prefilter or full scan).
    fn short_prefix_global_bitmap(&self, prefix: &[u8]) -> Option<RoaringBitmap> {
        let mut result = RoaringBitmap::new();
        let mut any_had_index = false;
        let mut seg_base: u32 = 0;
        for (seg_idx, seg) in self.segments().iter().enumerate() {
            let row_count = self
                .overlay()
                .segments()
                .get(seg_idx)
                .map_or(seg.row_count, |m| m.row_count);
            if seg.name_prefix.is_empty() {
                // No prefix index — include all rows from this segment.
                for local_row in 0..row_count {
                    let _ = result.insert(seg_base + local_row);
                }
            } else {
                any_had_index = true;
                if let Some(local_bm) = seg.name_prefix.get(prefix) {
                    for local_row in local_bm {
                        let _ = result.insert(seg_base + local_row);
                    }
                }
            }
            seg_base = seg_base.saturating_add(row_count);
        }
        if any_had_index { Some(result) } else { None }
    }
    /// Return the set of segment indices whose `source_path` passes
    /// `clauses.in_glob` AND every `clauses.exclude_globs` pattern.
    ///
    /// Returns `None` when neither filter is set (caller should treat as
    /// "all segments allowed").  Used to prune non-matching segments
    /// *before* `group_by_segment` so they are never opened or materialised.
    pub(super) fn segments_passing_path_filter(&self, clauses: &Clauses) -> Option<HashSet<u32>> {
        if clauses.in_glob.is_none() && clauses.exclude_globs.is_empty() {
            return None;
        }
        let mut allowed = HashSet::new();
        for (idx, meta) in self.overlay().segments().iter().enumerate() {
            if passes_resolve_glob(&meta.source_path, clauses)
                && let Ok(seg_idx) = u32::try_from(idx)
            {
                let _ = allowed.insert(seg_idx);
            }
        }
        Some(allowed)
    }

    /// Return the set of segment indices that *could* satisfy a numeric
    /// range predicate (`WHERE col OP val`) based on their zone maps.
    ///
    /// A segment is pruned (excluded from the returned set) when its
    /// `(min, max)` range provably cannot satisfy the predicate:
    /// - `col > val`  → prune when `seg.max ≤ val`
    /// - `col >= val` → prune when `seg.max < val`
    /// - `col < val`  → prune when `seg.min ≥ val`
    /// - `col <= val` → prune when `seg.min > val`
    /// - `col = val`  → prune when `val < seg.min || val > seg.max`
    ///
    /// Returns `None` when no segment has a zone map for the column
    /// (nothing can be pruned; caller should skip this optimisation).
    pub(super) fn segments_passing_zone_map(
        &self,
        col: &str,
        op: CompareOp,
        val: u32,
    ) -> Option<HashSet<u32>> {
        let mut any_zone_map = false;
        let mut allowed: HashSet<u32> = HashSet::new();
        for (idx, seg) in self.segments().iter().enumerate() {
            let Some(&(min, max)) = seg.zone_maps.get(col) else {
                // No zone map for this segment — cannot prune, include it.
                if let Ok(seg_idx) = u32::try_from(idx) {
                    let _ = allowed.insert(seg_idx);
                }
                continue;
            };
            any_zone_map = true;
            let passes = match op {
                CompareOp::Gt => max > val,
                CompareOp::Gte => max >= val,
                CompareOp::Lt => min < val,
                CompareOp::Lte => min <= val,
                CompareOp::Eq => val >= min && val <= max,
                // Non-range operators — cannot prune.
                _ => true,
            };
            if passes && let Ok(seg_idx) = u32::try_from(idx) {
                let _ = allowed.insert(seg_idx);
            }
        }
        if any_zone_map { Some(allowed) } else { None }
    }
    /// Stage 2 — partition global row IDs by segment index.
    pub(super) fn group_by_segment(
        &self,
        global_ids: &RoaringBitmap,
    ) -> HashMap<u32, RoaringBitmap> {
        let mut by_segment: HashMap<u32, RoaringBitmap> = HashMap::new();
        for global_id in global_ids {
            if let Some(RowPtr {
                segment_idx,
                local_row_idx,
            }) = self.overlay().resolve_global(global_id)
            {
                let _ = by_segment
                    .entry(segment_idx)
                    .or_default()
                    .insert(local_row_idx);
            }
        }
        by_segment
    }

    /// Stage 3 — materialize rows from each segment.
    pub(super) fn materialize_all(
        &self,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
    ) -> anyhow::Result<Vec<SymbolMatch>> {
        let seg_order = self.ordered_segments(by_segment);
        let fetch_cap = Self::fetch_cap_for(clauses);
        let topk_trim = Self::topk_trim_for(clauses);
        // Residual WHERE runs per segment so non-matching rows never
        // accumulate.  `count` is excluded: it is only assigned by GROUP BY
        // in Stage 5, so no materialised row carries it yet.
        let seg_predicates: Vec<crate::ir::Predicate> = clauses
            .where_predicates
            .iter()
            .filter(|p| p.field != "count")
            .cloned()
            .collect();
        let max_rows = find_max_rows();

        let mut results = Vec::new();
        for seg_idx in seg_order {
            // Early-exit: checked before opening the segment file so we don't pay
            // I/O cost once the fetch budget is exhausted.
            if fetch_cap.is_some_and(|cap| results.len() >= cap) {
                break;
            }
            let Some(mut seg_results) = self.materialize_one_segment(
                seg_idx,
                by_segment,
                clauses,
                fetch_cap,
                results.len(),
                &seg_predicates,
            ) else {
                continue;
            };
            results.append(&mut seg_results);

            // Running top-K trim: shed rows that cannot make the final top-K.
            // Fires whenever the working set exceeds K * TOPK_OVER_FETCH.
            if let Some(k) = topk_trim {
                let trim_at = k.saturating_mul(TOPK_OVER_FETCH);
                if results.len() > trim_at {
                    let keep = k.saturating_mul(TOPK_OVER_FETCH / 2).max(k);
                    results = collect_top_k(std::mem::take(&mut results), keep, |a, b| {
                        order_cmp(a, b, clauses)
                    });
                }
            }

            // Hard memory bound: refuse to grow past the row budget instead of
            // exhausting host RAM on an unscoped scan (a 42 M-symbol index can
            // otherwise materialise >25 GB before ORDER/GROUP/LIMIT applies).
            if results.len() > max_rows {
                anyhow::bail!(
                    "query materialised more than {max_rows} rows before \
                     ORDER/GROUP/LIMIT.  Narrow the scan with IN 'path/**', a \
                     more selective WHERE, or an explicit LIMIT — or raise \
                     FORGEQL_FIND_MAX_ROWS (0 disables the bound)."
                );
            }
        }
        Ok(results)
    }

    /// Segment indices sorted by source path (then line) — matches the legacy
    /// backend's path-ordered, file-by-file iteration so ORDER BY tie-breaking
    /// on equal-name symbols yields the same first-N across both backends.
    /// After FQOV v4 segments are already stored path-ordered, so for full-index
    /// queries this is a no-op; for filtered subsets it stays O(k log k).
    fn ordered_segments(&self, by_segment: &HashMap<u32, RoaringBitmap>) -> Vec<u32> {
        let mut seg_order: Vec<u32> = by_segment.keys().copied().collect();
        seg_order.sort_by_key(|&idx| {
            self.overlay()
                .segments()
                .get(idx as usize)
                .map(|m| m.source_path.clone())
        });
        seg_order
    }

    /// Early-exit fetch cap: when there is no ORDER BY / GROUP BY but an explicit
    /// LIMIT, stop opening segment files once the budget is spent.  Fetches cap+1
    /// so `total > results.len()` stays a reliable "more results exist" signal.
    /// When `limit` is None we deliberately do NOT inject DEFAULT_QUERY_LIMIT
    /// here — exec_find injects an explicit limit before calling find_symbols, so
    /// direct callers (tests) still receive all matching rows.
    fn fetch_cap_for(clauses: &Clauses) -> Option<usize> {
        if clauses.order_by.is_none() && clauses.group_by.is_none() {
            clauses.limit.map(|c| c.saturating_add(1))
        } else {
            None
        }
    }

    /// Running top-K trim budget (Phase 8): set when ORDER BY is present, LIMIT
    /// is small, OFFSET is zero, and GROUP BY is absent.  Bounds peak result
    /// memory to O(K * TOPK_OVER_FETCH) by periodically discarding rows that
    /// cannot make the final top-K.
    fn topk_trim_for(clauses: &Clauses) -> Option<usize> {
        if clauses.order_by.is_some()
            && clauses.group_by.is_none()
            && clauses.offset.unwrap_or(0) == 0
            && clauses.limit.is_some_and(|k| k <= TOPK_THRESHOLD)
        {
            clauses.limit
        } else {
            None
        }
    }

    /// Materialise one segment's matching rows: enrichment-posting prefilter →
    /// row materialisation → usage-count stamping → per-segment WHERE filter →
    /// fetch-budget trim.  Returns `None` to skip the segment (missing data or
    /// empty result).
    fn materialize_one_segment(
        &self,
        seg_idx: u32,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
        fetch_cap: Option<usize>,
        results_len: usize,
        seg_predicates: &[crate::ir::Predicate],
    ) -> Option<Vec<SymbolMatch>> {
        let local_rows = by_segment.get(&seg_idx)?;
        let seg = self.segments().get(seg_idx as usize)?;
        let seg_meta = self.overlay().segments().get(seg_idx as usize)?;

        // Stage 3a — narrow the local row set using per-segment enrichment
        // posting bitmaps before materialisation.  Falls back to the full local
        // set when no posting file exists for a given predicate.
        let narrowed = seg.prefilter_enrichment_postings(local_rows.clone(), clauses);
        if narrowed.is_empty() {
            return None;
        }

        // Stage 3b — test the residual WHERE against the segment's columns and
        // materialise only the rows that survive it.  Building a row is the
        // dominant cost of a filtered scan, and a row the predicate is going to
        // reject is a row that never needed building.
        //
        // `late` holds what a row view cannot answer, and it is not dropped:
        // every predicate is in exactly one of the two halves and `late` runs
        // against the built rows below, exactly as the whole set used to.
        let source_path = seg_meta.source_path.as_path();
        let (early, late) = split_seg_predicates(seg, seg_predicates, true);
        let narrowed = if early.is_empty() {
            narrowed
        } else {
            narrowed
                .iter()
                .filter(|&row| {
                    let view = SegRowRef {
                        seg,
                        row,
                        source_path: Some(source_path),
                    };
                    early
                        .iter()
                        .all(|(field, predicate)| eval_predicate_on(&view, field, predicate))
                })
                .collect()
        };
        if narrowed.is_empty() {
            return None;
        }

        // Pass the relative source path so IN/EXCLUDE glob matching in
        // apply_clauses works against the same relative paths the legacy backend
        // stores.  Do NOT join with worktree_root here.
        let mut seg_results = seg.materialize_rows(&narrowed, Some(source_path));

        // Stamp workspace usage counts before any predicate or top-K decision:
        // the per-segment `usages_count` column is a stale always-zero legacy
        // field, so WHERE usages / ORDER BY usages must see the overlay value.
        self.stamp_usage_counts(&mut seg_results);

        // Apply the residual WHERE per-segment so that only matching rows count
        // toward the fetch cap and the accumulated working set — non-matching
        // rows are dropped before they can pile up across segments.
        if !late.is_empty() {
            crate::filter::apply_where_predicates(&mut seg_results, &late);
        }

        // Trim within this segment to avoid overshooting the fetch budget.
        if let Some(cap) = fetch_cap {
            let remaining = cap.saturating_sub(results_len);
            seg_results.truncate(remaining);
        }
        Some(seg_results)
    }
}
// ─────────────────────────────────────────────────────────────────────────────
// Module-level helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Extract a leading-literal prefix from a SQL `LIKE` pattern.
///
/// Returns `Some(prefix_bytes)` only when the pattern starts with exactly
/// 1 or 2 literal UTF-8 characters before the first `%` or `_` wildcard.
/// Longer literals are handled by the trigram index.  Zero-length prefixes
/// (pattern starts with `%`) return `None` — nothing to prune.
///
/// The returned bytes are the lowercase UTF-8 encoding of the prefix
/// characters, matching the encoding used by the builder.
pub(super) fn pattern_as_prefix(pattern: &str) -> Option<Vec<u8>> {
    let mut prefix_bytes: Vec<u8> = Vec::new();
    let mut char_count = 0usize;
    let mut chars = pattern.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '%' || ch == '_' {
            break;
        }
        let lower_ch = ch.to_lowercase();
        for lc in lower_ch {
            let mut buf = [0u8; 4];
            let s = lc.encode_utf8(&mut buf);
            prefix_bytes.extend_from_slice(s.as_bytes());
        }
        char_count += 1;
        if char_count == 2 {
            // Stop accumulating at 2 chars.  But if the literal continues
            // (3rd character is not a wildcard and not end-of-string), the
            // trigram index is a stronger prefilter — return None so the
            // caller falls through to trigram_prefilter_for_pattern.
            if let Some(&(_, next)) = chars.peek()
                && next != '%'
                && next != '_'
            {
                return None; // 3+ char literal — use trigrams
            }
            break;
        }
    }
    if char_count == 1 || char_count == 2 {
        Some(prefix_bytes)
    } else {
        None
    }
}

/// Extract a literal directory prefix from a glob pattern for `path_row_range` clamping.
///
/// Returns the longest literal path prefix (including the trailing `/`) that
/// appears before the first wildcard character (`*`, `?`, or `[`).
/// Returns `None` when the glob has no such prefix (e.g. `*.c`, `**/*.c`).
///
/// Examples:
/// - `"include/**"`    → `Some("include/")`
/// - `"drivers/net/**"` → `Some("drivers/net/")`
/// - `"*.c"`           → `None`
/// - `"**/*.c"`        → `None`
pub(super) fn glob_to_path_prefix(glob: &str) -> Option<&str> {
    let wild_pos = glob.find(['*', '?', '['])?;
    let up_to = &glob[..wild_pos];
    let slash_pos = up_to.rfind('/')?;
    Some(&glob[..=slash_pos])
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 9 — GROUP BY / ORDER BY fast-paths that bypass full materialisation
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` when `GROUP BY file/path` can be served from segment metadata
/// alone (no per-row materialisation needed).
///
/// Condition: GROUP BY on the file/path field, no WHERE predicates (we cannot
/// predict which rows match a filter without reading them), dirty overlay empty
/// (dirty segments are not integrated into the overlay metadata yet).
pub(super) fn group_by_file_fast_path_eligible(clauses: &Clauses, dirty_empty: bool) -> bool {
    if !dirty_empty {
        return false;
    }
    // Canonical on both sides, here and in every other eligibility test: a
    // spelling that misses the fast path is answered by the slow pipeline, so
    // the failure is silent and shows up only as a latency number.
    if !matches!(&clauses.group_by, Some(GroupBy::Field(f))
        if crate::field_tiers::canonical(f) == "path")
    {
        return false;
    }
    // Phase 1: eligible if no where predicates, OR if all where predicates
    // are fql_kind / name indexed predicates only.
    // NOTE: enrichment predicates are intentionally excluded — fast_group_by_file
    // returns empty names, which breaks ordering and dedup in GROUP BY results.
    // Enrichment-predicate GROUP BY file queries use the normal pipeline, which
    // benefits from Phase 5 prefilter_global enrichment bitmaps for speed.
    if clauses.where_predicates.is_empty() {
        return true;
    }
    clauses.where_predicates.iter().all(|pred| {
        matches!(
            (crate::field_tiers::canonical(&pred.field), &pred.op),
            ("fql_kind", CompareOp::Eq)
                | ("name", CompareOp::Eq | CompareOp::Like | CompareOp::Matches)
        )
    })
}

/// Returns `true` when `GROUP BY fql_kind` can be served from the overlay's
/// kind bitmaps alone (no per-row materialisation needed).
pub(super) fn group_by_kind_fast_path_eligible(clauses: &Clauses, dirty_empty: bool) -> bool {
    dirty_empty
        && clauses.where_predicates.is_empty()
        && matches!(&clauses.group_by, Some(GroupBy::Field(f))
            if crate::field_tiers::canonical(f) == "fql_kind")
}

/// Returns `(kind_str, true)` when `ORDER BY name ASC LIMIT N` with a single
/// `WHERE fql_kind = 'kind_str'` predicate is eligible for the name-stream
/// fast-path extended with kind filtering.
pub(super) fn order_by_name_kind_fast_path(clauses: &Clauses) -> Option<&str> {
    // Base conditions same as order_by_name_fast_path, but allow exactly one
    // WHERE predicate that is a fql_kind equality.
    if !matches!(
        &clauses.order_by,
        Some(OrderBy { field, direction: SortDirection::Asc }) if field == "name"
    ) {
        return None;
    }
    if clauses.limit.is_none()
        || clauses.group_by.is_some()
        || clauses.in_glob.is_some()
        || !clauses.exclude_globs.is_empty()
    {
        return None;
    }
    // Exactly one WHERE predicate: fql_kind = '<kind>'
    if clauses.where_predicates.len() != 1 {
        return None;
    }
    let pred = &clauses.where_predicates[0];
    if crate::field_tiers::canonical(&pred.field) != "fql_kind" || pred.op != CompareOp::Eq {
        return None;
    }
    if let PredicateValue::String(ref kind) = pred.value {
        Some(kind.as_str())
    } else {
        None
    }
}

pub(super) fn order_by_name_kind_desc_fast_path(clauses: &Clauses) -> Option<&str> {
    if !matches!(
        &clauses.order_by,
        Some(OrderBy { field, direction: SortDirection::Desc }) if field == "name"
    ) {
        return None;
    }
    if clauses.limit.is_none()
        || clauses.group_by.is_some()
        || clauses.in_glob.is_some()
        || !clauses.exclude_globs.is_empty()
    {
        return None;
    }
    if clauses.where_predicates.len() != 1 {
        return None;
    }
    let pred = &clauses.where_predicates[0];
    if crate::field_tiers::canonical(&pred.field) != "fql_kind" || pred.op != CompareOp::Eq {
        return None;
    }
    if let PredicateValue::String(ref kind) = pred.value {
        Some(kind.as_str())
    } else {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolve helpers — shared by the three StorageEngine::resolve_* methods
// ─────────────────────────────────────────────────────────────────────────────
/// Split a qualified name (`Owner::member` or `Owner.member`) into
/// `(lookup_name, Some(owner))`.  Returns `(name, None)` for bare names.
///
/// Tries `::` first (Rust / C++) then `.` (Python / JS), mirroring the
/// legacy resolver's `split_qualified_name`.
pub(super) fn split_qualified_name(name: &str) -> (&str, Option<&str>) {
    if let Some(pos) = name.rfind("::") {
        let (owner, member) = (&name[..pos], &name[pos + 2..]);
        if !owner.is_empty() && !member.is_empty() {
            return (member, Some(owner));
        }
    }
    if let Some(pos) = name.rfind('.') {
        let (owner, member) = (&name[..pos], &name[pos + 1..]);
        if !owner.is_empty() && !member.is_empty() {
            return (member, Some(owner));
        }
    }
    (name, None)
}

/// Returns `true` when `clauses` contains at least one predicate that
/// [`ColumnarStorage::prefilter_global`] can answer with a bitmap narrower
/// than the full row-ID universe.  Used to decide whether the "fast path"
/// in `find_symbols` can skip the global bitmap entirely.
/// Return `true` when `ORDER BY name ASC LIMIT N` fast-path is eligible.
///
/// Conditions: ORDER BY name ASC, explicit LIMIT, no GROUP BY, no WHERE
/// predicates, no path filter.  The caller also gates on the dirty overlay
/// being empty so dirty rows cannot shadow committed rows with earlier names.
pub(super) fn order_by_name_fast_path(clauses: &Clauses) -> bool {
    matches!(
        &clauses.order_by,
        Some(OrderBy { field, direction: SortDirection::Asc }) if field == "name"
    ) && clauses.limit.is_some()
        && clauses.group_by.is_none()
        && clauses.where_predicates.is_empty()
        && clauses.in_glob.is_none()
        && clauses.exclude_globs.is_empty()
}

pub(super) fn order_by_name_desc_fast_path(clauses: &Clauses) -> bool {
    matches!(
        &clauses.order_by,
        Some(OrderBy { field, direction: SortDirection::Desc }) if field == "name"
    ) && clauses.limit.is_some()
        && clauses.group_by.is_none()
        && clauses.where_predicates.is_empty()
        && clauses.in_glob.is_none()
        && clauses.exclude_globs.is_empty()
}

pub(super) fn has_any_indexed_predicate(clauses: &Clauses, overlay: &Overlay) -> bool {
    clauses.where_predicates.iter().any(|pred| {
        matches!(
            (crate::field_tiers::canonical(&pred.field), &pred.op),
            ("fql_kind", CompareOp::Eq)
                | ("name", CompareOp::Eq | CompareOp::Like | CompareOp::Matches)
                // `language` is served from its stored column, so it is an
                // indexed predicate even though the overlay holds no key for
                // it. Without this the path-scoped shape an agent writes most
                // — `IN 'drivers/**' WHERE language = 'c'` — takes the
                // seed-every-row branch below and never reaches the tier.
                | ("language", _)
        ) || overlay.has_enrichment_field(&pred.field)
    })
}
/// no worktree-root stripping is needed.
pub(super) fn passes_resolve_glob(relative_path: &Path, clauses: &Clauses) -> bool {
    let in_ok = clauses
        .in_glob
        .as_deref()
        .is_none_or(|glob| glob_matches(relative_path, glob));
    let excl_ok = !clauses
        .exclude_globs
        .iter()
        .any(|glob| glob_matches(relative_path, glob));
    in_ok && excl_ok
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests;
