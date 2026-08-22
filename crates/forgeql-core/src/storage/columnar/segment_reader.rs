//! [`SegmentReader`] — mmap-based reader for `.fqsf` single-file columnar segments.
//!
//! Opens a segment file written by [`SegmentBuilder`], validates the outer
//! `FQSF` magic and the inner `FQSG` header blob, mmaps the whole file with
//! a single `Mmap`, reads the TOC in place to locate every named blob, opens
//! the FSTs over the mapping, and addresses the Roaring posting blobs in
//! place — a bitmap is decoded only when a lookup asks for it (see
//! `postings.rs`).
//!
//! One `Mmap` per segment → 1 VMA instead of 25.

// Suppress pedantic/nursery lints that are legitimate in this low-level
// mmap I/O module.
#![allow(
    clippy::cast_possible_truncation, // u32/u64 → usize: safe on all supported (≥32-bit) platforms
    clippy::cast_lossless,            // u8 → usize: more readable as `as` in tight binary parsing
    clippy::module_name_repetitions,  // SegmentReader in segment_reader — intentional public API
    clippy::too_many_lines,           // `open` is long by necessity; splitting hurts locality
    clippy::missing_panics_doc,       // cast_slice panics only on corrupt mmap data
    clippy::collapsible_if,           // let-chain style preferred; some nested ifs left for clarity
    clippy::doc_markdown,              // binary format identifiers and O-notation in docs
    clippy::must_use_candidate,        // reader accessors; callers decide whether to use results
    clippy::ref_option,                // &Option<Mmap> helper signatures are clear as-is
)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, ensure};
use bytemuck::cast_slice;
use fst::Map as FstMap;
use memmap2::{Mmap, MmapOptions};
use roaring::RoaringBitmap;

use crate::filter::apply_clauses;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;

use super::segment_builder::{
    ENTRY_NAME_LEN, FILE_MAGIC, FILE_VERSION, MAGIC, POSTING_ENRICHMENT_FIELDS, TOC_ENTRY_SIZE,
    ZONEMAP_NUMERIC_FIELDS,
};

mod intern;
mod load;
use intern::intern;
use load::{
    Toc, blob_name_in, decode_name_postings, load_zone_maps, parse_column_entries, parse_toc,
};
mod postings;
use postings::{Key, KeyKind, PostingBlob, decode};

// ─────────────────────────────────────────────────────────────────────────────
// Format constants (must match segment_builder.rs)
// ─────────────────────────────────────────────────────────────────────────────

const SEGMENT_SCHEMA_VERSION: u32 = 2;
const TYPE_TAG_STR_OPT: u8 = 5;
const CORE_COLUMN_NAMES: &[&str] = &[
    "name_id",
    "fql_kind_id",
    "line",
    "byte_start",
    "byte_end",
    "usages_count",
    "language_id",
];
/// Byte length of the fixed-size FQSG header blob preamble.
const HEADER_PREAMBLE_LEN: usize = 80;

// ─────────────────────────────────────────────────────────────────────────────
// MmapSlice — zero-copy FST backing
// ─────────────────────────────────────────────────────────────────────────────

/// A byte slice of a parent segment's `Arc<Mmap>`, used to back the FST
/// without any heap allocation.
///
/// `FstMap<MmapSlice>` holds the `Arc<Mmap>` alive and reads FST bytes
/// directly from the mapped pages — no `to_vec()` needed.
pub(crate) struct MmapSlice {
    mmap: Arc<Mmap>,
    start: usize,
    end: usize,
}

impl AsRef<[u8]> for MmapSlice {
    fn as_ref(&self) -> &[u8] {
        &self.mmap[self.start..self.end]
    }
}

impl MmapSlice {
    pub(crate) const fn new(mmap: Arc<Mmap>, start: usize, end: usize) -> Self {
        Self { mmap, start, end }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// StringPool — mmap-backed per-segment string intern table
// ─────────────────────────────────────────────────────────────────────────────

/// Slice-backed string intern table backed by the parent segment's `Arc<Mmap>`.
///
/// Reads `strings_offsets` and `strings_data` blobs from the single `.fqsf`
/// mmap rather than maintaining separate per-file mmaps.
struct StringPool {
    mmap: Arc<Mmap>,
    /// Byte range of the `strings_offsets` blob within `mmap`.
    off_start: usize,
    off_end: usize,
    /// Byte range of the `strings_data` blob within `mmap`.
    dat_start: usize,
    dat_end: usize,
    string_count: u32,
    /// Reverse map for O(1) prefilter lookups, built on first use.
    ///
    /// Its keys are an owned copy of every string in the pool — bytes the mmap
    /// already holds, duplicated onto the heap. It used to be built eagerly at
    /// open time, for every segment, whether or not anything would ask it a
    /// question.
    ///
    /// **No indexing step reads it.** Every caller is a query prefilter, so an
    /// index build used to pay to construct one of these per segment and then
    /// read none of them. Measured on the Linux kernel — 80,426 segments —
    /// building them cost 16.4 of the 17.8 seconds spent opening segments for
    /// the overlay build, which now takes 1.4. The overlay comes out
    /// byte-identical, which is the property to preserve if this is ever made
    /// eager again for some other reason.
    ///
    /// It is not, however, where that phase's memory went: dropping the eager
    /// build left the phase's 7.5 GiB growth unchanged. Of what remained,
    /// about 40% (~210 of ~550 MiB, measured on a 3.06M-row corpus) was the
    /// posting bitmaps `open` deserialised for every segment; those are now
    /// read in place (see `postings.rs`). The rest is the readers' own tables
    /// — TOC, column names, string-pool bounds — and is still heap.
    reverse: OnceLock<HashMap<String, u32>>,
}

impl StringPool {
    fn from_blobs(
        mmap: Arc<Mmap>,
        off_range: (usize, usize),
        dat_range: (usize, usize),
        string_count: u32,
    ) -> Result<Self> {
        let (off_start, off_end) = off_range;
        let (dat_start, dat_end) = dat_range;

        // Validate string pool at open time so corrupt data is detected early
        // rather than causing a panic mid-query inside `get()`.
        //
        // Required invariants:
        //  1. `strings_offsets` blob has ≥ (string_count + 1) * 4 bytes.
        //  2. Offsets are monotonically non-decreasing.
        //  3. Last offset ≤ `strings_data` blob length.
        if string_count > 0 {
            let expected_offset_bytes = (string_count as usize + 1) * 4;
            let actual_offset_bytes = off_end - off_start;
            ensure!(
                actual_offset_bytes >= expected_offset_bytes,
                "strings_offsets blob has {actual_offset_bytes} bytes; \
                 expected ≥ {expected_offset_bytes} for {string_count} strings"
            );

            let off_slice: &[u32] = cast_slice(&mmap[off_start..off_end]);
            let dat_len = dat_end - dat_start;
            for i in 0..string_count as usize {
                let lo = off_slice[i] as usize;
                let hi = off_slice[i + 1] as usize;
                ensure!(
                    lo <= hi,
                    "strings_offsets blob is not monotone at index {i}: {lo} > {hi}"
                );
            }
            let last = off_slice[string_count as usize] as usize;
            ensure!(
                last <= dat_len,
                "strings_offsets: last offset {last} > strings_data length {dat_len}"
            );
        }

        let pool = Self {
            mmap,
            off_start,
            off_end,
            dat_start,
            dat_end,
            string_count,
            reverse: OnceLock::new(),
        };

        Ok(pool)
    }

    /// The id of `s` in this pool, or `None` when the pool does not hold it.
    ///
    /// Builds the reverse map on first call and reuses it after. Callers are
    /// prefilters deciding whether a segment can match a value at all, so the
    /// answer has to be exact in both directions: `None` means the string is
    /// absent from this segment, which lets a caller skip the segment, and
    /// inventing one would skip a segment that matches.
    fn id_of(&self, s: &str) -> Option<u32> {
        self.reverse
            .get_or_init(|| {
                (0..self.string_count)
                    .map(|id| (self.get(id).to_owned(), id))
                    .collect()
            })
            .get(s)
            .copied()
    }

    /// Look up string ID `id`; returns `""` for absent / out-of-range IDs.
    fn get(&self, id: u32) -> &str {
        if id == u32::MAX || id >= self.string_count {
            return "";
        }
        let off_slice: &[u32] = cast_slice(&self.mmap[self.off_start..self.off_end]);
        let (Some(&start_u32), Some(&end_u32)) =
            (off_slice.get(id as usize), off_slice.get(id as usize + 1))
        else {
            return "";
        };
        let (start, end) = (
            start_u32 as usize + self.dat_start,
            end_u32 as usize + self.dat_start,
        );
        if end > self.dat_end || start > end {
            return "";
        }
        std::str::from_utf8(&self.mmap[start..end]).unwrap_or("")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Column ranges — resolved once at open
// ─────────────────────────────────────────────────────────────────────────────

/// Byte range `(start, end)` of a blob within the segment mmap.
///
/// A column the segment does not store resolves to `(0, 0)`, which slices to an
/// empty blob — exactly what a missing TOC entry used to yield — so an absent
/// column still reads as `0` / `u32::MAX` / `None`, never a panic.
type ColRange = (usize, usize);

/// Byte ranges of the columns every row read touches.
///
/// A column's bytes never move for the life of the mmap, so naming one per
/// access — formatting `col_<name>` and hashing it, for every column of every
/// row — bought nothing. The ranges are resolved once here at open and the
/// accessors index the mapping directly, the way [`StringPool`] already holds
/// its own two blob ranges.
struct FixedColumns {
    name_id: ColRange,
    fql_kind_id: ColRange,
    language_id: ColRange,
    line: ColRange,
    byte_start: ColRange,
    byte_end: ColRange,
    usages_count: ColRange,
    ordinal: ColRange,
    parent_ordinal: ColRange,
    rev: ColRange,
    first_child_ordinal: ColRange,
    next_sibling_ordinal: ColRange,
    prev_sibling_ordinal: ColRange,
}

impl FixedColumns {
    /// Resolve every fixed column against the segment's TOC.
    fn resolve(toc: &Toc<'_>) -> Self {
        let at = |name: &str| toc.get(name).unwrap_or((0, 0));
        Self {
            name_id: at("col_name_id"),
            fql_kind_id: at("col_fql_kind_id"),
            language_id: at("col_language_id"),
            line: at("col_line"),
            byte_start: at("col_byte_start"),
            byte_end: at("col_byte_end"),
            usages_count: at("col_usages_count"),
            ordinal: at("col_ordinal"),
            parent_ordinal: at("col_parent_ordinal"),
            rev: at("col_rev"),
            first_child_ordinal: at("col_first_child_ordinal"),
            next_sibling_ordinal: at("col_next_sibling_ordinal"),
            prev_sibling_ordinal: at("col_prev_sibling_ordinal"),
        }
    }

    /// Range of the fixed column spelled `col`, or the empty range when `col`
    /// names none of them.
    ///
    /// Every `col_*` blob a segment holds is either a fixed column or an
    /// enrichment column, so this and the enrichment list together reach every
    /// blob the old format-and-hash lookup could.
    fn by_short_name(&self, col: &str) -> ColRange {
        match col {
            "name_id" => self.name_id,
            "fql_kind_id" => self.fql_kind_id,
            "language_id" => self.language_id,
            "line" => self.line,
            "byte_start" => self.byte_start,
            "byte_end" => self.byte_end,
            "usages_count" => self.usages_count,
            "ordinal" => self.ordinal,
            "parent_ordinal" => self.parent_ordinal,
            "rev" => self.rev,
            "first_child_ordinal" => self.first_child_ordinal,
            "next_sibling_ordinal" => self.next_sibling_ordinal,
            "prev_sibling_ordinal" => self.prev_sibling_ordinal,
            _ => (0, 0),
        }
    }
}

/// Clause fields a materialised row answers from a `SymbolMatch` struct field
/// rather than from its enrichment map.
///
/// A segment storing an enrichment column under one of these names can make a
/// built row and a row view disagree, but only where the operator's type does
/// not match the struct accessor: `WHERE name LIKE …` reads the struct on a
/// built row, while `WHERE name = 42` falls through to `fields.get("name")` and
/// finds the shadow column, which the row view would never consult. That one
/// name therefore stops being answered from a column; the segment's other
/// fields are unaffected, since a collision on one name says nothing about the
/// rest.
const STRUCT_BACKED_FIELDS: &[&str] = &[
    "name",
    "node_kind",
    "fql_kind",
    "language",
    "path",
    "node_id",
    "usages",
    "count",
    "line",
];

/// One bit per name in [`SegmentReader::shadowed_struct_fields`], so the list
/// may not outgrow the mask that stands for it.
const _: () = assert!(
    STRUCT_BACKED_FIELDS.len() <= u16::BITS as usize,
    "STRUCT_BACKED_FIELDS no longer fits the shadowed-field mask"
);

/// Which column answers a clause field on a row of a segment.
///
/// One resolver serves both consumers — the caller deciding whether a
/// predicate can be answered before any row is built, and the row view reading
/// the value — so the two cannot drift into a field claimed answerable that
/// then resolves to nothing.
pub(crate) enum RowField {
    /// The name column.
    Name,
    /// The fql_kind column; the empty string reads as absent.
    FqlKind,
    /// The language column; the empty string reads as absent.
    Language,
    /// The path the caller supplied for this segment's file.
    Path,
    /// The line column; `0` reads as absent.
    Line,
    /// An enrichment column, already resolved to its byte range.
    Extra(ColRange),
    /// No column of this segment holds the field, and no struct field of the
    /// row it would build holds it either — so both readers answer `None`, and
    /// the predicate naming it can be answered here.
    ///
    /// Distinct from [`Self::Unanswerable`], which is where the two would
    /// disagree. Reporting a confident absence is an answer: on an absent
    /// field every operator [`crate::filter::eval_predicate_on`] consults the
    /// field with is false, negations included, which is exactly what the
    /// built row's filter concludes from the same missing value.
    Absent,
    /// The built row answers the field from somewhere this reader cannot see,
    /// so the predicate naming it is not answered early — it runs against the
    /// materialised rows instead.
    Unanswerable,
}

/// One row of one segment, viewed in place.
///
/// Implements [`crate::filter::ClauseTarget`] so a residual `WHERE` can be
/// evaluated against the columns before any row is built. It owns nothing:
/// every string it yields is borrowed from the segment's mapping.
pub(crate) struct SegRowRef<'a> {
    /// The segment holding the row.
    pub(crate) seg: &'a SegmentReader,
    /// Row index within that segment.
    pub(crate) row: u32,
    /// Path of the file the segment was built from, as the caller spells it.
    pub(crate) source_path: Option<&'a Path>,
}

impl<'a> SegRowRef<'a> {
    /// The row's value for the canonical clause field `field`, as a string.
    ///
    /// The lifetime is the segment's, not the borrow of this view: every
    /// string it yields lives in the mapping, so a caller may resolve one once
    /// and carry it while the view itself goes out of scope. [`RowView`] is
    /// that caller.
    pub(crate) fn str_value(&self, field: &str) -> Option<&'a str> {
        match self.seg.row_field(field, self.source_path.is_some()) {
            RowField::Name => Some(self.seg.name_of(self.row)),
            RowField::FqlKind => non_empty(self.seg.fql_kind_of(self.row)),
            RowField::Language => non_empty(self.seg.language_of(self.row)),
            RowField::Path => self.source_path.and_then(Path::to_str),
            RowField::Extra(range) => self.seg.opt_str_in(range, self.row),
            RowField::Line | RowField::Absent | RowField::Unanswerable => None,
        }
    }

    /// The row's value for the canonical clause field `field`, as a number.
    pub(crate) fn num_value(&self, field: &str) -> Option<i64> {
        match self.seg.row_field(field, self.source_path.is_some()) {
            RowField::Line => match self.seg.line_of(self.row) {
                0 => None,
                line => Some(i64::from(line)),
            },
            RowField::Extra(range) => self.seg.opt_str_in(range, self.row)?.parse().ok(),
            RowField::Name
            | RowField::FqlKind
            | RowField::Language
            | RowField::Path
            | RowField::Absent
            | RowField::Unanswerable => None,
        }
    }
}

/// The clause fields a row view cannot answer as the row it would build.
///
/// Every other field a built row reports, it reports from a column of the
/// segment the row came from, so a view reading that column reads the same
/// value. These three do not come from a column at all: `usages` is stamped
/// from the workspace overlay after materialisation, `node_id` is derived from
/// the row's ordinal as the row is built, and `count` is assigned later still
/// by GROUP BY.
///
/// **A same-named enrichment column is not on this list, and used to be.** A
/// built row answers `name`, `node_kind`, `fql_kind`, `language`, `path` and
/// `node_id` from its own struct fields and never from its enrichment map, so
/// a segment carrying a column called `name` changes what `WHERE name = 42`
/// reads — that predicate falls through to the map on both readers — without
/// changing what the ordering and the collapse key read, which is the struct.
/// Treating the shadow as if it withheld the field cost far more than it
/// saved, and the reason is structural rather than incidental. A segment's
/// enrichment columns are the enrichers' output PLUS every tree-sitter grammar
/// field of every emitted node, which `extract_fields`
/// (`ast/index/file_indexer/rows.rs`) copies wholesale into the row's map for
/// the builder to turn into columns. `name` is what most grammars call a
/// definition's identifier child, so essentially every code segment carries a
/// `name` column: 308 of the 411 segments of this repository's index, each of
/// them refused the cheap route for every query.
///
/// It is not a property of macros, and `name` is not the only one it has
/// already happened to — 211 of those same segments carry a column called
/// `path`. Nor is it a closed list: a grammar naming a field after a
/// struct-backed name shadows that one too, and nothing announces it.
pub(crate) const VIEW_CANNOT_ANSWER: &[&str] = &["node_id", "usages", "count"];

/// Whether a row view ranks and keys `field` the way the row it would build
/// ranks and keys it.
///
/// A property of the field, not of the segment: [`RowView`] resolves every
/// field exactly as [`crate::result::SymbolMatch`] resolves it — struct-backed
/// names from the fixed columns the built row is filled from, everything else
/// from the enrichment column the built row's map is filled from — so the only
/// fields the two can disagree on are the ones no column holds at all.
///
/// It is deliberately not [`SegmentReader::answers_field`], which is the
/// conservative question a *predicate* asks: that one may decline where it is
/// unsure, because a declined predicate is merely run later. An ordering that
/// declines where it should not is not merely slower — it takes the whole page
/// off the route.
pub(crate) fn ranks_field_like_a_built_row(field: &str) -> bool {
    !VIEW_CANNOT_ANSWER.contains(&field)
}

/// One segment row carried through the duplicate collapse, the ordering and
/// the page cut without building the [`SymbolMatch`] it stands for.
///
/// **It reads what the built row will read.** Each arm below mirrors the
/// matching arm of `impl ClauseTarget for SymbolMatch`, against the same
/// columns `materialize_one_row` fills that row from: the six struct-backed
/// names from their fixed columns, and every other name from the enrichment
/// column the row's map would be filled from. `a_view_reads_every_field_as_the_row_it_builds`
/// pins the two against each other field by field, including on a segment
/// whose enrichment column shadows a struct-backed name. The exceptions are
/// [`VIEW_CANNOT_ANSWER`], and a caller must gate on them rather than read
/// them.
///
/// `name` and `line` are resolved once, when the view is made, because
/// [`crate::filter::order_cmp`] consults `name` on every pair it compares and
/// `line` on every pair whose names tie. Reading either through the mapping
/// costs an index into the column plus a UTF-8 validation of the bytes, and
/// paying that per comparison instead of per row is the shape that made a
/// mapping-backed reader 12% slower than a heap-backed one on scans until the
/// segment column names were interned: a byte range into a mapping is free to
/// store and not free to read, so it must not sit on a per-comparison path.
///
/// It still owns nothing — `name` points into the segment's mapping — so a
/// view costs exactly its own size and no heap, which is why
/// [`crate::storage::columnar::columnar_storage::fast_paths`] can price the
/// scan bound from `size_of` alone. The fields are private so the two resolved
/// once can only be filled by [`Self::of`].
pub(crate) struct RowView<'a> {
    seg: &'a SegmentReader,
    /// Path of the file the segment was built from, as the caller spells it.
    source_path: Option<&'a Path>,
    /// The fixed name column at this row — what the built row's `name` is.
    name: &'a str,
    /// Row index within the segment.
    row: u32,
    /// The fixed line column at this row, with `0` standing for absent, which
    /// is how the built row spells it too.
    line: u32,
}

impl<'a> RowView<'a> {
    /// The view of row `row` of `seg`.
    pub(crate) fn of(seg: &'a SegmentReader, source_path: Option<&'a Path>, row: u32) -> Self {
        Self {
            seg,
            source_path,
            name: seg.name_of(row),
            row,
            line: seg.line_of(row),
        }
    }

    /// Row index within its segment.
    pub(crate) const fn row(&self) -> u32 {
        self.row
    }

    /// Path of the file this row's segment was built from.
    pub(crate) const fn source_path(&self) -> Option<&'a Path> {
        self.source_path
    }

    /// The row's value for the canonical clause field `field`, as a string —
    /// arm for arm what `impl ClauseTarget for SymbolMatch` would answer on the
    /// row this view stands for.
    ///
    /// `node_kind` is `None` because segments do not store it and the built row
    /// leaves it `None`; `node_id` is `None` because a view cannot know it, and
    /// is in [`VIEW_CANNOT_ANSWER`] for that reason rather than answered here.
    pub(crate) fn str_value(&self, field: &str) -> Option<&'a str> {
        match field {
            "name" => Some(self.name),
            "node_kind" | "node_id" => None,
            "fql_kind" => non_empty(self.seg.fql_kind_of(self.row)),
            "language" => non_empty(self.seg.language_of(self.row)),
            "path" => self.source_path.and_then(Path::to_str),
            other => self.enrichment(other),
        }
    }

    /// The row's value for the canonical clause field `field`, as a number —
    /// arm for arm what a built row would answer.
    ///
    /// The fallback reads the enrichment column and NOT [`Self::str_value`],
    /// because that is what the built row does: its `field_num` goes straight
    /// to the enrichment map for any name outside its own three, so on a
    /// segment whose column shadows a struct-backed name the two readers must
    /// both find the shadow here and both find the struct there.
    pub(crate) fn num_value(&self, field: &str) -> Option<i64> {
        match field {
            "usages" | "count" => None,
            "line" => (self.line != 0).then(|| i64::from(self.line)),
            other => self.enrichment(other)?.parse().ok(),
        }
    }

    /// This row's value in the enrichment column named `field`, or `None` where
    /// the segment has no such column — which is also when the built row's map
    /// would not carry it, the map being filled from these same columns.
    fn enrichment(&self, field: &str) -> Option<&'a str> {
        self.seg
            .extra_col_range(field)
            .and_then(|range| self.seg.opt_str_in(range, self.row))
    }

    /// The duplicate-collapse key of this row, read from the fixed columns.
    ///
    /// `(name, fql_kind, line)` — the path half of the Stage 4 key is the
    /// segment's own for every row it holds and so cannot tell two of them
    /// apart. All three are fixed columns of every segment and all three are
    /// what the built row is filled from, so this key and the key the pass over
    /// built rows collapses on are the same key on every segment.
    pub(crate) fn collapse_key(&self) -> (&'a str, Option<&'a str>, u32) {
        (self.name, self.str_value("fql_kind"), self.line)
    }

    /// A hash of [`Self::collapse_key`], for grouping candidates before they
    /// are compared. A collision costs a comparison and never a row.
    pub(crate) fn collapse_key_hash(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.collapse_key().hash(&mut hasher);
        hasher.finish()
    }

    /// Build the result row this view stands for.
    ///
    /// `None` where the view carries no path, which is the one thing a built
    /// row cannot be given after the fact — its `node_id` and `rev` are
    /// derived from it.
    pub(crate) fn materialize(&self) -> Option<SymbolMatch> {
        let path = self.source_path?;
        self.seg.materialize_one_row(self.row, path)
    }
}

/// `None` for the empty string, which is how a fixed string column spells a
/// value the row does not have.
const fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// Decode an id-keyed posting walk into `(id, bitmap)` pairs, one bitmap at a
/// time. A bytes-keyed entry in an id-keyed blob is a layout error, not a row.
fn id_entries(
    entries: postings::Entries<'_>,
) -> impl Iterator<Item = Result<(u32, RoaringBitmap)>> + '_ {
    entries.map(|entry| {
        let (key, bytes) = entry?;
        let Key::Id(id) = key else {
            anyhow::bail!("posting blob keyed by bytes where an id was expected");
        };
        Ok((id, decode(bytes)?))
    })
}

/// The text at `range` in `mmap`; the empty string when those bytes are not
/// valid UTF-8.
///
/// A segment spells its own column names in its header. They are read from
/// there once, at open, and interned — so this runs per column per segment
/// while a segment is being opened, and never again while it is read.
fn text_at(mmap: &Mmap, range: ColRange) -> &str {
    std::str::from_utf8(&mmap[range.0..range.1]).unwrap_or("")
}

// ─────────────────────────────────────────────────────────────────────────────
// SegmentReader
// ─────────────────────────────────────────────────────────────────────────────

/// Mmap-based read-only view of a single `.fqsf` columnar segment file.
///
/// Open with [`SegmentReader::open`].  The reader holds one `Arc<Mmap>` for
/// the whole file; individual blobs are accessed as subslices.
/// Fields decoded from the inner FQSG `header` blob of a segment file.
struct HeaderFields {
    provider_id: String,
    content_id: Vec<u8>,
    row_count: u32,
    string_count: u32,
    /// Byte range of each extra enrichment column name within the mapping, in
    /// header order. The header parse hands on the range; `open` reads the name
    /// from it once and interns it.
    extra_col_names: Vec<ColRange>,
}
pub struct SegmentReader {
    /// Whole-file mmap shared with the string pool.
    mmap: Arc<Mmap>,
    /// Byte ranges of the fixed columns, resolved once at open so that reading
    /// a row costs an index into the mapping and nothing else.
    fixed: FixedColumns,
    /// Number of rows stored in this segment.
    pub row_count: u32,
    /// Provider ID decoded from the header blob.
    pub provider_id: String,
    /// Raw content ID bytes (length matches the provider's hash width).
    pub content_id: Vec<u8>,
    /// Enrichment columns from the header blob, in header order.
    extra_cols: Vec<ExtraCol>,
    /// Bit `i` is set when an enrichment column of this segment is named
    /// `STRUCT_BACKED_FIELDS[i]` — the collision that makes that one field
    /// unanswerable from the columns.
    ///
    /// Derived from `extra_cols` at open because [`Self::row_field`] asks the
    /// question once per row: answering it by scanning the column names would
    /// read the mapping a million times over a whole-corpus scan, and the
    /// answer cannot change while the segment is open.
    shadowed_struct_fields: u16,
    strings: StringPool,
    /// `postings_fql_kind`, addressed in place — see `postings.rs`. Nothing
    /// is decoded until a lookup asks for one kind's rows.
    kind_blob: PostingBlob,
    /// One `postings_<field>` per enrichment field this segment posts, in
    /// `POSTING_ENRICHMENT_FIELDS` order, addressed in place.
    field_blobs: Vec<(&'static str, PostingBlob)>,
    pub(crate) zone_maps: HashMap<&'static str, (u32, u32)>,
    /// `name_prefix`, addressed in place; keyed by lower-cased 1–2 character
    /// name prefixes.
    name_prefix_blob: PostingBlob,
    pub(crate) name_fst: FstMap<MmapSlice>,
    /// Row ids behind the name FST's values, as a byte range in the mapping.
    name_postings: ColRange,
    /// Usage postings FST (BUG-006): identifier text → 1-based source lines.
    /// `None` when the file produced no usage sites (blob omitted at flush).
    pub(crate) usages_fst: Option<FstMap<MmapSlice>>,
    /// Lines behind the usage FST's values, as a byte range in the mapping;
    /// empty when the segment carries no usages blob.
    usages_postings: ColRange,
    /// Mention postings, one per occurrence role the file produced, sorted by
    /// role so lookups return roles in a stable order. Empty when the file
    /// produced no mentions (the blob pairs are omitted at flush).
    mentions: Vec<MentionIndex>,
}

/// One enrichment column of a segment.
///
/// The values stay where the `col_<name>` blob already holds them, as a byte
/// range into the mapping. The name is interned instead of either copied or
/// re-read: a segment carries its own column names, so tens of thousands of
/// segments naming the same forty columns would hold tens of thousands of
/// copies of those names, while reading each one back out of the mapping makes
/// every row batch pay for it again.
struct ExtraCol {
    /// The column's name, shared with every other reader whose segment names a
    /// column the same way.
    name: Arc<str>,
    /// Byte range of the column's `col_<name>` blob within the mapping.
    data: ColRange,
}

/// One occurrence role's mention index.
struct MentionIndex {
    /// The role's name, shared the same way an enrichment column name is.
    /// Reading it off the segment's own table of contents rather than a list
    /// compiled into the reader is what keeps roles additive.
    role: Arc<str>,
    /// Name → encoded postings for this role.
    fst: FstMap<MmapSlice>,
    /// Byte range of the role's `mentions_<role>_postings` blob.
    postings: ColRange,
}

impl SegmentReader {
    /// Open and validate a `.fqsf` segment file.
    ///
    /// Mmaps the whole file, parses the outer FQSF TOC, validates the inner
    /// `FQSG` header blob, builds the string pool, opens the FSTs over the
    /// mapping, and checks the layout of the Roaring posting blobs without
    /// decoding them — a bitmap is deserialised only when a lookup asks for
    /// it (see `postings.rs`), so an open segment holds no posting bitmap on
    /// the heap.
    ///
    /// # Errors
    /// Returns `Err` on I/O failure, missing file, format mismatch, schema
    /// version mismatch, corrupt string pool, or a posting blob whose entry
    /// table runs past its end. Corrupt bitmap *bytes* inside a well-formed
    /// entry table are not detected here; a lookup reports them.
    pub fn open(path: &Path) -> Result<Self> {
        // ── 1-2. Mmap + validate the outer FQSF header ────────────────────
        let (mmap, file_len) = Self::map_and_validate(path)?;

        // ── 3. Parse TOC ──────────────────────────────────────────────────
        // Read in place, and dropped when this function returns: what the
        // reader keeps of the table is the byte ranges resolved below, plus
        // one interned copy of each name it still needs once open has returned.
        let toc = parse_toc(&mmap, file_len, path)?;

        // ── 4-5. Inner FQSG header blob + extra enrichment columns ────────
        let hdr = Self::parse_header_blob(&mmap, &toc, path)?;

        // ── 6. String pool ────────────────────────────────────────────────
        let off_range = toc.get("strings_offsets").unwrap_or((0, 0));
        let dat_range = toc.get("strings_data").unwrap_or((0, 0));
        let strings =
            StringPool::from_blobs(Arc::clone(&mmap), off_range, dat_range, hdr.string_count)?;

        // ── 7. Roaring postings — addressed in place, layout checked, not
        //       decoded (see `postings.rs`). The blob names are built in a
        //       stack buffer: one `String` per posted field per segment is
        //       millions of allocations across a workspace.
        let kind_blob =
            PostingBlob::new(toc.get("postings_fql_kind").unwrap_or((0, 0)), KeyKind::Id);
        kind_blob.validate(&mmap, "postings_fql_kind")?;
        let mut field_blobs: Vec<(&'static str, PostingBlob)> = Vec::new();
        let mut name_buf = [0_u8; ENTRY_NAME_LEN];
        for &field in POSTING_ENRICHMENT_FIELDS {
            let Some(blob_name) = blob_name_in(&mut name_buf, "postings_", field, "") else {
                continue;
            };
            let Some(range) = toc.get(blob_name) else {
                continue;
            };
            let blob = PostingBlob::new(range, KeyKind::Id);
            if !blob.is_present() {
                continue;
            }
            blob.validate(&mmap, blob_name)?;
            field_blobs.push((field, blob));
        }
        let zone_maps = load_zone_maps(&toc, &mmap)?;

        // ── 8. FST + name prefix ──────────────────────────────────────────
        let (fst_start, fst_end) = toc.get("name_fst").unwrap_or((0, 0));
        let name_fst = FstMap::new(MmapSlice {
            mmap: Arc::clone(&mmap),
            start: fst_start,
            end: fst_end,
        })
        .context("parsing name_fst blob")?;
        let name_postings = toc.get("name_postings").unwrap_or((0, 0));
        let name_prefix_blob =
            PostingBlob::new(toc.get("name_prefix").unwrap_or((0, 0)), KeyKind::Bytes);
        name_prefix_blob.validate(&mmap, "name_prefix")?;

        // ── 8b. Usage postings FST (BUG-006; blob absent = no usages) ─────
        let usages_fst = match toc.get("usages_fst") {
            Some((start, end)) if end > start => Some(
                FstMap::new(MmapSlice {
                    mmap: Arc::clone(&mmap),
                    start,
                    end,
                })
                .context("parsing usages_fst blob")?,
            ),
            _ => None,
        };
        let usages_postings = toc.get("usages_postings").unwrap_or((0, 0));

        // ── 8c. Mention postings, one FST per occurrence role ─────────────
        let mentions = Self::load_mentions(&mmap, &toc)?;

        // ── 9. Column ranges ──────────────────────────────────────────────
        // A column's bytes never move for the life of the mmap, so every range
        // is resolved once here; reading a row then costs an index into the
        // mapping, with no column name to format and no TOC lookup to hash.
        let fixed = FixedColumns::resolve(&toc);
        let mut name_buf = [0_u8; ENTRY_NAME_LEN];
        let extra_cols: Vec<ExtraCol> = hdr
            .extra_col_names
            .iter()
            .map(|&name| {
                let name = text_at(&mmap, name);
                let data = blob_name_in(&mut name_buf, "col_", name, "")
                    .and_then(|blob| toc.get(blob))
                    .unwrap_or((0, 0));
                ExtraCol {
                    name: intern(name),
                    data,
                }
            })
            .collect();

        // Which of the fields a built row answers from its own struct this
        // segment shadows with an enrichment column of the same name. Asked
        // once per row later, so it is answered once here.
        let shadowed_struct_fields = Self::shadow_mask(&extra_cols);

        Ok(Self {
            mmap,
            fixed,
            row_count: hdr.row_count,
            provider_id: hdr.provider_id,
            content_id: hdr.content_id,
            extra_cols,
            shadowed_struct_fields,
            strings,
            kind_blob,
            field_blobs,
            zone_maps,
            name_prefix_blob,
            name_fst,
            name_postings,
            usages_fst,
            usages_postings,
            mentions,
        })
    }

    /// Discover this segment's mention postings from its table of contents.
    ///
    /// One FST per occurrence role, found by the shape of the blob name so the
    /// reader needs no list of roles compiled into it: a segment written before
    /// a role existed simply has no blob for it. What a role costs the reader
    /// is its FST, two byte ranges, and a pointer to the one shared copy of
    /// the role's name.
    ///
    /// # Errors
    /// Returns `Err` when a role's FST blob does not parse.
    fn load_mentions(mmap: &Arc<Mmap>, toc: &Toc<'_>) -> Result<Vec<MentionIndex>> {
        const PREFIX: &str = "mentions_";
        const SUFFIX: &str = "_fst";

        let mut mentions: Vec<MentionIndex> = Vec::new();
        let mut name_buf = [0_u8; ENTRY_NAME_LEN];
        for entry in toc.entries() {
            let Some(role) = entry
                .name
                .strip_prefix(PREFIX)
                .and_then(|rest| rest.strip_suffix(SUFFIX))
            else {
                continue;
            };
            let (start, end) = entry.range;
            if end <= start {
                continue;
            }
            let fst = FstMap::new(MmapSlice {
                mmap: Arc::clone(mmap),
                start,
                end,
            })
            .with_context(|| format!("parsing {} blob", entry.name))?;
            let postings = blob_name_in(&mut name_buf, PREFIX, role, "_postings")
                .and_then(|name| toc.get(name))
                .unwrap_or((0, 0));
            mentions.push(MentionIndex {
                role: intern(role),
                fst,
                postings,
            });
        }
        // Sorted by role name — the order the map this replaced returned.
        mentions.sort_by(|a, b| a.role.cmp(&b.role));
        Ok(mentions)
    }

    /// Which of the fields a built row answers from its own struct these
    /// enrichment columns shadow, as one bit per name in
    /// [`STRUCT_BACKED_FIELDS`].
    ///
    /// The mask is a derivation of the column names and of nothing else, so it
    /// is computed here and installed together with them — see
    /// [`Self::set_extra_cols`] — rather than kept in step by hand. A mask that
    /// disagreed with the columns would have [`Self::row_field`] answer a
    /// shadowed field from the struct, confidently and wrongly.
    fn shadow_mask(extra_cols: &[ExtraCol]) -> u16 {
        STRUCT_BACKED_FIELDS
            .iter()
            .enumerate()
            .fold(0_u16, |mask, (index, field)| {
                let shadowed = extra_cols.iter().any(|extra| extra.name.as_ref() == *field);
                if shadowed { mask | (1 << index) } else { mask }
            })
    }

    /// Replace the enrichment columns, recomputing the shadow mask from them.
    ///
    /// Change the columns of an open reader through here and not by assigning
    /// `extra_cols`: the mask is derived from the names and would otherwise go
    /// stale. Module-private, so that is a convention this module keeps, not
    /// something the compiler enforces.
    #[cfg(test)]
    fn set_extra_cols(&mut self, extra_cols: Vec<ExtraCol>) {
        self.shadowed_struct_fields = Self::shadow_mask(&extra_cols);
        self.extra_cols = extra_cols;
    }

    /// Mmap `path` and validate the outer FQSF magic, version, and host
    /// endianness. Returns the shared mmap and the file length in bytes.
    fn map_and_validate(path: &Path) -> Result<(Arc<Mmap>, usize)> {
        // ── 1. Mmap the whole file ────────────────────────────────────────
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening segment {}", path.display()))?;
        let file_len = file.metadata()?.len() as usize;
        ensure!(
            file_len >= 12,
            "segment {} is only {file_len} bytes (need ≥ 12 for FQSF header)",
            path.display()
        );
        #[expect(unsafe_code, reason = "single mmap of immutable segment file")]
        let mmap = Arc::new(
            unsafe { MmapOptions::new().map(&file) }
                .with_context(|| format!("mmap {}", path.display()))?,
        );
        drop(file);

        // ── 2. Validate outer FQSF magic ─────────────────────────────────
        ensure!(
            mmap[..4] == FILE_MAGIC,
            "invalid FQSF magic in {}",
            path.display()
        );
        let file_version = u32::from_le_bytes(mmap[4..8].try_into().context("FQSF version bytes")?);
        ensure!(
            file_version == FILE_VERSION,
            "FQSF version mismatch in {}: expected {FILE_VERSION}, got {file_version}",
            path.display()
        );

        if cfg!(target_endian = "big") {
            anyhow::bail!(
                "segment format is little-endian only; cannot open {} on a big-endian host",
                path.display()
            );
        }

        Ok((mmap, file_len))
    }

    /// Parse and validate the inner FQSG `header` blob: schema version, provider
    /// id, content id, and row/string counts, plus the extra enrichment column
    /// names (non-core string-option columns).
    fn parse_header_blob(mmap: &Arc<Mmap>, toc: &Toc<'_>, path: &Path) -> Result<HeaderFields> {
        let (hs, he) = toc.get("header").context("missing 'header' blob in FQSF")?;
        let header_bytes = &mmap[hs..he];

        ensure!(
            header_bytes.len() >= HEADER_PREAMBLE_LEN,
            "'header' blob in {} is only {} bytes (need ≥ {})",
            path.display(),
            header_bytes.len(),
            HEADER_PREAMBLE_LEN,
        );
        ensure!(
            header_bytes[..4] == MAGIC,
            "invalid FQSG magic in 'header' blob of {}",
            path.display()
        );

        let schema_version = u32::from_le_bytes(
            header_bytes[4..8]
                .try_into()
                .context("schema_version bytes")?,
        );
        ensure!(
            schema_version == SEGMENT_SCHEMA_VERSION,
            "schema version mismatch in {}: expected {SEGMENT_SCHEMA_VERSION}, got {schema_version}",
            path.display()
        );

        let provider_id = {
            let pid_bytes = &header_bytes[8..24];
            let end = pid_bytes.iter().position(|&b| b == 0).unwrap_or(16);
            String::from_utf8_lossy(&pid_bytes[..end]).into_owned()
        };

        let content_id_len = header_bytes[24] as usize;
        ensure!(content_id_len <= 32, "content_id_len {content_id_len} > 32");

        let content_id = header_bytes[28..28 + content_id_len].to_vec();

        let row_count =
            u32::from_le_bytes(header_bytes[60..64].try_into().context("row_count bytes")?);
        let string_count = u32::from_le_bytes(
            header_bytes[64..68]
                .try_into()
                .context("string_count bytes")?,
        );
        let column_count = u32::from_le_bytes(
            header_bytes[68..72]
                .try_into()
                .context("column_count bytes")?,
        );

        // Parse variable-length column entries from the header blob.
        let columns = parse_column_entries(header_bytes, HEADER_PREAMBLE_LEN, column_count)?;

        // ── 5. Collect extra enrichment column names ───────────────────────
        // As byte ranges into the mapping, since the header blob already spells
        // every one of them: the ranges are shifted from header-relative to
        // mapping-absolute here so a reader can read a name back without
        // knowing where its header sits.
        let extra_col_names: Vec<ColRange> = columns
            .iter()
            .filter(|&&(name, tag)| {
                let text = std::str::from_utf8(&header_bytes[name.0..name.1]).unwrap_or("");
                !CORE_COLUMN_NAMES.contains(&text) && tag == TYPE_TAG_STR_OPT
            })
            .map(|&(name, _)| (hs + name.0, hs + name.1))
            .collect();

        Ok(HeaderFields {
            provider_id,
            content_id,
            row_count,
            string_count,
            extra_col_names,
        })
    }

    /// Execute `FIND symbols` against this single segment.
    ///
    /// 1. Builds a candidate bitmap via Roaring prefilter for
    ///    `WHERE fql_kind = 'X'` predicates.
    /// 2. Materialises the candidate rows as [`SymbolMatch`] values.
    /// 3. Runs `apply_clauses` for residual WHERE, GROUP BY, ORDER BY,
    ///    LIMIT, OFFSET — ensuring parity with the legacy pipeline.
    ///
    /// `source_path` — optional path to the source file this segment
    /// represents.  Passed through as `SymbolMatch.path`; useful for
    /// parity testing and Phase 05 overlay queries.
    pub fn find_symbols(
        &self,
        clauses: &Clauses,
        source_path: Option<&Path>,
    ) -> Result<Vec<SymbolMatch>> {
        if self.row_count == 0 {
            return Ok(Vec::new());
        }
        let candidates = self.prefilter_kind(clauses);
        let mut results = self.materialize_rows(&candidates, source_path);
        apply_clauses(&mut results, clauses);
        Ok(results)
    }

    /// Return all row IDs whose symbol name exactly equals `name`.
    ///
    /// O(log n) FST lookup + O(k) postings decode where k = match count.
    /// Returns an empty `Vec` when no match exists.
    pub fn lookup_name(&self, name: &str) -> Vec<u32> {
        let Some(encoded) = self.name_fst.get(name.as_bytes()) else {
            return Vec::new();
        };
        decode_name_postings(encoded, self.name_postings_bytes())
    }

    /// Return the 1-based source lines where identifier `name` occurs in
    /// this file (usage postings, BUG-006).
    ///
    /// Returns an empty `Vec` when the segment has no usage blobs (file
    /// produced no usage sites) or the name never occurs.
    pub fn lookup_usage_lines(&self, name: &str) -> Vec<u32> {
        let Some(fst) = &self.usages_fst else {
            return Vec::new();
        };
        let Some(encoded) = fst.get(name.as_bytes()) else {
            return Vec::new();
        };
        decode_name_postings(encoded, self.col_bytes(self.usages_postings))
    }

    /// Every distinct usage token in this segment.
    ///
    /// Only the dirty overlay needs this: its freshly indexed segments are not
    /// yet folded into the workspace-wide usage dictionary, so a substring
    /// search would miss tokens that exist only in an edited file.
    pub fn usage_tokens_where(&self, keep: impl Fn(&str) -> bool) -> Vec<String> {
        use fst::Streamer;

        let Some(fst) = &self.usages_fst else {
            return Vec::new();
        };
        let mut tokens = Vec::new();
        let mut stream = fst.stream();
        while let Some((key, _postings)) = stream.next() {
            if let Ok(text) = std::str::from_utf8(key)
                && keep(text)
            {
                tokens.push(text.to_owned());
            }
        }
        tokens
    }

    /// Every mention of `name` in this file, as `(role, 1-based line)` pairs.
    ///
    /// Roles come back in sorted order and a line repeats once per occurrence
    /// on it. Returns empty when the file mentions the name nowhere.
    pub fn lookup_mention_sites(&self, name: &str) -> Vec<(&str, u32)> {
        let mut sites = Vec::new();
        for mention in &self.mentions {
            let Some(encoded) = mention.fst.get(name.as_bytes()) else {
                continue;
            };
            let role: &str = &mention.role;
            sites.extend(
                decode_name_postings(encoded, self.col_bytes(mention.postings))
                    .into_iter()
                    .map(|line| (role, line)),
            );
        }
        sites
    }

    /// Return the raw bytes of the `name_postings` blob (used by overlay builder).
    pub fn name_postings_bytes(&self) -> &[u8] {
        self.col_bytes(self.name_postings)
    }

    /// Return the number of enrichment (extra) column names stored in this segment.
    #[must_use]
    pub const fn extra_col_count(&self) -> usize {
        self.extra_cols.len()
    }

    /// Whether this segment stores an enrichment column named `name`.
    #[must_use]
    pub fn has_extra_col(&self, name: &str) -> bool {
        self.extra_col_range(name).is_some()
    }

    /// Whether this segment stores a value→rows posting index for `field`.
    ///
    /// False for a field whose distinct-value count here exceeded the
    /// per-segment budget: the column is still stored, only the index is not,
    /// so this segment's rows are invisible to any bitmap built from postings.
    #[must_use]
    pub fn posts_field(&self, field: &str) -> bool {
        self.field_blob(field).is_some()
    }

    /// The posting blob of `field`, when this segment posts it.
    fn field_blob(&self, field: &str) -> Option<PostingBlob> {
        self.field_blobs
            .iter()
            .find(|(name, _)| *name == field)
            .map(|(_, blob)| *blob)
    }

    /// The enrichment fields this segment posts, in
    /// `POSTING_ENRICHMENT_FIELDS` order.
    pub(crate) fn posted_fields(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.field_blobs.iter().map(|(name, _)| *name)
    }

    /// Every `(value_id, rows)` pair `field` posts, in file order; empty when
    /// the segment does not post `field`. Each bitmap is decoded from the
    /// mmap as the iterator reaches it, so a walk holds one at a time.
    pub(crate) fn field_postings(
        &self,
        field: &str,
    ) -> impl Iterator<Item = Result<(u32, RoaringBitmap)>> + '_ {
        id_entries(
            self.field_blob(field)
                .unwrap_or_default()
                .entries(&self.mmap),
        )
    }

    /// The rows of `field = <value_id>`, decoded from the mmap on each call.
    /// `None` when the segment posts no such value for the field — callers
    /// that must tell "field not posted" from "value absent" ask
    /// [`Self::posts_field`] first.
    ///
    /// # Errors
    /// Corrupt bitmap bytes, or an entry table that runs past its blob.
    pub(crate) fn field_rows(&self, field: &str, value_id: u32) -> Result<Option<RoaringBitmap>> {
        let Some(blob) = self.field_blob(field) else {
            return Ok(None);
        };
        blob.find_id(&self.mmap, value_id)?.map(decode).transpose()
    }

    /// The rows whose `fql_kind` has string-pool id `kind_id`, decoded from
    /// the mmap on each call; `None` when the segment posts no such kind.
    ///
    /// # Errors
    /// Corrupt bitmap bytes, or an entry table that runs past its blob.
    pub(crate) fn kind_rows(&self, kind_id: u32) -> Result<Option<RoaringBitmap>> {
        self.kind_blob
            .find_id(&self.mmap, kind_id)?
            .map(decode)
            .transpose()
    }

    /// Every `(kind_id, rows)` pair this segment posts, in file order, each
    /// bitmap decoded as the iterator reaches it.
    pub(crate) fn kind_postings(&self) -> impl Iterator<Item = Result<(u32, RoaringBitmap)>> + '_ {
        id_entries(self.kind_blob.entries(&self.mmap))
    }

    /// Whether this segment carries a name-prefix index with at least one
    /// entry. A segment written before the index existed, or holding no
    /// names, has none — its rows cannot be pruned by prefix.
    #[must_use]
    pub(crate) fn has_name_prefix_index(&self) -> bool {
        self.name_prefix_blob.entry_count(&self.mmap) > 0
    }

    /// The rows whose lower-cased name starts with `prefix` (one or two
    /// characters, as the index is keyed), decoded from the mmap on each
    /// call; `None` when no name here does.
    ///
    /// # Errors
    /// Corrupt bitmap bytes, or an entry table that runs past its blob.
    pub(crate) fn name_prefix_rows(&self, prefix: &[u8]) -> Result<Option<RoaringBitmap>> {
        self.name_prefix_blob
            .find_bytes(&self.mmap, prefix)?
            .map(decode)
            .transpose()
    }

    /// Return the hex-encoded content ID of this segment.
    ///
    /// Used by [`DirtyOverlay::staged_hex_ids`] to enumerate the hex IDs of
    /// staged segments without storing a separate `String` field.
    #[must_use]
    pub fn content_id_hex(&self) -> String {
        self.content_id.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    /// Read the symbol name for row `row`.
    pub fn name_of(&self, row: u32) -> &str {
        self.str_in(self.fixed.name_id, row)
    }

    /// Read the raw string-pool ID for the `name` column at `row`.
    ///
    /// Used by [`super::overlay_builder`] to build dedup keys without string allocation.
    pub(crate) fn name_id_of(&self, row: u32) -> u32 {
        self.u32_in(self.fixed.name_id, row)
    }

    /// Read the raw string-pool ID for the `fql_kind` column at `row`.
    ///
    /// Used by [`super::overlay_builder`] to build dedup keys without string allocation.
    pub(crate) fn fql_kind_id_of(&self, row: u32) -> u32 {
        self.u32_in(self.fixed.fql_kind_id, row)
    }

    /// Read the FQL kind string for row `row`.
    pub fn fql_kind_of(&self, row: u32) -> &str {
        self.str_in(self.fixed.fql_kind_id, row)
    }

    /// Read the language string for row `row`.
    pub fn language_of(&self, row: u32) -> &str {
        self.str_in(self.fixed.language_id, row)
    }

    /// Rows whose stored language satisfies `accepts`.
    ///
    /// Exact, not a superset: every row is decided against its own stored
    /// value. A row whose language is empty is skipped without consulting
    /// `accepts`, because `SymbolMatch::field_str` reports `None` for it and
    /// `filter::eval_predicate` fails every operator on an absent field —
    /// a negation included.
    ///
    /// `None` means the language column does not account for this segment's
    /// rows one-for-one, so nothing here may be concluded and the caller must
    /// fall back to the complete scan.
    pub(crate) fn rows_with_language_matching(
        &self,
        accepts: &dyn Fn(&str) -> bool,
    ) -> Option<RoaringBitmap> {
        let blob = self.col_bytes(self.fixed.language_id);
        let ids: &[u32] = cast_slice(blob);
        if ids.len() != self.row_count as usize {
            return None;
        }
        let mut decided: HashMap<u32, bool> = HashMap::new();
        let mut rows = RoaringBitmap::new();
        for (row, &id) in ids.iter().enumerate() {
            let Ok(row) = u32::try_from(row) else {
                return None;
            };
            let keep = *decided.entry(id).or_insert_with(|| {
                let stored = self.strings.get(id);
                !stored.is_empty() && accepts(stored)
            });
            if keep {
                let _ = rows.insert(row);
            }
        }
        Some(rows)
    }

    /// Read the 1-based source line for row `row`.
    pub fn line_of(&self, row: u32) -> u32 {
        self.u32_in(self.fixed.line, row)
    }

    /// Read the byte-range start for row `row`.
    pub fn byte_start_of(&self, row: u32) -> u32 {
        self.u32_in(self.fixed.byte_start, row)
    }

    /// Read the byte-range end for row `row`.
    pub fn byte_end_of(&self, row: u32) -> u32 {
        self.u32_in(self.fixed.byte_end, row)
    }

    /// Read the usages count for row `row`.
    pub fn usages_count_of(&self, row: u32) -> u32 {
        self.u32_in(self.fixed.usages_count, row)
    }

    /// Read the stable node ordinal for row `row`.
    ///
    /// Returns `None` when the column is absent or the slot is the null
    /// sentinel (`u32::MAX`).
    pub fn ordinal_of(&self, row: u32) -> Option<u32> {
        let blob = self.col_bytes(self.fixed.ordinal);
        if blob.is_empty() {
            return None;
        }
        let slice: &[u32] = cast_slice(blob);
        match slice.get(row as usize).copied() {
            Some(u32::MAX) | None => None,
            Some(v) => Some(v),
        }
    }

    /// Read the parent ordinal for `row` (`u32::MAX` = top-level node).
    pub fn parent_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.col_bytes(self.fixed.parent_ordinal);
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read the rev handle for `row` (first 8 bytes of SHA-256 of node bytes, LE u64).
    /// Returns `0` for analysis-only rows or when the column is absent.
    pub fn rev_of(&self, row: u32) -> u64 {
        let blob = self.col_bytes(self.fixed.rev);
        let start = row as usize * 8;
        let end = start + 8;
        if blob.len() < end {
            return 0;
        }
        u64::from_le_bytes(blob[start..end].try_into().unwrap_or([0u8; 8]))
    }

    /// Read the first-child ordinal for `row` (`u32::MAX` = no children).
    pub fn first_child_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.col_bytes(self.fixed.first_child_ordinal);
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read the next-sibling ordinal for `row` (`u32::MAX` = no next sibling).
    pub fn next_sibling_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.col_bytes(self.fixed.next_sibling_ordinal);
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read the prev-sibling ordinal for `row` (`u32::MAX` = no prev sibling).
    pub fn prev_sibling_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.col_bytes(self.fixed.prev_sibling_ordinal);
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read an enrichment field value for row `row`.
    ///
    /// Returns `None` when the column is absent or the row's slot is `NULL`
    /// (encoded as `u32::MAX` in the segment).
    pub fn extra_field_str(&self, col: &str, row: u32) -> Option<&str> {
        self.opt_str_in(self.col_range(col), row)
    }

    /// Collect all enrichment field values for `row` into a `HashMap`.
    ///
    /// Mirrors the field-collection loop in [`Self::materialize_rows`] but for a
    /// single row.  Returns an empty map when no enrichment columns are present.
    pub(crate) fn enrichment_for_row(&self, row: u32) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for extra in &self.extra_cols {
            let blob = self.col_bytes(extra.data);
            if blob.is_empty() {
                continue;
            }
            let slice: &[u32] = cast_slice(blob);
            if let Some(&id) = slice.get(row as usize) {
                if id != u32::MAX {
                    let s = self.strings.get(id);
                    if !s.is_empty() {
                        let _ = map.insert((*extra.name).to_owned(), s.to_owned());
                    }
                }
            }
        }
        map
    }

    /// Iterate the segment's enrichment columns as `(name, value ids)`.
    ///
    /// This is [`Self::enrichment_for_row`] turned inside out, and it is the
    /// form to reach for when reading many rows: the value ids are handed over
    /// as the `&[u32]` column they already are on disk, indexed by row, so a
    /// caller can walk rows without materialising a single `String`.
    ///
    /// A value id indexes THIS segment's string table.  Two segments can spell
    /// the same text with different ids, so a `(column, id)` pair identifies a
    /// value only within one segment — resolve ids with
    /// [`Self::string_of_id`] before comparing anything across segments.
    ///
    /// `u32::MAX` is the NULL slot, and a row past the end of a column has no
    /// value for it.  An absent column yields an empty slice rather than being
    /// skipped, so every segment reports the same column list its header does.
    pub(crate) fn enrichment_columns(&self) -> impl Iterator<Item = (&str, &[u32])> {
        self.extra_cols.iter().map(|extra| {
            let blob = self.col_bytes(extra.data);
            let ids: &[u32] = if blob.is_empty() {
                &[]
            } else {
                cast_slice(blob)
            };
            (extra.name.as_ref(), ids)
        })
    }

    // ─────────────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────────────

    /// The bytes at a range resolved at open — a column's values, or a
    /// postings blob.
    ///
    /// Anything the segment does not store resolved to `(0, 0)`, which slices
    /// to the same empty blob a missing TOC entry used to yield.
    fn col_bytes(&self, range: ColRange) -> &[u8] {
        &self.mmap[range.0..range.1]
    }

    /// A resolved column's bytes viewed as `u32` values; empty when absent.
    fn col_u32(&self, range: ColRange) -> &[u32] {
        cast_slice(self.col_bytes(range))
    }

    /// Byte range of the `col_<col>` blob, resolved at open.
    ///
    /// Enrichment columns first — every caller names one of those — then the
    /// fixed columns, so every `col_*` blob a segment holds stays reachable by
    /// name, exactly as when the name was formatted and hashed per access.
    fn col_range(&self, col: &str) -> ColRange {
        if let Some(range) = self.extra_col_range(col) {
            return range;
        }
        self.fixed.by_short_name(col)
    }

    /// Return a u32 column value at `row`.
    /// `0` when the column is absent or `row` is past its end.
    fn u32_in(&self, range: ColRange, row: u32) -> u32 {
        let blob = self.col_bytes(range);
        if blob.is_empty() {
            return 0;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(0)
    }

    /// Resolve a string-id column to its pool string at `row`.
    fn str_in(&self, range: ColRange, row: u32) -> &str {
        let id = self.u32_in(range, row);
        self.strings.get(id)
    }

    /// Byte range of the enrichment column named `col`, or `None` when this
    /// segment stores no such column.
    ///
    /// Unlike [`Self::col_range`] this never falls back to a fixed column: a
    /// clause naming `line` must not be answered by reading the line column as
    /// a string id.
    fn extra_col_range(&self, col: &str) -> Option<ColRange> {
        self.extra_cols
            .iter()
            .find(|extra| extra.name.as_ref() == col)
            .map(|extra| extra.data)
    }

    /// Resolve a string-id column to its pool string at `row`, reporting an
    /// unset slot (`u32::MAX`) and the empty string as absent.
    ///
    /// Those are the same two conditions under which `materialize_rows` leaves
    /// the column out of a row's enrichment map, so a clause evaluated here and
    /// the same clause evaluated on the built row see the same value.
    fn opt_str_in(&self, range: ColRange, row: u32) -> Option<&str> {
        let blob = self.col_bytes(range);
        if blob.is_empty() {
            return None;
        }
        let slice: &[u32] = cast_slice(blob);
        let id = slice.get(row as usize).copied()?;
        if id == u32::MAX {
            return None;
        }
        non_empty(self.strings.get(id))
    }

    /// Which column of this segment answers the canonical clause field
    /// `field`, given whether the caller supplied the file's path.
    ///
    /// The names a materialised row answers from its own struct are
    /// deliberately [`RowField::Unanswerable`]: `node_kind` is never stored in
    /// a segment, `node_id` is built during materialisation, `usages` is
    /// overwritten from the workspace overlay afterwards, and `count` is
    /// assigned later still by GROUP BY. A predicate on one of them is left to
    /// run against the built rows rather than answered from a column.
    ///
    /// A field no column holds is [`RowField::Absent`] rather than
    /// unanswerable, which is a different thing: the built row's map would not
    /// carry it either, so the two readers agree on `None` and the predicate is
    /// answered here.
    pub(crate) fn row_field(&self, field: &str, has_path: bool) -> RowField {
        let Some(index) = STRUCT_BACKED_FIELDS.iter().position(|name| *name == field) else {
            // Not struct-backed. If a column holds it, read that; otherwise the
            // row this segment would build carries it in neither its struct nor
            // its enrichment map, so both readers answer `None` and saying so
            // early is the same answer said sooner.
            //
            // Unless something writes it onto the row after the columns are
            // read. `body` is read out of the file as the row is materialised
            // and `role` is written onto an occurrence row by the read pass, so
            // for those two "no column" is not "nothing", and they wait for the
            // row like `usages` does.
            return self.extra_col_range(field).map_or_else(
                || {
                    if crate::field_tiers::written_after_materialisation(field) {
                        RowField::Unanswerable
                    } else {
                        RowField::Absent
                    }
                },
                RowField::Extra,
            );
        };
        // Only the colliding name falls back. An enrichment column named after
        // one struct-backed field says nothing about the others, so a segment
        // carrying one still answers every field it does not shadow. The mask
        // stands for the column-name scan this used to run per call, over the
        // same names and resolved at open.
        if self.shadowed_struct_fields & (1 << index) != 0 {
            return RowField::Unanswerable;
        }
        match field {
            "name" => RowField::Name,
            "fql_kind" => RowField::FqlKind,
            "language" => RowField::Language,
            "line" => RowField::Line,
            "path" if has_path => RowField::Path,
            _ => RowField::Unanswerable,
        }
    }

    /// Whether a row view over this segment answers `field` the way the row it
    /// would build answers it.
    ///
    /// A caller filters a predicate before materialisation only when this is
    /// true; a predicate it is false for is not dropped, it is left for the
    /// filter that runs on the built rows.
    ///
    /// **Reporting a confident absence counts as answering.** A field no
    /// column of this segment holds is one the built row carries in neither
    /// its struct nor its enrichment map, so both readers resolve it to
    /// `None` and every operator that consults the field is false on both —
    /// `!=` and `NOT LIKE` included, since each arm of
    /// [`crate::filter::eval_predicate_on`] is `is_some_and`. Only
    /// [`RowField::Unanswerable`] is false here: those are the names where the
    /// built row reads from somewhere no view can see.
    pub(crate) fn answers_field(&self, field: &str, has_path: bool) -> bool {
        !matches!(self.row_field(field, has_path), RowField::Unanswerable)
    }

    /// Look up a string-pool entry by ID.
    ///
    /// Used by `OverlayBuilder` to resolve per-segment `kind_id` values
    /// (from `self.kind_postings` keys) back to their string representation
    /// without exposing `StringPool` outside this module.
    pub(crate) fn string_of_id(&self, id: u32) -> &str {
        self.strings.get(id)
    }

    /// Build the candidate row bitmap using Roaring postings.
    ///
    /// Handles only `WHERE fql_kind = 'X'` (exact equality) predicates;
    /// all other predicates fall through to the `apply_clauses` residual
    /// filter.  Multiple fql_kind predicates are AND'd.
    fn prefilter_kind(&self, clauses: &Clauses) -> RoaringBitmap {
        let mut result: Option<RoaringBitmap> = None;

        for pred in &clauses.where_predicates {
            if pred.field == "fql_kind" && pred.op == CompareOp::Eq {
                if let PredicateValue::String(ref kind_val) = pred.value {
                    let Some(kind_id) = self.strings.id_of(kind_val.as_str()) else {
                        // Kind not present in this segment → no candidates.
                        return RoaringBitmap::new();
                    };
                    let bm = match self.kind_rows(kind_id) {
                        Ok(rows) => rows.unwrap_or_default(),
                        Err(e) => {
                            // A bitmap that will not decode cannot narrow: every
                            // row stays a candidate and the residual filter
                            // decides — complete, slower, never a false negative.
                            tracing::warn!(
                                segment = %self.content_id_hex(),
                                kind = %kind_val,
                                "postings_fql_kind unreadable, not narrowing: {e:#}"
                            );
                            (0..self.row_count).collect()
                        }
                    };
                    result = Some(match result {
                        Some(prev) => prev & bm,
                        None => bm,
                    });
                }
            }
        }

        result.unwrap_or_else(|| (0..self.row_count).collect())
    }

    /// `true` when this segment can PROVE that no row of its own carries
    /// `field = value`.
    ///
    /// Answered from the per-segment postings, which are keyed by raw row id
    /// and so cover every row the segment holds — including rows the workspace
    /// overlay never keys, either because they lost the per-segment
    /// `(name, fql_kind, line)` dedup or because their whole path is shadowed
    /// by another segment. That is what makes this, rather than the overlay's
    /// key set, the honest place to ask the question.
    ///
    /// Returns `false` — "cannot prove it" — when the segment holds the column
    /// but no postings blob for it, which is what the builder does when the
    /// field's per-segment cardinality exceeds its cap. Those rows are
    /// invisible here, so the caller must keep the complete scan.
    pub(crate) fn proves_enrichment_value_absent(&self, field: &str, value: &str) -> bool {
        if !self.posts_field(field) {
            return !self.has_extra_col(field);
        }
        let Some(value_id) = self.strings.id_of(value) else {
            // The value is not even in this segment's string pool.
            return true;
        };
        match self.field_rows(field, value_id) {
            Ok(None) => true,
            Ok(Some(bm)) => bm.is_empty(),
            // An unreadable bitmap proves nothing.
            Err(_) => false,
        }
    }

    /// Narrow `local_rows` using per-segment enrichment posting bitmaps.
    ///
    /// For each `WHERE <field> = '<value>'` predicate where `<field>` has a
    /// posting file loaded, intersects `local_rows` with the matching bitmap.
    ///
    /// Returns the narrowed bitmap.  When no enrichment posting is available
    /// for a predicate the predicate is left to the residual `apply_clauses`
    /// filter (safe — correctness is never compromised, only performance).
    pub(crate) fn prefilter_enrichment_postings(
        &self,
        local_rows: RoaringBitmap,
        clauses: &Clauses,
    ) -> RoaringBitmap {
        let mut rows = local_rows;
        for pred in &clauses.where_predicates {
            if pred.op != CompareOp::Eq {
                continue;
            }
            let PredicateValue::String(ref val) = pred.value else {
                continue;
            };
            if !self.posts_field(&pred.field) {
                continue;
            }
            let Some(value_id) = self.strings.id_of(val.as_str()) else {
                // Value not in this segment's pool → no rows can match.
                return RoaringBitmap::new();
            };
            let bm = match self.field_rows(&pred.field, value_id) {
                Ok(Some(bm)) => bm,
                Ok(None) => {
                    // Value is in the pool but has no rows with this field value.
                    return RoaringBitmap::new();
                }
                Err(e) => {
                    // A bitmap that will not decode cannot narrow; the
                    // predicate is left to the residual filter, like a field
                    // with no posting file at all.
                    tracing::warn!(
                        segment = %self.content_id_hex(),
                        field = %pred.field,
                        "posting bitmap unreadable, not narrowing: {e:#}"
                    );
                    continue;
                }
            };
            rows &= bm;
            if rows.is_empty() {
                return rows;
            }
        }
        rows
    }

    /// Materialise `rows` into `Vec<SymbolMatch>`.
    ///
    /// Exposed as `pub(crate)` so [`ColumnarStorage`] can call it directly
    /// for efficient batched row resolution without going through `find_symbols`.
    pub(crate) fn materialize_rows(
        &self,
        rows: &RoaringBitmap,
        source_path: Option<&Path>,
    ) -> Vec<SymbolMatch> {
        // Every column this batch reads is resolved once, here: a column's
        // range is a property of the segment, not of the row being read, so
        // nothing below the loop needs to name a column again.
        let name_ids = self.col_u32(self.fixed.name_id);
        let kind_ids = self.col_u32(self.fixed.fql_kind_id);
        let language_ids = self.col_u32(self.fixed.language_id);
        let lines = self.col_u32(self.fixed.line);
        let usage_counts = self.col_u32(self.fixed.usages_count);
        let extras: Vec<(&str, &[u32])> = self
            .extra_cols
            .iter()
            .map(|extra| (extra.name.as_ref(), self.col_u32(extra.data)))
            .collect();

        rows.iter()
            .map(|row| {
                let idx = row as usize;
                let name = self
                    .strings
                    .get(name_ids.get(idx).copied().unwrap_or(0))
                    .to_owned();
                let fql_kind = self
                    .strings
                    .get(kind_ids.get(idx).copied().unwrap_or(0))
                    .to_owned();
                let language = self
                    .strings
                    .get(language_ids.get(idx).copied().unwrap_or(0))
                    .to_owned();
                let line = lines.get(idx).copied().unwrap_or(0);
                let usages = usage_counts.get(idx).copied().unwrap_or(0);

                let mut fields: HashMap<String, String> = HashMap::new();
                for (col_name, ids) in &extras {
                    if ids.is_empty() {
                        continue;
                    }
                    if let Some(&id) = ids.get(idx) {
                        if id != u32::MAX {
                            let s = self.strings.get(id);
                            if !s.is_empty() {
                                let _ = fields.insert((*col_name).to_owned(), s.to_owned());
                            }
                        }
                    }
                }

                SymbolMatch {
                    name,
                    node_kind: None, // segments do not store node_kind
                    fql_kind: if fql_kind.is_empty() {
                        None
                    } else {
                        Some(fql_kind)
                    },
                    language: if language.is_empty() {
                        None
                    } else {
                        Some(language)
                    },
                    path: source_path.map(ToOwned::to_owned),
                    line: if line == 0 { None } else { Some(line as usize) },
                    usages_count: Some(usages as usize),
                    fields,
                    count: None,
                    node_id: source_path.and_then(|p| {
                        self.ordinal_of(row)
                            .map(|ord| crate::node_id::make_node_id(&p.to_string_lossy(), ord))
                    }),
                    rev: self
                        .ordinal_of(row)
                        .map(|_| crate::node_id::format_rev(self.rev_of(row)))
                        .filter(|r| !r.is_empty()),
                }
            })
            .collect()
    }

    /// Materialise a single row by local row index.
    ///
    /// Equivalent to calling `materialize_rows` with a single-element bitmap
    /// but avoids constructing a `RoaringBitmap`.  Returns `None` when
    /// `local_row_idx >= row_count`.
    pub(crate) fn materialize_one_row(
        &self,
        local_row_idx: u32,
        source_path: &Path,
    ) -> Option<SymbolMatch> {
        if local_row_idx >= self.row_count {
            return None;
        }
        let row = local_row_idx;
        let name = self.str_in(self.fixed.name_id, row).to_owned();
        let fql_kind = self.str_in(self.fixed.fql_kind_id, row).to_owned();
        let language = self.str_in(self.fixed.language_id, row).to_owned();
        let line = self.u32_in(self.fixed.line, row);
        let usages = self.u32_in(self.fixed.usages_count, row);

        let mut fields: HashMap<String, String> = HashMap::new();
        for extra in &self.extra_cols {
            let blob = self.col_bytes(extra.data);
            if blob.is_empty() {
                continue;
            }
            let slice: &[u32] = cast_slice(blob);
            if let Some(&id) = slice.get(row as usize) {
                if id != u32::MAX {
                    let s = self.strings.get(id);
                    if !s.is_empty() {
                        let _ = fields.insert((*extra.name).to_owned(), s.to_owned());
                    }
                }
            }
        }

        Some(SymbolMatch {
            name,
            node_kind: None,
            fql_kind: if fql_kind.is_empty() {
                None
            } else {
                Some(fql_kind)
            },
            language: if language.is_empty() {
                None
            } else {
                Some(language)
            },
            path: Some(source_path.to_owned()),
            line: if line == 0 { None } else { Some(line as usize) },
            usages_count: Some(usages as usize),
            fields,
            count: None,
            node_id: self
                .ordinal_of(row)
                .map(|ord| crate::node_id::make_node_id(&source_path.to_string_lossy(), ord)),
            // The handle and its rev travel together: a row you can address is a
            // row you can safely mutate, without a second read to fetch the rev.
            rev: self
                .ordinal_of(row)
                .map(|_| crate::node_id::format_rev(self.rev_of(row)))
                .filter(|r| !r.is_empty()),
        })
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests;
