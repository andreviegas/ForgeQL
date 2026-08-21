//! Phase 9 fast-path query methods and module-level helpers for [`super::ColumnarStorage`].
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use roaring::RoaringBitmap;

use crate::ast::query::glob_matches;
use crate::filter::{
    TOPK_THRESHOLD, apply_clauses_counted, collect_top_k, eval_predicate_on, order_cmp,
};
use crate::ir::{Clauses, CompareOp, GroupBy, OrderBy, PredicateValue, SortDirection};
use crate::result::SymbolMatch;
use crate::storage::FindPage;

use super::super::overlay::{Overlay, RowPtr};
use super::super::segment_reader::{
    RowView, SegRowRef, SegmentReader, ranks_field_like_a_built_row,
};
use super::ColumnarStorage;
use super::query::find::dedupe_symbol_matches;

/// Over-fetch factor for the running top-K trim in [`ColumnarStorage::materialize_all`].
const TOPK_OVER_FETCH: usize = 4;

/// The memory one `FIND` may spend on materialised result rows.
///
/// The bound is a memory budget rather than a row count, because a row count is
/// the thing that drifts: rows have grown enrichment fields several times and
/// the count was never revisited. It is written as the budget and the per-row
/// cost so both are visible to the next person to change either.
const FIND_ROW_BUDGET_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Working figure for the cost of one materialised [`SymbolMatch`], including
/// the heap its `String`s and `HashMap` own rather than just the struct.
///
/// Rows vary — a bare occurrence row is smaller, an enrichment-heavy one
/// larger — and the observed range is 1.5–1.6 KB, of which this takes the
/// conservative end. It is the number to re-measure when the row grows a field,
/// not a constant that stays true on its own.
const FIND_BYTES_PER_ROW: usize = 1_600;

/// Default hard bound on rows one `FIND` may materialise before ORDER/GROUP/LIMIT
/// apply — the budget above divided by the per-row cost, about 1.34 million.
///
/// It is about **3.7x tighter than the five million it replaces**, so a scan
/// that used to complete can now be refused. That is the intended direction —
/// the old number was never measured against the row it was bounding — but it
/// is a reachability change, not a restatement, and where the new line falls on
/// a multi-million-symbol corpus has not been measured either. The case to
/// watch is a `GROUP BY` no fast path accepts: its answer is a handful of rows
/// but it materialises every matching row to get there. `fql_kind`, the path,
/// and an enrichment field the segments post per value are counted from the
/// index instead. All three want no uncommitted rows in the session, no two
/// segments built from one source path, and any `HAVING`/`ORDER BY` to name
/// only `count` or the grouped field — a group row carries those two and
/// nothing else, so a predicate on any other field would be false on every
/// group and deliver an empty set with full confidence. `fql_kind` and the
/// enrichment field also want no `WHERE`: a stored cardinality counts a value
/// over whole segments and cannot be narrowed to what a predicate selects.
/// `GROUP BY file` groups by segment, so it admits exactly one —
/// `fql_kind = '<value>'`, whose postings are intersected with each segment's
/// canonical rows at build and are the only tier that both verifies and
/// deduplicates. A `name` predicate is admitted at no literal length: the
/// trigram index over-generates, the name FST is never canonical-intersected,
/// and nothing on a counted path reads a row to settle either. The enrichment
/// one also wants the field to have survived the overlay's value budget with
/// none of the segments the query selects storing the column without posting
/// it — an excluded segment is not asked, its rows being in neither the counts
/// nor the total.
/// Outside those gates the scan is still what answers.
///
/// **What this bound covers.** Every path that BUILDS a `FIND` result row set
/// reads it: the on-disk `FIND symbols` scan in
/// [`ColumnarStorage::materialize_all`] — on the route that builds as it goes,
/// tested after each segment is appended, so the real peak is the budget plus
/// the one segment being materialised; on the route that chooses its page from
/// row views first, tested once over the chosen page, so the bound is reached
/// before the memory is spent rather than after — the name-ordered stream,
/// which declines to the scan rather than stream past it, the union of a
/// session's uncommitted rows into that scan, the `FIND usages` site list on
/// both backends, and the legacy in-memory backend's own scan. `FIND files` is
/// outside it and needs no bound of its own: it pages at the standard 20-row
/// `FIND` default with an honest `total`, so its response is bounded by the
/// page rather than by the workspace.
///
/// What it does not cover is the rows a scan CARRIES while it chooses which to
/// build. Those are row views, a thirty-third of the size, and
/// [`DEFAULT_FIND_MAX_VIEWS`] bounds them out of the same 2 GiB.
const DEFAULT_FIND_MAX_ROWS: usize = FIND_ROW_BUDGET_BYTES / FIND_BYTES_PER_ROW;

/// Row budget for [`ColumnarStorage::materialize_all`], read per query.
/// `FORGEQL_FIND_MAX_ROWS` overrides the default; `0` disables the bound.
pub(in crate::storage) fn find_max_rows() -> usize {
    match std::env::var("FORGEQL_FIND_MAX_ROWS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_FIND_MAX_ROWS,
    }
}

/// The cost of one [`RowView`] — the whole cost, not a working figure.
///
/// A view owns nothing: its name points into the segment's mapping and every
/// other field is a reference, an index or a line number. So unlike
/// [`FIND_BYTES_PER_ROW`], which has to guess at the heap a `String` and a
/// `HashMap` reach for and is re-measured when the row grows a field, this one
/// is read straight off the type and cannot drift from it.
///
/// Measured at 48 bytes on this target, which is about a thirty-third of a
/// built row; `a_view_costs_what_the_scan_bound_prices_it_at` fails if that
/// changes, so the figure quoted here and in the agent docs is checked rather
/// than remembered.
const FIND_BYTES_PER_VIEW: usize = std::mem::size_of::<RowView<'static>>();

/// Default hard bound on the row VIEWS one `FIND` carries at once.
///
/// The same memory budget as [`DEFAULT_FIND_MAX_ROWS`], divided by what a
/// carried row costs rather than what a built one costs — about 44.7 million
/// against 1.34 million. That is the whole point of choosing a page before
/// building it: the scan is bounded by what it holds, which is now a view, and
/// the built bound is left to bound what is delivered.
///
/// `FORGEQL_FIND_MAX_ROWS` overrides both, in rows, because both count rows —
/// one the rows a scan carries, the other the rows it builds from them. The
/// defaults differ because the bytes per row differ.
const DEFAULT_FIND_MAX_VIEWS: usize = FIND_ROW_BUDGET_BYTES / FIND_BYTES_PER_VIEW;

/// View budget for [`ColumnarStorage::page_from_row_views`], read per query.
/// `FORGEQL_FIND_MAX_ROWS` overrides the default; `0` disables the bound.
fn find_max_views() -> usize {
    match std::env::var("FORGEQL_FIND_MAX_ROWS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_FIND_MAX_VIEWS,
    }
}

/// The refusal raised when the views a scan carries would not fit.
///
/// Checked between segments, like the built-row bound, so the real peak is the
/// bound plus one segment's views. It is reached by a page so large that the
/// running trim's own window outgrows the budget, or by a single segment
/// holding more rows than the budget on its own — the built bound cannot say
/// either, because on this route no row has been built to count.
fn carried_row_budget_exceeded(max_views: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "FIND carried more than {max_views} matching rows before ORDER/LIMIT — \
         about {} GiB of row views, which is the budget in force. A row view is \
         {FIND_BYTES_PER_VIEW} bytes against about {FIND_BYTES_PER_ROW} for the \
         row it would build, so this bound is roughly thirty times looser than \
         the one on rows built for delivery, and a scan reaching it is asking \
         for a page no host will hold either. Narrow the scan — IN 'path/**', \
         or a more selective WHERE — or ask for a smaller page: this route \
         carries LIMIT + OFFSET rows, so the bound moves with what you asked \
         for. Every segment is still read, tested and counted. \
         FORGEQL_FIND_MAX_ROWS overrides the bound in rows; 0 disables it.",
        max_views.saturating_mul(FIND_BYTES_PER_VIEW) / (1024 * 1024 * 1024)
    )
}

/// Working figure for the cost of one candidate row ID in the per-segment
/// bitmaps Stage 2 hands to materialisation.
///
/// Taken as a bare `u32`, which is the worst case and deliberately so: a
/// Roaring bitmap stores a dense run in about a bit per row and a sparse one
/// in two bytes, so a real candidate set costs less than this and often far
/// less. A bound computed from the best case would not stop a scan before the
/// host ran out of memory, which is the only reason the bound exists.
const FIND_BYTES_PER_ROW_ID: usize = 4;

/// Default hard bound on the candidate row IDs one `FIND` holds at once.
///
/// The same memory budget as [`DEFAULT_FIND_MAX_ROWS`], divided by a very much
/// smaller per-item cost — which is the whole reason the two are counted
/// separately. A row ID is 4 bytes and a materialised row is about 1,600, so
/// the same 2 GiB buys roughly 537 million of the first and 1.34 million of
/// the second. Bounding row IDs by the row figure would refuse the scans that
/// choosing rows before building them exists to make possible.
const DEFAULT_FIND_MAX_ROW_IDS: usize = FIND_ROW_BUDGET_BYTES / FIND_BYTES_PER_ROW_ID;

/// Row-ID budget for [`ColumnarStorage::materialize_all`], read per query.
/// `FORGEQL_FIND_MAX_ROW_IDS` overrides the default; `0` disables the bound.
fn find_max_row_ids() -> usize {
    match std::env::var("FORGEQL_FIND_MAX_ROW_IDS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_FIND_MAX_ROW_IDS,
    }
}

/// The refusal raised when the candidate set alone would not fit.
///
/// Checked once, over the whole partitioned candidate set, before a row is
/// built. It is a different bound from the row budget and not a substitute for
/// it: this one is about the row IDs a scan holds, that one about the rows it
/// materialises from them, and a query can be inside either one and past the
/// other.
fn row_id_budget_exceeded(candidates: u64, max_row_ids: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "FIND matched {candidates} candidate rows, past the {max_row_ids} this \
         index will hold row IDs for — about {} GiB of candidate set before a \
         single row is built. Narrow the scan: IN 'path/**' prunes whole \
         segments before they are partitioned, and a more selective WHERE \
         prunes rows inside them. A LIMIT does not help here — it bounds what \
         is delivered and built, never what is searched, so the candidate set \
         is the same size with one as without. FORGEQL_FIND_MAX_ROW_IDS \
         overrides the bound in row IDs; 0 disables it.",
        max_row_ids.saturating_mul(FIND_BYTES_PER_ROW_ID) / (1024 * 1024 * 1024)
    )
}

/// How many rows a chooser that sheds on rank retains once it fires.
///
/// Written once because three callers ask it — the page cut from row views, the
/// running trim over built rows, and the per-segment bounded choice — and a
/// page chosen by two different retained sizes is two different pages.
const fn topk_keep(k: usize) -> usize {
    let over = k.saturating_mul(TOPK_OVER_FETCH / 2);
    if over > k { over } else { k }
}

/// The refusal raised when a `FIND` would spend more than the row budget on
/// rows it BUILDS.
///
/// One check per route. On the scan that builds as it goes it sits between
/// segments in the accumulation loop, counting rows already built. On the
/// route that chooses a page from row views it sits once, over the chosen
/// page, before a row of it is built — the same bound in the same currency,
/// reached before the memory is spent rather than after. Choosing a segment's
/// contribution from its columns adds neither: a segment admitted to that
/// route contributes at most `2k` rows, so it can only make the count this
/// bound watches smaller.
///
/// It bounds the rows a scan **builds**. What a scan **carries** while it
/// chooses them has its own, much looser bound in
/// [`carried_row_budget_exceeded`], and the candidate row IDs it holds to
/// carry them from a looser one still in [`row_id_budget_exceeded`] — a
/// candidate costs four bytes, a carried row forty-eight, and a materialised
/// row about four hundred times the first.
pub(in crate::storage) fn row_budget_exceeded(max_rows: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "FIND would build more than {max_rows} result rows — about {} GiB \
         of them, which is the budget in force. Narrow the scan — IN \
         'path/**', or a more selective WHERE. A LIMIT usually completes \
         instead, at any k, with or without an ORDER BY and with or without \
         an OFFSET, because the page is chosen over row views read from the \
         segment columns and only LIMIT + OFFSET rows are ever built: every \
         segment is still read, tested and counted, so the answer is the \
         true page, a full one, and a total counting every row that \
         matched. That route wants no GROUP BY and no HAVING — a HAVING \
         runs after the page is cut — no two segments of this index built \
         from one source path, and every field the WHERE and the ORDER BY \
         name answerable from a segment's own columns, which usages, \
         node_id and count never are and which no regex operator is, \
         whatever field it names. Where it declines, a running top-K trim \
         over built rows still holds the working set to a few thousand rows \
         for k <= 1000 with no OFFSET, no GROUP BY and no HAVING, and where \
         neither applies this refusal is what is left — as it is where two \
         segments of this index were built from one source path and a \
         segment collapsing its own duplicates is no longer the whole \
         collapse. One shape is counted differently: ORDER BY name, with at \
         most an fql_kind equality beside it, no IN or EXCLUDE and no \
         uncommitted edits in the session, is what the name index streams k \
         rows at a time, and that route reports k as its total — it hands \
         the query back to the full scan whenever its page would be short, \
         so the same query is sometimes counted honestly and sometimes not. \
         FORGEQL_FIND_MAX_ROWS overrides the bound in rows; 0 disables it.",
        max_rows.saturating_mul(FIND_BYTES_PER_ROW) / (1024 * 1024 * 1024)
    )
}

/// The same budget refusing a `FIND usages` whose site list outgrew it.
///
/// Sites, not rows, are what accumulate on that path — one `(path, line,
/// role)` per occurrence, gathered from the postings and from reading the
/// files — and every site becomes exactly one result row, so the row budget
/// is the honest bound for them too. The bound is checked between matching
/// tiers, so the peak can overshoot it by one tier's finds.
pub(in crate::storage) fn usages_budget_exceeded(sites: usize, max_rows: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "FIND usages holds {sites} occurrence sites, past the {max_rows}-row \
         budget — about {} GiB of result rows. Narrow the reading: IN 'path/**' \
         and EXCLUDE prune files before they are read, and a more specific name \
         matches fewer sites. A LIMIT does not help here — it selects whole \
         files out of a computed answer, never bounding what is searched. The \
         bound is checked between matching tiers, so the peak can overshoot it \
         by one tier's finds. FORGEQL_FIND_MAX_ROWS overrides it in rows; 0 \
         disables it.",
        max_rows.saturating_mul(FIND_BYTES_PER_ROW) / (1024 * 1024 * 1024)
    )
}

/// Whether this predicate has to wait for a built row on this segment.
///
/// The one definition of "late", so the split below and the whole-query gate
/// that asks whether a scan may carry views at all cannot answer it
/// differently. A predicate waits when [`SegmentReader::answers_field`]
/// declines its field — `usages`, which is stamped from the workspace overlay
/// only after materialisation; `node_id`, which is built during it; `count`,
/// which GROUP BY assigns later still; a struct-backed name this segment
/// shadows with an enrichment column; or a field no column of this segment
/// holds — and also when the operator is a regex, because
/// `apply_where_predicates` compiles the pattern once for a whole batch while
/// a per-row evaluation would recompile it for every row.
fn predicate_waits_for_a_built_row(
    seg: &SegmentReader,
    predicate: &crate::ir::Predicate,
    has_path: bool,
) -> bool {
    matches!(predicate.op, CompareOp::Matches | CompareOp::NotMatches)
        || !seg.answers_field(crate::field_tiers::canonical(&predicate.field), has_path)
}

/// Split a segment's residual `WHERE` into the predicates a row view answers
/// from the columns and the ones that have to wait for materialised rows.
///
/// The split is by construction, so every predicate is in exactly one half and
/// none can be lost between them. Which half a predicate lands in is
/// [`predicate_waits_for_a_built_row`] and nothing else, so a caller that
/// gates on that function ahead of the scan is asking the same question this
/// answers row by row.
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
        if predicate_waits_for_a_built_row(seg, predicate, has_path) {
            late.push(predicate.clone());
        } else {
            early.push((crate::field_tiers::canonical(&predicate.field), predicate));
        }
    }
    (early, late)
}

/// Whether an ordering can be applied to row views at all.
///
/// A comparator consults more than the ORDER BY field: every
/// [`crate::filter::ORDER_TIE_BREAKERS`] entry has to rank the same on a row
/// view as on the row that view would build, or the ranking is not the same
/// ranking. [`ranks_field_like_a_built_row`] is what decides that for one
/// field, and the tie-breakers are all outside its exclusion set, so in
/// practice this is a question about the ORDER BY field alone — asked over the
/// whole list anyway, so that adding a tie-breaker a view cannot answer cannot
/// slip past.
///
/// It is a property of the query and not of any segment. It used to be per
/// segment, because a view withheld a struct-backed name an enrichment column
/// shadowed while the built row still answered it, and one such segment among
/// thirteen was enough to switch the route off for a whole workspace. A view
/// now reads what the built row reads, so nothing about a particular segment
/// bears on this.
///
/// It says nothing about the residual `WHERE`; that is a property of the rows a
/// segment matched rather than of the ordering, and the caller checks it.
fn ordering_travels_on_views(order_field: &str) -> bool {
    std::iter::once(order_field)
        .chain(crate::filter::ORDER_TIE_BREAKERS.iter().copied())
        .all(ranks_field_like_a_built_row)
}

/// The field a page is ranked by: the ORDER BY field where one is written,
/// `name` where none is.
///
/// With no ORDER BY the comparator is the tie-breakers alone and `name` is the
/// first of them, so naming it here asks exactly what the ranking will do.
/// Written once because two callers ask it — the segment that chooses its own
/// contribution, and the whole-query gate that decides whether a scan may rank
/// views at all — and a gate keyed on a different field from the ranking is
/// not a gate.
fn order_field_of(clauses: &Clauses) -> &str {
    clauses.order_by.as_ref().map_or("name", |order_by| {
        crate::field_tiers::canonical(&order_by.field)
    })
}

/// Whether every row of this segment can travel as a [`RowView`] under these
/// clauses — collapsed, ranked and paged without ever being built.
///
/// The one thing left that is a property of a *segment*: every residual
/// predicate has to be answerable from this segment's columns. One that is not
/// would leave rows the query excludes in the set being ranked, and a page cut
/// from that set is short by however many the filter over built rows then
/// removes, with nothing in the reply to say so.
///
/// The collapse needs no admission of its own. Its key is
/// [`RowView::collapse_key`] — `name`, `fql_kind`, `line` — three fixed columns
/// every segment has, read by the view exactly as the built row reads them.
/// Whether the ORDER BY can travel is a property of the query, not of any
/// segment: see [`ordering_travels_on_views`].
fn segment_rows_can_travel_as_views(
    seg: &SegmentReader,
    predicates: &[crate::ir::Predicate],
) -> bool {
    !predicates
        .iter()
        .any(|predicate| predicate_waits_for_a_built_row(seg, predicate, true))
}

/// Collapse the rows a segment holds more than once, without building any.
///
/// One file's row table can already carry two rows agreeing on `(name,
/// fql_kind, path, line)`, which is the key the pass over built rows collapses
/// on. Doing it here instead lets a bounded page choose among rows that are all
/// going to survive, so it can no longer discard a row that belonged in the
/// answer in favour of one that was about to collapse into another.
///
/// It cannot decline. The key is [`RowView::collapse_key`] — `name`,
/// `fql_kind`, `line` — read from three fixed columns every segment has, by a
/// view that resolves them exactly as the row it would build resolves them.
/// `the_column_key_and_the_built_row_key_agree_on_every_pair` pins that on a
/// segment whose enrichment column shadows one of the three, which is the case
/// that used to make this return nothing.
///
/// The hash only groups candidates; membership is decided by comparing the key
/// fields, so a collision costs a comparison and never a row.
///
/// Views are the input and the output because the caller has them already or is
/// about to make them: a view resolves the name once, and the key reads that
/// name, so keying a segment through views costs one read of each name rather
/// than one per key comparison.
fn dedupe_views_of_segment(views: Vec<RowView<'_>>) -> Vec<RowView<'_>> {
    let mut kept: Vec<RowView<'_>> = Vec::with_capacity(views.len());
    let mut under_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for view in views {
        let hash = view.collapse_key_hash();
        let seen = under_hash.entry(hash).or_default();
        if seen
            .iter()
            .any(|&earlier| kept[earlier].collapse_key() == view.collapse_key())
        {
            continue;
        }
        seen.push(kept.len());
        kept.push(view);
    }
    kept
}

/// One segment's rows narrowed to what survives everything testable before a
/// row is built, together with what is left to test once they are.
///
/// The two halves travel together because they are decided together:
/// `split_seg_predicates` puts every residual predicate on exactly one side,
/// the ones a row view can answer have already been applied to `rows`, and
/// `late` is precisely the remainder. A caller that keeps one and forgets the
/// other does not fail loudly — it answers with rows the query excluded.
struct NarrowedSegment<'a> {
    /// The segment the rows belong to.
    seg: &'a SegmentReader,
    /// The segment's source path as the caller spells it — relative, so that
    /// IN/EXCLUDE glob matching in `apply_clauses` sees the same paths the
    /// legacy backend stores. Do NOT join it with the worktree root.
    source_path: &'a Path,
    /// Local row indices that survived the postings prefilter and every
    /// predicate a row view could answer.
    rows: RoaringBitmap,
    /// Predicates no row view could answer, still to run on the built rows.
    late: Vec<crate::ir::Predicate>,
}

impl<'a> NarrowedSegment<'a> {
    /// A view of every row that survived, in row order.
    ///
    /// Only sound where nothing is left in `late`: a view carries no answer to
    /// a predicate still to run, so a caller ranking or paging these while
    /// `late` is non-empty is ranking rows the query excludes.
    fn views(&self) -> Vec<RowView<'a>> {
        self.rows
            .iter()
            .map(|row| RowView::of(self.seg, Some(self.source_path), row))
            .collect()
    }
}

impl ColumnarStorage {
    // ─────────────────────────────────────────────────────────────────────
    // Phase 9 — GROUP BY / ORDER BY fast-path methods
    // ─────────────────────────────────────────────────────────────────────

    /// Fast-path for `FIND symbols GROUP BY file ORDER BY count DESC LIMIT N`
    /// with no `WHERE`, or one built entirely from `fql_kind = '<value>'`.
    ///
    /// Sums `SegmentMeta.dedup_row_count` per source path in O(segments) time —
    /// or, under a predicate, each segment's share of the canonical kind bitmap.
    /// No individual symbol rows are materialised.
    ///
    /// `None` hands the query to the scan. A predicate no stored structure
    /// settles exactly cannot be counted here, because nothing on this path ever
    /// reads a row to test one against.
    pub(super) fn fast_group_by_file(&self, clauses: &Clauses) -> Option<FindPage> {
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();

        let path_floor = clauses
            .in_glob
            .as_deref()
            .and_then(glob_to_path_prefix)
            .map(|prefix| {
                let row_range = self.overlay().path_row_range(prefix);
                row_range.collect::<RoaringBitmap>()
            });

        // Counted, so every predicate must be one a stored structure settles
        // exactly. `prefilter_global` SKIPS a predicate no tier can serve rather
        // than failing, and this path clears the residual `WHERE` before
        // delivering — so an unadmitted predicate would neither narrow the
        // candidates nor be tested against a row, and the count reported would be
        // the whole segment's. `group_by_file_fast_path_eligible` refuses those
        // shapes; this refuses them again, because the two sit far apart in the
        // file and only this one is next to the counting.
        let candidates = if clauses.where_predicates.is_empty() {
            None
        } else {
            if !clauses.where_predicates.iter().all(counts_exactly) {
                return None;
            }
            Some(self.prefilter_global(clauses, path_floor))
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
        // HAVING, ORDER BY, LIMIT (skip GROUP BY — already grouped; IN/EXCLUDE already applied
        // during segment iteration so strip them here to avoid re-filtering by path.
        // Also clear where_predicates: each was applied exactly to the candidate
        // bitmap above, and none of them names a field a group row carries.)
        let mut no_group = clauses.clone();
        no_group.group_by = None;
        no_group.in_glob = None;
        no_group.exclude_globs.clear();
        no_group.where_predicates.clear();
        let total = apply_clauses_counted(&mut results, &no_group);
        Some(FindPage::of(results, total))
    }

    /// Fast-path for `FIND symbols GROUP BY fql_kind ORDER BY count DESC LIMIT N`
    /// when there are no WHERE predicates.
    ///
    /// Deserialises each kind bitmap and reads its cardinality in O(n_kinds) time.
    /// For IN-glob queries, intersects each kind bitmap with the path range bitmap.
    ///
    /// The bitmaps hold only the rows that carry a kind, so the rows carrying
    /// none are counted by subtracting the rest from the canonical total the
    /// selected segments declare — the group the scan keys by the empty string.
    /// `None` hands the query to the scan, which is what a subtraction that
    /// cannot hold (or a kind table that cannot be read) means: the two sides are
    /// not describing the same rows, and a group count is not worth guessing.
    pub(super) fn fast_group_by_kind(&self, clauses: &Clauses) -> Option<FindPage> {
        // `passes_resolve_glob` decides per source path and a segment is one
        // source path, so IN/EXCLUDE select whole segments: the canonical total
        // under them is the sum of the stored per-segment counts, and the mask
        // that narrows the bitmaps is a union of whole segment row ranges.
        let path_filtered = clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty();
        let mut mask = RoaringBitmap::new();
        let mut canonical_rows: usize = 0;
        for (idx, meta) in self.overlay().segments().iter().enumerate() {
            if path_filtered && !passes_resolve_glob(&meta.source_path, clauses) {
                continue;
            }
            canonical_rows = canonical_rows.saturating_add(meta.dedup_row_count as usize);
            if path_filtered {
                let _ = mask.insert_range(self.overlay().segment_row_range(idx));
            }
        }

        let kind_counts = self
            .overlay()
            .kind_global_counts(path_filtered.then_some(&mask))?;
        let described: usize = kind_counts.iter().map(|(_, n)| *n).sum();
        let mut results: Vec<SymbolMatch> = kind_counts
            .into_iter()
            .map(|(kind, count)| SymbolMatch {
                name: kind.clone(),
                fql_kind: Some(kind),
                count: Some(count),
                ..SymbolMatch::default()
            })
            .collect();

        // A row of a selected segment either carries a kind or it does not, and
        // `step5_build_kind_postings` skips the empty one, so the bitmaps
        // partition only the rows that do. The rest are one group keyed by the
        // empty string — what the grouping pass makes of a row whose field
        // resolves to nothing — and its size is the canonical total less the rows
        // the kinds account for. A remainder that cannot exist means the two
        // sides are not describing the same rows, so the query goes to the scan
        // rather than carry a count derived from a contradiction.
        let unaccounted = canonical_rows.checked_sub(described)?;
        if unaccounted > 0 {
            results.push(SymbolMatch {
                count: Some(unaccounted),
                ..SymbolMatch::default()
            });
        }

        // IN/EXCLUDE already applied via path_mask — strip to avoid re-filtering.
        // HAVING is left on: `apply_clauses_counted` runs it, and the gate has
        // already refused any that names a field these rows do not carry.
        let mut no_group = clauses.clone();
        no_group.group_by = None;
        no_group.in_glob = None;
        no_group.exclude_globs.clear();
        let total = apply_clauses_counted(&mut results, &no_group);
        Some(FindPage::of(results, total))
    }

    /// `GROUP BY <posted enrichment field>` answered from the overlay's
    /// `field=value` key table: one stored bitmap cardinality per value, and
    /// not one result row built for any of the rows counted.
    ///
    /// The counts are the pipeline's collapsed counts, not an approximation of
    /// them: `Overlay::enrichment_value_counts` reads bitmaps drawn from each
    /// segment's canonical row set, which is the same collapse the scan's
    /// dedupe pass performs. The caller has already refused the three shapes
    /// where that equivalence breaks on the query's side — a `WHERE`, whose
    /// subset a cardinality counted over whole segments cannot be narrowed to;
    /// a session with uncommitted rows the table cannot know about; and an
    /// index holding two segments built from one source path, where a segment
    /// collapsing its own duplicates is not the whole collapse — and this
    /// function refuses the two on the index's side: a field the overlay pruned
    /// for carrying more distinct values than its budget allows, and a selected
    /// segment that stores the column without posting it.
    ///
    /// What is NOT identical is the order of a page no clause decides. A group
    /// row built here is named by its own value, where the scan's is the first
    /// row of the group and is named by that row, so with no `ORDER BY` — or on
    /// a tie under `ORDER BY count` — the same groups with the same counts come
    /// back in a different sequence, and a `LIMIT` then cuts a different page
    /// out of them. `total` and every count are the same either way.
    ///
    /// `None` for either index-side refusal, for a key table that could not be
    /// read whole, and for counts that do not fit inside the canonical rows
    /// they are drawn from. Every one of them falls through to the scan, which
    /// is slower and still right.
    pub(super) fn fast_group_by_enrichment(
        &self,
        field: &str,
        clauses: &Clauses,
    ) -> Option<FindPage> {
        // Pruned at build: past its value budget the overlay drops every
        // `field=` key it had collected and writes no more, so the table would
        // report no groups at all rather than fewer.
        if !self.overlay().has_enrichment_field(field) {
            return None;
        }

        // `passes_resolve_glob` decides per source path and a segment is one
        // source path, so IN/EXCLUDE select whole segments: the canonical total
        // under them is the sum of the stored per-segment counts, and the mask
        // that narrows the bitmaps is a union of whole segment row ranges.
        let path_filtered = clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty();
        let mut mask = RoaringBitmap::new();
        let mut canonical_rows: usize = 0;
        for (idx, meta) in self.overlay().segments().iter().enumerate() {
            if path_filtered && !passes_resolve_glob(&meta.source_path, clauses) {
                continue;
            }
            // A selected segment that stores the column and posted none of its
            // values holds rows no key of the table counts, and counting from
            // the keys would report every one of them as carrying no value at
            // all. Only the SELECTED segments are asked: the rows of the rest
            // are in neither the mask nor the total, so what they store cannot
            // change this answer. A segment whose reader is missing is asked
            // too, and answers by declining — an unread segment is exactly the
            // one whose rows nothing here can account for.
            let seg = self.segments().get(idx)?;
            if segment_posts_partially(seg, field) {
                return None;
            }
            canonical_rows = canonical_rows.saturating_add(meta.dedup_row_count as usize);
            if path_filtered {
                let _ = mask.insert_range(self.overlay().segment_row_range(idx));
            }
        }

        let counts = self
            .overlay()
            .enrichment_value_counts(field, path_filtered.then_some(&mask))?;
        let described: usize = counts.iter().map(|(_, n)| *n).sum();

        let mut results: Vec<SymbolMatch> = counts
            .into_iter()
            .map(|(value, count)| {
                let mut fields = HashMap::new();
                let _ = fields.insert(field.to_owned(), value.clone());
                SymbolMatch {
                    name: value,
                    fields,
                    count: Some(count),
                    ..SymbolMatch::default()
                }
            })
            .collect();

        // The rows the field says nothing about are one group keyed by the
        // empty string — what the grouping pass makes of a row whose field
        // resolves to nothing, and a group the key table has no key for. The
        // bitmaps partition the rows that DO carry a value out of the same
        // canonical rows `dedup_row_count` counts, so the remainder is exactly
        // that group's size. A remainder that cannot exist means the two sides
        // disagree about which rows they are describing, and a count is not
        // worth guessing: hand the query to the scan.
        let unaccounted = canonical_rows.checked_sub(described)?;
        if unaccounted > 0 {
            results.push(SymbolMatch {
                count: Some(unaccounted),
                ..SymbolMatch::default()
            });
        }

        // IN/EXCLUDE were applied to the segments above; left on, they would
        // re-filter rows that carry no path and drop every group.
        let mut no_group = clauses.clone();
        no_group.group_by = None;
        no_group.in_glob = None;
        no_group.exclude_globs.clear();
        let total = apply_clauses_counted(&mut results, &no_group);
        Some(FindPage::of(results, total))
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
            if segment_posts_partially(seg, field) {
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
            if seg.has_name_prefix_index() {
                any_had_index = true;
                match seg.name_prefix_rows(prefix) {
                    Ok(Some(local_bm)) => {
                        for local_row in local_bm {
                            let _ = result.insert(seg_base + local_row);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // An index that will not decode cannot prune: keep
                        // every row of the segment a candidate, as for a
                        // segment with no index at all.
                        tracing::warn!(
                            segment = %seg.content_id_hex(),
                            "name_prefix index unreadable, not pruning: {e:#}"
                        );
                        for local_row in 0..row_count {
                            let _ = result.insert(seg_base + local_row);
                        }
                    }
                }
            } else {
                // No prefix index — include all rows from this segment.
                for local_row in 0..row_count {
                    let _ = result.insert(seg_base + local_row);
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

    /// Stage 3 — turn the surviving row IDs into result rows.
    ///
    /// Two routes answer this, and they answer it identically.
    ///
    /// Where every row the query selects can be filtered, keyed and ranked from
    /// its segment's own columns, the whole scan travels as row views and only
    /// the page is ever built ([`Self::page_from_row_views`]). Where any row
    /// cannot — a predicate that has to wait for a built row, an ordering by a
    /// field only a built row carries — the scan builds as it goes
    /// ([`Self::page_from_built_rows`]), a segment at a time, with the running
    /// trim holding the working set down. [`Self::view_page_bound`] and
    /// [`Self::every_row_can_travel_as_a_view`] are the whole of that choice.
    ///
    /// The two share the comparator, the collapse key, the over-fetch constants
    /// and the shed accounting, so which one answers changes the work and never
    /// the rows.
    ///
    /// **Every segment is read on both.** Nothing here stops the loop: a
    /// `LIMIT` bounds what is delivered, and therefore what is built, never
    /// what is searched. That is what makes the second return value meaningful
    /// — how many rows were shed on rank alone. Those rows matched and were
    /// distinct, so they belong to the size of the answer even though no page
    /// can hold them; without the count the caller would report the retained
    /// window as the `total`, which is neither the page nor the answer.
    ///
    /// The collapse before the shedding is what makes that count mean anything.
    /// Two rows carry the same `(name, fql_kind, path, line)` only if they
    /// carry the same path, and a path belongs to one segment unless two
    /// segments were built from it — so where that does not happen the
    /// per-segment pass IS the whole collapse, nothing shed was about to merge
    /// into a survivor, and no row is counted twice. Where it does happen
    /// neither route sheds at all rather than shed on an incomplete collapse,
    /// and such a scan is refused by the row budget where it would once have
    /// been answered wrongly.
    pub(super) fn materialize_all(
        &self,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
    ) -> anyhow::Result<(Vec<SymbolMatch>, usize)> {
        // The candidate set is bounded before anything is built from it, and
        // before either route is chosen. On a scan the working set below keeps
        // to a few thousand rows or views, the row IDs are what the query
        // actually holds, and nothing else was watching them.
        let max_row_ids = find_max_row_ids();
        let candidates: u64 = by_segment.values().map(RoaringBitmap::len).sum();
        if candidates > u64::try_from(max_row_ids).unwrap_or(u64::MAX) {
            return Err(row_id_budget_exceeded(candidates, max_row_ids));
        }

        // Residual WHERE runs per segment so non-matching rows never
        // accumulate.  `count` is excluded: it is only assigned by GROUP BY
        // in Stage 5, so no materialised row carries it yet.
        let seg_predicates: Vec<crate::ir::Predicate> = clauses
            .where_predicates
            .iter()
            .filter(|p| p.field != "count")
            .cloned()
            .collect();

        if let Some(need) = self.view_page_bound(clauses)
            && ordering_travels_on_views(order_field_of(clauses))
            && self.every_row_can_travel_as_a_view(by_segment, &seg_predicates)
            && let Some(page) =
                self.page_from_row_views(by_segment, clauses, &seg_predicates, need)?
        {
            return Ok(page);
        }

        self.page_from_built_rows(by_segment, clauses, &seg_predicates)
    }

    /// How many rows a page cut from row views has to carry, or `None` where a
    /// page cannot be chosen before its rows are built.
    ///
    /// `LIMIT + OFFSET`, because the rows `OFFSET` skips are still rows the
    /// page needs: the skip runs downstream, after a session's uncommitted rows
    /// have been merged in, and a dirty row landing ahead of the window shifts
    /// which persistent rows fall inside it. Keeping `LIMIT + OFFSET` of them
    /// is enough whatever the overlay adds — no persistent row ranked past that
    /// can reach the page, since `LIMIT + OFFSET` better ones already precede
    /// it — and keeping fewer would make a dirty session answer a query
    /// differently from a clean one on the same bytes.
    ///
    /// `None` where `GROUP BY` or `HAVING` is written: `GROUP BY` assigns a
    /// `count` no view can carry, and `HAVING` runs after the page is cut, so
    /// anything shed before it is a row that might have qualified. `None` too
    /// with no `LIMIT` at all — there is no page to cut to — and where two
    /// segments were built from one source path, because there a segment
    /// collapsing its own duplicates is no longer the whole collapse and a row
    /// shed on rank might have been about to merge into a survivor.
    ///
    /// It is deliberately not [`Self::trim_budget`], which carries two further
    /// conditions — and they are not the same kind of condition, so they are
    /// not dropped for the same reason.
    ///
    /// `k <= TOPK_THRESHOLD` is a COST guard: past it the running trim's window
    /// is no longer cheap to hold in built rows. This route holds views, and has
    /// its own bound on how many ([`carried_row_budget_exceeded`]), so it does
    /// not need that one.
    ///
    /// `offset == 0` is a CORRECTNESS condition, and dropping it would be a bug
    /// on the trim. That trim retains `topk_keep(limit)`, which simply does not
    /// contain the rows a page starting at `OFFSET` needs. This route is sound
    /// only because it bounds at `LIMIT + OFFSET` instead, as the paragraph
    /// above argues — so nobody should read this as licence to relax
    /// [`Self::topk_trim_for`], which would silently drop rows.
    fn view_page_bound(&self, clauses: &Clauses) -> Option<usize> {
        if !self.per_segment_collapse_is_whole() {
            return None;
        }
        Self::view_page_bound_for(clauses)
    }

    /// [`Self::view_page_bound`] asking only about the clauses.
    ///
    /// Split for the same reason [`Self::topk_trim_for`] is: the workspace
    /// condition belongs to the caller that owns the overlay, and a callee
    /// reaching for this one directly would cut a page under exactly the
    /// workspace shape the other half exists to exclude.
    fn view_page_bound_for(clauses: &Clauses) -> Option<usize> {
        if clauses.group_by.is_some() || !crate::filter::no_having_after_paging(clauses) {
            return None;
        }
        clauses
            .limit?
            .checked_add(clauses.offset.unwrap_or(0))
            .filter(|need| *need > 0)
    }

    /// Whether every segment this query selects can hand it row views.
    ///
    /// Asked once, over the whole candidate set, and answered from the segment
    /// headers alone — no row is read to decide it. The set it enumerates is
    /// exactly the set the scan will read: `by_segment` after Stage 2 has
    /// pruned it, which is where path globs, dirty shadows and zone maps have
    /// already removed whatever cannot contribute. A segment missing a reader
    /// contributes nothing to either route and so cannot close the gate.
    ///
    /// Only the residual `WHERE` is left to ask about here. Whether the
    /// ordering can travel is a property of the query
    /// ([`ordering_travels_on_views`]) and the collapse can always travel, so
    /// this is the whole of what a *segment* still decides.
    ///
    /// It is a whole-query question because the accumulated working set is a
    /// whole-query object: one segment that cannot answer a predicate would put
    /// rows the query excludes into a window everything else is ranked against.
    /// [`Self::topk_rows_of_segment`] asks the same question per segment, which
    /// is what still narrows the other route where this one declines.
    fn every_row_can_travel_as_a_view(
        &self,
        by_segment: &HashMap<u32, RoaringBitmap>,
        seg_predicates: &[crate::ir::Predicate],
    ) -> bool {
        by_segment.keys().all(|&seg_idx| {
            self.segments()
                .get(seg_idx as usize)
                .is_none_or(|seg| segment_rows_can_travel_as_views(seg, seg_predicates))
        })
    }

    /// Stage 3 over row views: choose the page from the segment columns, then
    /// build only the rows that made it.
    ///
    /// The loop is the same loop as [`Self::page_from_built_rows`] with the
    /// currency changed — same segment order, same per-segment collapse, same
    /// comparator, same over-fetch, same shed accounting — so the retained
    /// window is the same window at every step. What is not the same is that
    /// nothing is materialised inside it: a `SymbolMatch` costs about a
    /// thirty-third of its size as a view, and on a scan matching millions of
    /// rows to deliver twenty, the rows that are never built are almost all of
    /// them.
    ///
    /// Usage counts are stamped on the page rather than on every row that
    /// reached it, which is sound only because `usages` can never be a view
    /// field: [`predicate_waits_for_a_built_row`] defers any predicate naming
    /// it and [`ordering_travels_on_views`] refuses any ordering by it, so
    /// nothing before delivery on this route has looked at one.
    ///
    /// `Ok(None)` hands the query back to the built route unanswered. The gate
    /// has already promised every segment can travel, so this is a
    /// contradiction rather than a decline — but a contradiction that returns
    /// rows the query excludes is worse than one that costs a second pass.
    fn page_from_row_views(
        &self,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
        seg_predicates: &[crate::ir::Predicate],
        need: usize,
    ) -> anyhow::Result<Option<(Vec<SymbolMatch>, usize)>> {
        let max_views = find_max_views();
        let keep = topk_keep(need);
        let trim_at = need.saturating_mul(TOPK_OVER_FETCH);

        let mut kept: Vec<RowView<'_>> = Vec::new();
        // Rows that matched, were distinct, and were shed on rank rather than
        // kept. The caller adds them to `total`. This loop has no early exit,
        // because a `LIMIT` bounds what is delivered and never what is
        // searched.
        let mut shed = 0usize;
        for seg_idx in self.ordered_segments(by_segment) {
            let Some(narrowed) =
                self.narrow_one_segment(seg_idx, by_segment, clauses, seg_predicates)
            else {
                continue;
            };
            if !narrowed.late.is_empty() {
                return Ok(None);
            }
            kept.extend(dedupe_views_of_segment(narrowed.views()));

            if kept.len() > trim_at {
                let before = kept.len();
                kept = collect_top_k(std::mem::take(&mut kept), keep, |a, b| {
                    order_cmp(a, b, clauses)
                });
                shed = shed.saturating_add(before.saturating_sub(kept.len()));
            }

            // Hard memory bound on what the scan CARRIES, checked between
            // segments like the built route's bound on what it builds, so the
            // real peak is the bound plus one segment's views.
            if kept.len() > max_views {
                return Err(carried_row_budget_exceeded(max_views));
            }
        }

        // The final cut: the running trim over-fetches so it does not fire on
        // every segment, and what it leaves behind is a window, not a page.
        if kept.len() > need {
            let before = kept.len();
            kept = collect_top_k(kept, need, |a, b| order_cmp(a, b, clauses));
            shed = shed.saturating_add(before.saturating_sub(kept.len()));
        }

        // The same bound the built route enforces, in the same currency,
        // reached before the memory is spent rather than after.
        let max_rows = find_max_rows();
        if kept.len() > max_rows {
            return Err(row_budget_exceeded(max_rows));
        }

        let mut results: Vec<SymbolMatch> = Vec::with_capacity(kept.len());
        for view in &kept {
            // A view that will not build is a row that would go missing from a
            // page reported as complete; hand the query back rather than
            // deliver a short one.
            let Some(row) = view.materialize() else {
                return Ok(None);
            };
            results.push(row);
        }
        self.stamp_usage_counts_with(self.usage_stamper().as_deref(), &mut results);

        Ok(Some((results, shed)))
    }

    /// Stage 3 building as it goes: a segment at a time, with a running trim
    /// over the rows already built.
    ///
    /// What answers wherever [`Self::page_from_row_views`] cannot — a predicate
    /// that has to wait for a built row, an ordering by a field only a built
    /// row carries, a `GROUP BY`, a `HAVING`, two segments over one source
    /// path. Individual segments still choose their contribution from their
    /// columns where they can ([`Self::topk_rows_of_segment`]); the rest build
    /// what they matched.
    fn page_from_built_rows(
        &self,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
        seg_predicates: &[crate::ir::Predicate],
    ) -> anyhow::Result<(Vec<SymbolMatch>, usize)> {
        let seg_order = self.ordered_segments(by_segment);
        let topk_trim = self.trim_budget(clauses);
        let max_rows = find_max_rows();
        // One correction for the whole scan: on a session with dirty rows the
        // usage counts are stamped per segment, and fetching the table per
        // segment would re-check the dirty overlay as many times.
        let adjust = self.usage_stamper();

        let mut results = Vec::new();
        // Rows that matched, were distinct, and were shed on rank rather than
        // kept. They are the difference between the size of the answer and the
        // size of the working set, and the caller adds them to `total`. Every
        // segment is read: this loop has no early exit, because a `LIMIT`
        // bounds what is delivered and never what is searched.
        let mut shed = 0usize;
        for seg_idx in seg_order {
            let Some(narrowed) =
                self.narrow_one_segment(seg_idx, by_segment, clauses, seg_predicates)
            else {
                continue;
            };
            let (mut seg_results, seg_shed) =
                self.materialize_one_segment(&narrowed, clauses, adjust.as_deref());
            shed = shed.saturating_add(seg_shed);

            // Collapse this segment's own duplicates before anything sheds
            // a row on rank. Two rows carry the same `(name, fql_kind, path,
            // line)` only if they carry the same path, and a path belongs to
            // one segment unless two segments were built from it — so wherever
            // that does not happen, this IS the Stage 4 pass, and the trim
            // below can no longer discard a row that was going to collapse
            // into one of the survivors it lost to. The unconditional Stage 4
            // pass still runs afterwards and covers the rest.
            if seg_results.len() > 1 {
                dedupe_symbol_matches(&mut seg_results);
            }

            results.append(&mut seg_results);

            // Running top-K trim: shed rows that cannot make the final top-K.
            // Fires whenever the working set exceeds K * TOPK_OVER_FETCH.
            if let Some(k) = topk_trim {
                let trim_at = k.saturating_mul(TOPK_OVER_FETCH);
                if results.len() > trim_at {
                    let before = results.len();
                    results = collect_top_k(std::mem::take(&mut results), topk_keep(k), |a, b| {
                        order_cmp(a, b, clauses)
                    });
                    // A shed row is a distinct row that matched — the dedupe
                    // above has already run — so it belongs to the count even
                    // though it will never be delivered. Without this the
                    // reported `total` would be the size of the retained
                    // window, which is neither the page nor the answer.
                    shed = shed.saturating_add(before.saturating_sub(results.len()));
                }
            }

            // Hard memory bound: refuse to grow past the row budget instead of
            // exhausting host RAM on an unscoped scan. Checked between segments,
            // so the real peak is the budget plus one segment's rows — bounded,
            // and cheaper than testing it per row.
            if results.len() > max_rows {
                return Err(row_budget_exceeded(max_rows));
            }
        }
        Ok((results, shed))
    }

    /// The rows of one segment worth building for a bounded top-K, chosen
    /// before any of them is built.
    ///
    /// This is [`Self::page_from_row_views`] narrowed to a single segment, and
    /// it is what still narrows the built route where that one declines: a
    /// query with a predicate one segment cannot answer takes the built route
    /// whole, and every *other* segment of it can still choose its own
    /// contribution here. The ranking runs on [`RowView`]s — the stored columns
    /// read in place — with the same [`order_cmp`] that sorts the built rows,
    /// and only the survivors are handed on to be built.
    ///
    /// The threshold and the retained size are the trim's own, so the rows this
    /// sheds are rows the trim would have shed on the very next statement. The
    /// working set the loop goes on to accumulate is a *superset* of the one it
    /// accumulated before — a segment now contributes its own top `2k` where it
    /// used to contribute everything and be cut to `2k` against the segments
    /// before it — and the true top `k` is inside both, so the page does not
    /// move.
    ///
    /// The ordering leaves the partition no choice worth naming. Two rows
    /// compare *equal* here only when they agree on the ORDER BY field and on
    /// all of `name`, `line`, `path` and `fql_kind` — and those four fields
    /// are the Stage 4 duplicate-collapse key, so such rows are one row of
    /// the answer, merged before any page is cut. `select_nth_unstable_by`
    /// still partitions unstably, but every choice it makes is between rows
    /// no answer tells apart.
    ///
    /// Returns `None` — the segment contributes everything it matched —
    /// whenever the choice could differ from the trim's:
    ///
    /// - `topk_trim` is `None` — the clauses do not allow a bounded top-K, or the
    ///   workspace is one where shedding on rank is not sound. That decision
    ///   belongs to [`Self::trim_budget`] and arrives here already made; this
    ///   function must never re-derive it from `topk_trim_for`, which knows
    ///   only about the clauses;
    /// - the segment matched no more rows than the trim's own threshold, so
    ///   nothing would have been shed yet;
    /// - a residual `WHERE` still has to run against this segment's built rows,
    ///   so a row discarded on rank alone might be the one that survives it;
    /// - the ordering names a field a row view cannot answer as the row it
    ///   would build (see [`ordering_travels_on_views`]).
    ///
    /// The last of those is per query, not per segment. It used to be per
    /// segment: a view withheld a struct-backed name an enrichment column
    /// shadowed, and one such segment among thirteen was enough to switch the
    /// route off for a whole workspace. A view now reads what the built row
    /// reads, so no segment is excluded for what its columns are called.
    ///
    /// Beside the rows it returns how many it shed, which the caller adds to
    /// the answer's size. Those rows are distinct — the collapse ran first —
    /// so each one is an answer the page could not hold rather than a row that
    /// was about to merge into a survivor.
    fn topk_rows_of_segment(
        narrowed: &NarrowedSegment<'_>,
        clauses: &Clauses,
        topk_trim: Option<usize>,
    ) -> Option<(RoaringBitmap, usize)> {
        let k = topk_trim?;
        if !narrowed.late.is_empty() {
            return None;
        }
        let trim_at = k.saturating_mul(TOPK_OVER_FETCH);
        if narrowed.rows.len() <= trim_at as u64 {
            return None;
        }
        if !ordering_travels_on_views(order_field_of(clauses)) {
            return None;
        }

        // Collapse first, choose second. Choosing first would let this shed a
        // row that belonged in the answer to keep one that was about to
        // collapse into another survivor — the whole reason the pass over
        // built rows was too late to be the only one.
        let rows = dedupe_views_of_segment(narrowed.views());
        let keep = topk_keep(k);
        let shed = rows.len().saturating_sub(keep);

        Some((
            collect_top_k(rows, keep, |a, b| order_cmp(a, b, clauses))
                .iter()
                .map(RowView::row)
                .collect(),
            shed,
        ))
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

    /// True when a segment collapsing its own duplicates is the whole
    /// collapse — the condition every early stop in this pipeline needs, and
    /// the one place that decides it.
    ///
    /// Two rows carry the same `(name, fql_kind, path, line)` only if they
    /// carry the same path, and a path belongs to one segment unless two
    /// segments were built from it. The dirty overlay cannot contribute a pair
    /// either: `prune_candidate_segments` drops every segment a dirty file
    /// shadows, so a dirty row and a segment row never share a path.
    fn per_segment_collapse_is_whole(&self) -> bool {
        !self.overlay().has_duplicate_paths()
    }

    /// The running trim's budget for this query, or `None` where shedding on
    /// rank is not sound.
    ///
    /// [`Self::topk_trim_for`] answers only whether the *clauses* allow a
    /// bounded top-K. This adds the workspace condition. Discarding a row on
    /// rank is sound only while every row still in front of the chooser is a
    /// distinct answer row, and the collapse that makes that true runs per
    /// segment — so two segments built from one source path can hold a
    /// duplicate pair no per-segment collapse can see, and there nothing sheds
    /// at all.
    ///
    /// **Every place that sheds takes this value; none re-derives it.** A
    /// callee reaching for `topk_trim_for` itself would shed under exactly the
    /// workspace shape this exists to exclude, and would then report the rows
    /// it shed as answers when some of them were about to merge into a
    /// survivor — a `total` counting one answer twice.
    fn trim_budget(&self, clauses: &Clauses) -> Option<usize> {
        if !self.per_segment_collapse_is_whole() {
            return None;
        }
        Self::topk_trim_for(clauses)
    }
    /// Running top-K trim budget: set when LIMIT is small, OFFSET is zero, and
    /// both GROUP BY and HAVING are absent.  Bounds peak result memory to
    /// O(K * TOPK_OVER_FETCH) by periodically discarding rows that cannot make
    /// the final top-K.
    ///
    /// **An explicit ORDER BY is not required, because there is always an
    /// ordering.** With no ORDER BY clause `apply_ordering` still sorts by the
    /// `(name, line, path, fql_kind)` tie-breakers before it cuts the page — the same
    /// `order_cmp` this trim ranks with — so a bare `LIMIT k` asks for the k
    /// smallest rows under that ordering, not for the first k the scan
    /// happens to reach. This is what retired the segment fetch cap: that cap
    /// kept `limit + 1` rows in scan order and let the sort choose from them,
    /// so `total` was the number fetched, an `OFFSET` paged past rows that
    /// were never fetched, and raising the LIMIT surfaced rows a smaller one
    /// had not shown. The trim keeps the k best seen so far instead, reads
    /// every segment, and counts what it sheds.
    ///
    /// "Cannot make the final top-K" holds only against that ordering, so the
    /// trim is only sound while nothing else can remove a row afterwards.
    /// HAVING can, which is why it is excluded here. The Stage 4 collapse of
    /// duplicates on `(name, fql_kind, path, line)` also could, and used to:
    /// a row this discarded could turn out to have belonged in the answer once
    /// the survivors it was ranked against merged into one. That is why both
    /// this trim and [`ColumnarStorage::topk_rows_of_segment`] now collapse
    /// their input before they choose from it — see
    /// [`ColumnarStorage::materialize_all`] for why a per-segment collapse is
    /// the whole collapse, and for the one workspace shape where it is not and
    /// the trim stays off instead.
    fn topk_trim_for(clauses: &Clauses) -> Option<usize> {
        if clauses.group_by.is_none()
            && clauses.offset.unwrap_or(0) == 0
            && clauses.limit.is_some_and(|k| k <= TOPK_THRESHOLD)
            && crate::filter::no_having_after_paging(clauses)
        {
            clauses.limit
        } else {
            None
        }
    }

    /// Narrow one segment to the rows worth building, without building any.
    ///
    /// This is everything about a segment that can be decided from its stored
    /// columns: the enrichment-posting prefilter, then the residual `WHERE`
    /// split into the half a row view can answer — applied here — and the half
    /// that has to wait for a built row. Building a row is the dominant cost of
    /// a filtered scan, and a row the predicate is going to reject is a row that
    /// never needed building.
    ///
    /// Returns `None` when the segment contributes nothing: no rows selected for
    /// it, its reader or metadata missing, or nothing surviving the prefilter.
    fn narrow_one_segment(
        &self,
        seg_idx: u32,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
        seg_predicates: &[crate::ir::Predicate],
    ) -> Option<NarrowedSegment<'_>> {
        let local_rows = by_segment.get(&seg_idx)?;
        let seg: &SegmentReader = self.segments().get(seg_idx as usize)?;
        let seg_meta = self.overlay().segments().get(seg_idx as usize)?;

        // Stage 3a — narrow the local row set using per-segment enrichment
        // posting bitmaps before materialisation.  Falls back to the full local
        // set when no posting file exists for a given predicate.
        let narrowed = seg.prefilter_enrichment_postings(local_rows.clone(), clauses);
        if narrowed.is_empty() {
            return None;
        }

        // Stage 3b — test the residual WHERE against the segment's columns and
        // keep only the rows that survive it.
        //
        // `late` holds what a row view cannot answer, and it is not dropped:
        // every predicate is in exactly one of the two halves and `late` runs
        // against the built rows below, exactly as the whole set used to.
        let source_path = seg_meta.source_path.as_path();
        let (early, late) = split_seg_predicates(seg, seg_predicates, true);

        let rows: RoaringBitmap = if early.is_empty() {
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
        if rows.is_empty() {
            return None;
        }

        Some(NarrowedSegment {
            seg,
            source_path,
            rows,
            late,
        })
    }

    /// Build one narrowed segment's rows: choose which to build → row
    /// materialisation → usage-count stamping → the residual WHERE that needed
    /// a built row.
    ///
    /// Returns the rows and how many the choice shed, which the caller adds to
    /// the answer's size — a row not built is still a row that matched.
    fn materialize_one_segment(
        &self,
        narrowed: &NarrowedSegment<'_>,
        clauses: &Clauses,
        adjust: Option<&super::usage_adjust::UsageAdjust>,
    ) -> (Vec<SymbolMatch>, usize) {
        // Choose the rows worth building before building any of them, where
        // the query lets that be decided from the columns. The budget comes
        // from `trim_budget` and nowhere else, here as in the loop above.
        let chosen = Self::topk_rows_of_segment(narrowed, clauses, self.trim_budget(clauses));
        // Rows this segment matched that the choice discarded on rank. They
        // are distinct — the choice collapses before it ranks — so each
        // belongs to the count once, even though none of them was built.
        let (rows, shed) = chosen
            .as_ref()
            .map_or((&narrowed.rows, 0), |(kept, n)| (kept, *n));

        let mut seg_results = narrowed
            .seg
            .materialize_rows(rows, Some(narrowed.source_path));

        // Stamp workspace usage counts before any predicate or top-K decision:
        // the per-segment `usages_count` column is a stale always-zero legacy
        // field, so WHERE usages / ORDER BY usages must see the overlay value.
        // That staleness is also why an ordering by `usages` never takes the
        // pre-materialisation path above — there is nothing there to rank by.
        self.stamp_usage_counts_with(adjust, &mut seg_results);

        // Apply the residual WHERE per-segment so that non-matching rows are
        // dropped before they can pile up across segments — and so that what
        // the caller trims and counts is rows that matched.
        if !narrowed.late.is_empty() {
            crate::filter::apply_where_predicates(&mut seg_results, &narrowed.late);
        }

        (seg_results, shed)
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
/// Condition: `GROUP BY` on the file/path field, an empty dirty overlay (dirty
/// segments are not integrated into the overlay metadata yet), a `HAVING` and
/// `ORDER BY` naming only `count` or the grouped field, and either no `WHERE` at
/// all or one built entirely from `fql_kind = '<value>'`. That is the whole
/// admitted set: [`counts_exactly`] is the authority on the predicate and says
/// why every other tier is out, and [`only_the_group_row_is_read`] on the two
/// aggregate clauses.
///
/// With no `WHERE` a group's count is its segment's own `dedup_row_count`; with
/// one it is that segment's share of a canonical-intersected kind bitmap.
/// Neither counts a row the answer does not hold. Everything else — a `name`
/// predicate, an enrichment predicate, a `HAVING` or `ORDER BY` on a field a
/// group row does not carry — is answered by the scan, which reads each row.
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
    if !only_the_group_row_is_read(clauses, "path") {
        return false;
    }
    clauses.where_predicates.iter().all(counts_exactly)
}

/// The one predicate a counted grouping may narrow by: `fql_kind = '<value>'`.
///
/// Its postings are the only tier that both verifies and deduplicates:
/// `step5_build_kind_postings` intersects them with each segment's canonical row
/// set, so a row sits in exactly one kind bitmap and a cardinality is a count of
/// rows. Every other tier only proposes. The trigram index over-generates by
/// construction, and the name FST carries a segment's raw rows because
/// `step6_build_name_fst` does not canonical-intersect the way step 5 does — so
/// a plain `name =` proposes the intra-segment duplicates the scan collapses.
/// Nothing at query time can settle either of those, because the canonical row
/// set is stored as a per-segment COUNT and not as a set, so there is nothing to
/// intersect a candidate against; the shapes are handed to the scan, which
/// decides each row by reading it. Making `name =` exact instead means
/// canonical-intersecting the name postings at overlay build, which changes
/// stored index output and owes an `ENRICH_VER` bump.
///
/// A non-string value is out for a different reason: `prefilter_global` has no
/// arm for `fql_kind = <number>`, and a predicate it cannot serve it SKIPS —
/// leaving every row a candidate rather than failing.
fn counts_exactly(pred: &crate::ir::Predicate) -> bool {
    matches!(
        (
            crate::field_tiers::canonical(&pred.field),
            &pred.op,
            &pred.value
        ),
        ("fql_kind", CompareOp::Eq, PredicateValue::String(_))
    )
}

/// Whether every `HAVING` and `ORDER BY` names something a counted group row can
/// answer: its own `count`, or the field the grouping keyed it by.
///
/// A group row built from counts carries the grouped value and the count and
/// nothing else. Those two read on it exactly as they read on the representative
/// row the scan returns; any other field reads as absent here and as that row's
/// own value there. So a `HAVING lines >= 2` would be false on every group and
/// deliver an empty set with full confidence, and an `ORDER BY` on one would
/// rank every group equal. Both are handed back to the scan.
fn only_the_group_row_is_read(clauses: &Clauses, grouped_field: &str) -> bool {
    let reads_a_group_field = |name: &str| {
        let named = crate::field_tiers::canonical(name);
        named == "count" || named == grouped_field
    };
    clauses
        .having_predicates
        .iter()
        .all(|p| reads_a_group_field(&p.field))
        && clauses
            .order_by
            .as_ref()
            .is_none_or(|o| reads_a_group_field(&o.field))
}

/// Returns `true` when `GROUP BY fql_kind` can be served from the overlay's
/// kind bitmaps alone (no per-row materialisation needed).
///
/// Condition: `GROUP BY` on the kind field, an empty dirty overlay, no `WHERE`
/// at all, and a `HAVING`/`ORDER BY` naming only `count` or the grouped field
/// ([`only_the_group_row_is_read`]). The kind bitmaps carry no remainder — the
/// rows with no kind are in none of them — so the counting path derives that
/// group by subtraction and declines if the arithmetic cannot hold.
pub(super) fn group_by_kind_fast_path_eligible(clauses: &Clauses, dirty_empty: bool) -> bool {
    dirty_empty
        && clauses.where_predicates.is_empty()
        && matches!(&clauses.group_by, Some(GroupBy::Field(f))
            if crate::field_tiers::canonical(f) == "fql_kind")
        && only_the_group_row_is_read(clauses, "fql_kind")
}

/// A segment that stores the column for `field` but posted no values for it —
/// its rows carry values no key of the overlay's table counts.
///
/// A segment past its per-field posting budget writes the column and skips the
/// postings, so the two questions "does this segment know the field" and "does
/// the index hold its values" have different answers there. Reading such a
/// segment as "no value here" is a wrong count, not a slow one, so both readers
/// of this condition treat its rows as unaccounted for: the `=` tier keeps them
/// as candidates, and the `GROUP BY` count path declines outright.
///
/// Only meaningful for a field the builder posts. Ask it about any other name —
/// a column-only enrichment field, or one no segment stores — and it answers a
/// trivial yes that says nothing, which is why both callers test membership of
/// `POSTING_ENRICHMENT_FIELDS` before they get here.
fn segment_posts_partially(seg: &SegmentReader, field: &str) -> bool {
    !seg.posts_field(field) && seg.has_extra_col(field)
}

/// The `GROUP BY` shape [`ColumnarStorage::fast_group_by_enrichment`] answers,
/// and the canonical field it groups on.
///
/// Canonical on both sides, here as in every other eligibility test: a spelling
/// that misses the fast path is answered by the slow pipeline, so the failure is
/// silent and shows up only as a latency number.
pub(super) fn group_by_enrichment_fast_path_field(
    clauses: &Clauses,
    dirty_empty: bool,
) -> Option<&str> {
    if !dirty_empty || !clauses.where_predicates.is_empty() {
        return None;
    }
    let Some(GroupBy::Field(field)) = &clauses.group_by else {
        return None;
    };
    let field = crate::field_tiers::canonical(field);
    // Only the fields a segment posts per value: those are the ones the overlay
    // holds a `field=value` bitmap for, and the ones `segment_posts_partially`
    // can report incomplete coverage of. A field stored only as a column is
    // reached the same way it always was.
    if !super::super::segment_builder::POSTING_ENRICHMENT_FIELDS.contains(&field) {
        return None;
    }
    if !only_the_group_row_is_read(clauses, field) {
        return None;
    }
    Some(field)
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
        || !crate::filter::no_having_after_paging(clauses)
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
        || !crate::filter::no_having_after_paging(clauses)
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
/// predicates, no path filter, no HAVING.  The caller also gates on the dirty
/// overlay being empty so dirty rows cannot shadow committed rows with earlier
/// names.
///
/// HAVING is excluded because it runs in Stage 5, after the stream has already
/// stopped at `limit + offset` rows.  The Stage 4 dedupe runs after the stream
/// too and is NOT excluded: the caller collapses the streamed page, and a page
/// that stopped at `need` and then collapsed short is declined whole — handed
/// back to the pipeline rather than served missing rows the stream never read.
pub(super) fn order_by_name_fast_path(clauses: &Clauses) -> bool {
    matches!(
        &clauses.order_by,
        Some(OrderBy { field, direction: SortDirection::Asc }) if field == "name"
    ) && clauses.limit.is_some()
        && clauses.group_by.is_none()
        && clauses.where_predicates.is_empty()
        && clauses.in_glob.is_none()
        && clauses.exclude_globs.is_empty()
        && crate::filter::no_having_after_paging(clauses)
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
        && crate::filter::no_having_after_paging(clauses)
}

/// Return `true` when a bare `LIMIT N` — no ORDER BY written at all — may be
/// served by the ascending name stream.
///
/// With no ORDER BY the pipeline still sorts by the `(name, line, path,
/// fql_kind)` tie-break before it cuts the page, and that ordering starts
/// with `name` ascending — the order the name FST streams in. The stream
/// completes each name group before it checks its budget and the caller sorts
/// the streamed superset with the same comparator, so the page it cuts is the
/// pipeline's page. The clause conditions mirror
/// [`order_by_name_fast_path`]; the caller additionally requires unique
/// source paths, which is what makes the stored per-segment deduplicated
/// counts sum to this query's honest `total`.
pub(super) const fn bare_limit_name_fast_path(clauses: &Clauses) -> bool {
    clauses.order_by.is_none()
        && clauses.limit.is_some()
        && clauses.group_by.is_none()
        && clauses.where_predicates.is_empty()
        && clauses.in_glob.is_none()
        && clauses.exclude_globs.is_empty()
        && crate::filter::no_having_after_paging(clauses)
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
