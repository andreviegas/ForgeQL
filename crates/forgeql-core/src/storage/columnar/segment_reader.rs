//! [`SegmentReader`] — mmap-based reader for `.fqsf` single-file columnar segments.
//!
//! Opens a segment file written by [`SegmentBuilder`], validates the outer
//! `FQSF` magic and the inner `FQSG` header blob, mmaps the whole file with
//! a single `Mmap`, parses the TOC to locate every named blob, and
//! deserialises Roaring bitmap posting lists and the FST from their blobs.
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
    clippy::unused_self,               // u32_of dispatches on col+row, not self; retained as method for symmetry
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

// ─────────────────────────────────────────────────────────────────────────────
// Format constants (must match segment_builder.rs)
// ─────────────────────────────────────────────────────────────────────────────

const SEGMENT_SCHEMA_VERSION: u32 = 1;
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
    /// Pre-built reverse map for O(1) kind-prefilter lookups.
    reverse: HashMap<String, u32>,
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

        let mut pool = Self {
            mmap,
            off_start,
            off_end,
            dat_start,
            dat_end,
            string_count,
            reverse: HashMap::new(),
        };

        // Build reverse map in one pass so prefilter lookups are O(1).
        for id in 0..string_count {
            let s = pool.get(id).to_owned();
            let _ = pool.reverse.insert(s, id);
        }

        Ok(pool)
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
// SegmentReader
// ─────────────────────────────────────────────────────────────────────────────

/// Mmap-based read-only view of a single `.fqsf` columnar segment file.
///
/// Open with [`SegmentReader::open`].  The reader holds one `Arc<Mmap>` for
/// the whole file; individual blobs are accessed as subslices.
pub struct SegmentReader {
    /// Whole-file mmap shared with the string pool.
    mmap: Arc<Mmap>,
    /// TOC: blob name → `(start, end)` byte offsets within `mmap`.
    blobs: HashMap<String, (usize, usize)>,
    /// Absolute path of the opened `.fqsf` file (for diagnostics).
    pub path: PathBuf,
    /// Number of rows stored in this segment.
    pub row_count: u32,
    /// Provider ID decoded from the header blob.
    pub provider_id: String,
    /// Raw content ID bytes (length matches the provider's hash width).
    pub content_id: Vec<u8>,
    /// Enrichment column names discovered from the header blob.
    extra_col_names: Vec<String>,
    strings: StringPool,
    pub(crate) kind_postings: HashMap<u32, RoaringBitmap>,
    pub(crate) field_postings: HashMap<String, HashMap<u32, RoaringBitmap>>,
    pub(crate) zone_maps: HashMap<String, (u32, u32)>,
    pub(crate) name_prefix: HashMap<Vec<u8>, RoaringBitmap>,
    pub(crate) name_fst: FstMap<MmapSlice>,
}

impl SegmentReader {
    /// Open and validate a `.fqsf` segment file.
    ///
    /// Mmaps the whole file, parses the outer FQSF TOC, validates the inner
    /// `FQSG` header blob, builds the string pool, and deserialises Roaring
    /// bitmap postings and the FST.
    ///
    /// # Errors
    /// Returns `Err` on I/O failure, missing file, format mismatch, schema
    /// version mismatch, or corrupt string pool.
    pub fn open(path: &Path) -> Result<Self> {
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

        // ── 3. Parse TOC ──────────────────────────────────────────────────
        let entry_count =
            u32::from_le_bytes(mmap[8..12].try_into().context("FQSF entry_count bytes")?) as usize;
        let toc_end = 12 + entry_count * TOC_ENTRY_SIZE;
        ensure!(
            file_len >= toc_end,
            "segment {} too short for TOC (need {toc_end} bytes, have {file_len})",
            path.display()
        );

        let mut blobs: HashMap<String, (usize, usize)> = HashMap::with_capacity(entry_count);
        for i in 0..entry_count {
            let es = 12 + i * TOC_ENTRY_SIZE;
            let entry = &mmap[es..es + TOC_ENTRY_SIZE];
            let name_end = entry[..ENTRY_NAME_LEN]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(ENTRY_NAME_LEN);
            let name = std::str::from_utf8(&entry[..name_end])
                .with_context(|| format!("blob name at TOC index {i}"))?
                .to_owned();
            let offset = u32::from_le_bytes(
                entry[ENTRY_NAME_LEN..ENTRY_NAME_LEN + 4]
                    .try_into()
                    .context("blob offset")?,
            ) as usize;
            let len = u32::from_le_bytes(
                entry[ENTRY_NAME_LEN + 4..ENTRY_NAME_LEN + 8]
                    .try_into()
                    .context("blob length")?,
            ) as usize;
            ensure!(
                offset + len <= file_len,
                "blob '{name}' extends beyond file end ({offset} + {len} > {file_len})"
            );
            let _ = blobs.insert(name, (offset, offset + len));
        }

        // ── 4. Parse inner FQSG header blob ──────────────────────────────
        let &(hs, he) = blobs
            .get("header")
            .context("missing 'header' blob in FQSF")?;
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
        let extra_col_names: Vec<String> = columns
            .iter()
            .filter(|(name, tag)| {
                !CORE_COLUMN_NAMES.contains(&name.as_str()) && *tag == TYPE_TAG_STR_OPT
            })
            .map(|(name, _)| name.clone())
            .collect();

        // ── 6. String pool ────────────────────────────────────────────────
        let off_range = blobs.get("strings_offsets").copied().unwrap_or((0, 0));
        let dat_range = blobs.get("strings_data").copied().unwrap_or((0, 0));
        let strings =
            StringPool::from_blobs(Arc::clone(&mmap), off_range, dat_range, string_count)?;

        // ── 7. Roaring postings ───────────────────────────────────────────
        let kind_postings = {
            let data = blob_slice(&blobs, &mmap, "postings_fql_kind");
            load_kind_postings(data)?
        };
        let field_postings = load_enrichment_postings(&blobs, &mmap)?;
        let zone_maps = load_zone_maps(&blobs, &mmap)?;

        // ── 8. FST + name prefix ──────────────────────────────────────────
        let (fst_start, fst_end) = blobs.get("name_fst").copied().unwrap_or((0, 0));
        let name_fst = FstMap::new(MmapSlice {
            mmap: Arc::clone(&mmap),
            start: fst_start,
            end: fst_end,
        })
        .context("parsing name_fst blob")?;
        let name_prefix = {
            let data = blob_slice(&blobs, &mmap, "name_prefix");
            load_name_prefix(data)?
        };

        Ok(Self {
            mmap,
            blobs,
            path: path.to_owned(),
            row_count,
            provider_id,
            content_id,
            extra_col_names,
            strings,
            kind_postings,
            field_postings,
            zone_maps,
            name_prefix,
            name_fst,
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

    /// Return the raw bytes of the `name_postings` blob (used by overlay builder).
    pub fn name_postings_bytes(&self) -> &[u8] {
        self.blob_bytes("name_postings")
    }

    /// Return the number of enrichment (extra) column names stored in this segment.
    #[must_use]
    pub const fn extra_col_count(&self) -> usize {
        self.extra_col_names.len()
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
        self.str_of("name_id", row)
    }

    /// Read the raw string-pool ID for the `name` column at `row`.
    ///
    /// Used by [`super::overlay_builder`] to build dedup keys without string allocation.
    pub(crate) fn name_id_of(&self, row: u32) -> u32 {
        self.u32_at("name_id", row)
    }

    /// Read the raw string-pool ID for the `fql_kind` column at `row`.
    ///
    /// Used by [`super::overlay_builder`] to build dedup keys without string allocation.
    pub(crate) fn fql_kind_id_of(&self, row: u32) -> u32 {
        self.u32_at("fql_kind_id", row)
    }

    /// Read the FQL kind string for row `row`.
    pub fn fql_kind_of(&self, row: u32) -> &str {
        self.str_of("fql_kind_id", row)
    }

    /// Read the language string for row `row`.
    pub fn language_of(&self, row: u32) -> &str {
        self.str_of("language_id", row)
    }

    /// Read the 1-based source line for row `row`.
    pub fn line_of(&self, row: u32) -> u32 {
        self.u32_at("line", row)
    }

    /// Read the byte-range start for row `row`.
    pub fn byte_start_of(&self, row: u32) -> u32 {
        self.u32_at("byte_start", row)
    }

    /// Read the byte-range end for row `row`.
    pub fn byte_end_of(&self, row: u32) -> u32 {
        self.u32_at("byte_end", row)
    }

    /// Read the usages count for row `row`.
    pub fn usages_count_of(&self, row: u32) -> u32 {
        self.u32_at("usages_count", row)
    }

    /// Read the stable node ordinal for row `row`.
    ///
    /// Returns `None` when the column is absent or the slot is the null
    /// sentinel (`u32::MAX`).
    pub fn ordinal_of(&self, row: u32) -> Option<u32> {
        let blob = self.blob_bytes("col_ordinal");
        if blob.is_empty() {
            return None;
        }
        let slice: &[u32] = cast_slice(blob);
        match slice.get(row as usize).copied() {
            Some(u32::MAX) | None => None,
            Some(v) => Some(v),
        }
    }

    /// Read an enrichment field value for row `row`.
    ///
    /// Returns `None` when the column is absent or the row's slot is `NULL`
    /// (encoded as `u32::MAX` in the segment).
    pub fn extra_field_str(&self, col: &str, row: u32) -> Option<&str> {
        let blob = self.blob_bytes(&format!("col_{col}"));
        if blob.is_empty() {
            return None;
        }
        let slice: &[u32] = cast_slice(blob);
        let id = slice.get(row as usize).copied()?;
        if id == u32::MAX {
            None
        } else {
            let s = self.strings.get(id);
            if s.is_empty() { None } else { Some(s) }
        }
    }

    /// Collect all enrichment field values for `row` into a `HashMap`.
    ///
    /// Mirrors the field-collection loop in [`Self::materialize_rows`] but for a
    /// single row.  Returns an empty map when no enrichment columns are present.
    pub(crate) fn enrichment_for_row(&self, row: u32) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for col_name in &self.extra_col_names {
            let blob = self.blob_bytes(&format!("col_{col_name}"));
            if blob.is_empty() {
                continue;
            }
            let slice: &[u32] = cast_slice(blob);
            if let Some(&id) = slice.get(row as usize) {
                if id != u32::MAX {
                    let s = self.strings.get(id);
                    if !s.is_empty() {
                        let _ = map.insert(col_name.clone(), s.to_owned());
                    }
                }
            }
        }
        map
    }

    // ─────────────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Return the byte range for a named blob as a `&[u8]` slice.
    /// Returns `&[]` when the blob is absent.
    fn blob_bytes(&self, name: &str) -> &[u8] {
        let Some(&(start, end)) = self.blobs.get(name) else {
            return &[];
        };
        &self.mmap[start..end]
    }

    /// Return a u32 column value at `row`.
    /// `col` is the short column name (without the `col_` prefix).
    pub(crate) fn u32_at(&self, col: &str, row: u32) -> u32 {
        let blob = self.blob_bytes(&format!("col_{col}"));
        if blob.is_empty() {
            return 0;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(0)
    }

    /// Resolve a string-id column to its pool string at `row`.
    fn str_of(&self, col: &str, row: u32) -> &str {
        let id = self.u32_at(col, row);
        self.strings.get(id)
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
                    let bm = if let Some(&kind_id) = self.strings.reverse.get(kind_val.as_str()) {
                        self.kind_postings
                            .get(&kind_id)
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        // Kind not present in this segment → no candidates.
                        return RoaringBitmap::new();
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
            let Some(field_map) = self.field_postings.get(&pred.field) else {
                continue;
            };
            let Some(&value_id) = self.strings.reverse.get(val.as_str()) else {
                // Value not in this segment's pool → no rows can match.
                return RoaringBitmap::new();
            };
            let Some(bm) = field_map.get(&value_id) else {
                // Value is in the pool but has no rows with this field value.
                return RoaringBitmap::new();
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
        rows.iter()
            .map(|row| {
                let name = self.str_of("name_id", row).to_owned();
                let fql_kind = self.str_of("fql_kind_id", row).to_owned();
                let language = self.str_of("language_id", row).to_owned();
                let line = self.u32_at("line", row);
                let usages = self.u32_at("usages_count", row);

                let mut fields: HashMap<String, String> = HashMap::new();
                for col_name in &self.extra_col_names {
                    let blob = self.blob_bytes(&format!("col_{col_name}"));
                    if blob.is_empty() {
                        continue;
                    }
                    let slice: &[u32] = cast_slice(blob);
                    if let Some(&id) = slice.get(row as usize) {
                        if id != u32::MAX {
                            let s = self.strings.get(id);
                            if !s.is_empty() {
                                let _ = fields.insert(col_name.clone(), s.to_owned());
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
        let name = self.str_of("name_id", row).to_owned();
        let fql_kind = self.str_of("fql_kind_id", row).to_owned();
        let language = self.str_of("language_id", row).to_owned();
        let line = self.u32_at("line", row);
        let usages = self.u32_at("usages_count", row);

        let mut fields: HashMap<String, String> = HashMap::new();
        for col_name in &self.extra_col_names {
            let blob = self.blob_bytes(&format!("col_{col_name}"));
            if blob.is_empty() {
                continue;
            }
            let slice: &[u32] = cast_slice(blob);
            if let Some(&id) = slice.get(row as usize) {
                if id != u32::MAX {
                    let s = self.strings.get(id);
                    if !s.is_empty() {
                        let _ = fields.insert(col_name.clone(), s.to_owned());
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
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Private blob helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Return a blob byte slice from the blobs map; `&[]` when absent.
fn blob_slice<'m>(blobs: &HashMap<String, (usize, usize)>, mmap: &'m Mmap, name: &str) -> &'m [u8] {
    let Some(&(start, end)) = blobs.get(name) else {
        return &[];
    };
    &mmap[start..end]
}

/// Parse column metadata entries from the header byte slice.
///
/// Each entry: `[u8: name_len][u8 × name_len: name][u8: type_tag][u64 LE: element_count]`
fn parse_column_entries(
    header: &[u8],
    start: usize,
    column_count: u32,
) -> Result<Vec<(String, u8)>> {
    let mut pos = start;
    let mut cols = Vec::with_capacity(column_count as usize);

    for i in 0..column_count {
        ensure!(
            pos < header.len(),
            "header truncated at column entry {i}: pos {pos} ≥ len {}",
            header.len()
        );
        let name_len = header[pos] as usize;
        pos += 1;
        ensure!(
            pos + name_len + 1 + 8 <= header.len(),
            "column entry {i} is truncated (name_len={name_len})"
        );
        let name = std::str::from_utf8(&header[pos..pos + name_len])
            .with_context(|| format!("column {i} name is not valid UTF-8"))?
            .to_owned();
        pos += name_len;
        let type_tag = header[pos];
        pos += 1 + 8; // type_tag + element_count (u64, not used by reader)
        cols.push((name, type_tag));
    }

    Ok(cols)
}

/// Deserialise the `postings_fql_kind` blob into `HashMap<kind_id, RoaringBitmap>`.
///
/// Format: `[kind_count: u32] (kind_id: u32, bitmap_len: u32, bitmap_bytes)*`
fn load_kind_postings(data: &[u8]) -> Result<HashMap<u32, RoaringBitmap>> {
    if data.len() < 4 {
        return Ok(HashMap::new());
    }

    let kind_count = u32::from_le_bytes(data[..4].try_into().context("kind_count bytes")?) as usize;
    let mut map = HashMap::with_capacity(kind_count);
    let mut pos = 4usize;

    for entry in 0..kind_count {
        ensure!(
            pos + 8 <= data.len(),
            "postings_fql_kind blob truncated at entry {entry}"
        );
        let kind_id = u32::from_le_bytes(data[pos..pos + 4].try_into().context("kind_id bytes")?);
        let bitmap_len = u32::from_le_bytes(
            data[pos + 4..pos + 8]
                .try_into()
                .context("bitmap_len bytes")?,
        ) as usize;
        pos += 8;
        ensure!(
            pos + bitmap_len <= data.len(),
            "postings_fql_kind bitmap truncated at entry {entry}"
        );
        let bitmap = RoaringBitmap::deserialize_from(&data[pos..pos + bitmap_len])
            .with_context(|| format!("deserialising bitmap for kind_id {kind_id}"))?;
        pos += bitmap_len;
        let _ = map.insert(kind_id, bitmap);
    }

    Ok(map)
}

/// Load per-field enrichment posting blobs for fields in [`POSTING_ENRICHMENT_FIELDS`].
///
/// For each field, looks up blob `postings_<field>` in `blobs`.
/// Missing blobs are silently skipped (callers fall back to linear scan).
fn load_enrichment_postings(
    blobs: &HashMap<String, (usize, usize)>,
    mmap: &Mmap,
) -> Result<HashMap<String, HashMap<u32, RoaringBitmap>>> {
    let mut result: HashMap<String, HashMap<u32, RoaringBitmap>> = HashMap::new();

    for &field in POSTING_ENRICHMENT_FIELDS {
        let blob_name = format!("postings_{field}");
        let data = blob_slice(blobs, mmap, &blob_name);
        if data.len() < 4 {
            continue;
        }

        let value_count =
            u32::from_le_bytes(data[..4].try_into().context("value_count bytes")?) as usize;
        let mut bitmap_map: HashMap<u32, RoaringBitmap> = HashMap::with_capacity(value_count);
        let mut pos = 4usize;

        for entry in 0..value_count {
            ensure!(
                pos + 8 <= data.len(),
                "postings_{field} blob truncated at entry {entry}"
            );
            let value_id =
                u32::from_le_bytes(data[pos..pos + 4].try_into().context("value_id bytes")?);
            let bitmap_len = u32::from_le_bytes(
                data[pos + 4..pos + 8]
                    .try_into()
                    .context("bitmap_len bytes")?,
            ) as usize;
            pos += 8;
            ensure!(
                pos + bitmap_len <= data.len(),
                "postings_{field} bitmap truncated at entry {entry}"
            );
            let bitmap = RoaringBitmap::deserialize_from(&data[pos..pos + bitmap_len])
                .with_context(|| {
                    format!("deserialising enrichment bitmap for {field} value_id {value_id}")
                })?;
            pos += bitmap_len;
            let _ = bitmap_map.insert(value_id, bitmap);
        }

        let _ = result.insert(field.to_owned(), bitmap_map);
    }

    Ok(result)
}

/// Load zone maps from `zonemap_<col>` blobs.
fn load_zone_maps(
    blobs: &HashMap<String, (usize, usize)>,
    mmap: &Mmap,
) -> Result<HashMap<String, (u32, u32)>> {
    let mut result: HashMap<String, (u32, u32)> = HashMap::new();
    for (col_name, _has_sentinel) in ZONEMAP_NUMERIC_FIELDS {
        let blob_name = format!("zonemap_{col_name}");
        let data = blob_slice(blobs, mmap, &blob_name);
        if data.len() < 8 {
            continue;
        }
        let min = u32::from_le_bytes(data[..4].try_into().context("zonemap min bytes")?);
        let max = u32::from_le_bytes(data[4..8].try_into().context("zonemap max bytes")?);
        let _ = result.insert((*col_name).to_owned(), (min, max));
    }
    Ok(result)
}

/// Decode FST-encoded name posting.
///
/// FST value layout: `(count as u64) | ((byte_offset as u64) << 32)` where
/// `byte_offset` is a byte index into `name_postings.bin` pointing to
/// `count` consecutive `u32 LE` row IDs.
fn decode_name_postings(encoded: u64, name_postings: &[u8]) -> Vec<u32> {
    let count = usize::try_from(encoded & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    let byte_offset = usize::try_from((encoded >> 32) & 0xFFFF_FFFF).unwrap_or(usize::MAX);

    let end = byte_offset + count * 4;
    if end > name_postings.len() {
        return Vec::new();
    }
    #[expect(clippy::indexing_slicing, reason = "bounds checked above")]
    cast_slice::<u8, u32>(&name_postings[byte_offset..end]).to_vec()
}

/// Load the name prefix index from the `name_prefix` blob.
///
/// Returns an empty map when the blob is absent or empty.
///
/// Wire format:
/// ```text
/// [entry_count: u32 LE]
/// ( [prefix_len: u8] [prefix_bytes: u8 × prefix_len]
///   [bitmap_len: u32 LE] [bitmap_bytes: roaring] )*
/// ```
fn load_name_prefix(data: &[u8]) -> Result<HashMap<Vec<u8>, RoaringBitmap>> {
    if data.len() < 4 {
        return Ok(HashMap::new());
    }
    let entry_count = u32::from_le_bytes(
        data[..4]
            .try_into()
            .context("name_prefix entry_count bytes")?,
    ) as usize;
    let mut result: HashMap<Vec<u8>, RoaringBitmap> = HashMap::with_capacity(entry_count);
    let mut pos = 4usize;

    for entry in 0..entry_count {
        ensure!(
            pos < data.len(),
            "name_prefix blob truncated at entry {entry}"
        );
        let prefix_len = data[pos] as usize;
        pos += 1;
        ensure!(
            pos + prefix_len + 4 <= data.len(),
            "name_prefix blob truncated at prefix bytes for entry {entry}"
        );
        let prefix = data[pos..pos + prefix_len].to_vec();
        pos += prefix_len;
        let bitmap_len = u32::from_le_bytes(
            data[pos..pos + 4]
                .try_into()
                .context("name_prefix bitmap_len bytes")?,
        ) as usize;
        pos += 4;
        ensure!(
            pos + bitmap_len <= data.len(),
            "name_prefix blob bitmap truncated at entry {entry}"
        );
        let bitmap = RoaringBitmap::deserialize_from(&data[pos..pos + bitmap_len])
            .with_context(|| format!("deserialising name_prefix bitmap for entry {entry}"))?;
        pos += bitmap_len;
        let _ = result.insert(prefix, bitmap);
    }

    Ok(result)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ir::{
        Clauses, CompareOp, GroupBy, OrderBy, Predicate, PredicateValue, SortDirection,
    };
    use crate::storage::columnar::segment_builder::{SegmentBuilder, SymbolRow};

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Write a segment with known rows to a temp dir and return the
    /// (tempdir, segment path) pair.
    fn make_segment(rows: &[(&str, &str, u32)]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seg = tmp.path().join("seg.fqsf");
        let content_id = [0xAB_u8; 20];
        let mut b = SegmentBuilder::new("test", &content_id);
        for &(name, kind, line) in rows {
            b.add_row(SymbolRow {
                name,
                fql_kind: kind,
                language: "rust",
                line,
                byte_start: 0,
                byte_end: 10,
                usages_count: 0,
            });
        }
        b.flush(&seg).expect("flush");
        (tmp, seg)
    }

    fn clauses_where_kind(kind: &str) -> Clauses {
        Clauses {
            where_predicates: vec![Predicate {
                field: "fql_kind".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String(kind.to_owned()),
            }],
            ..Clauses::default()
        }
    }

    fn names(results: &[SymbolMatch]) -> Vec<&str> {
        results.iter().map(|r| r.name.as_str()).collect()
    }

    // ── tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn open_segment_written_by_builder() {
        let (_tmp, seg) = make_segment(&[("foo", "function", 1)]);
        let reader = SegmentReader::open(&seg).expect("open");
        assert_eq!(reader.row_count, 1);
        assert_eq!(reader.provider_id, "test");
    }

    #[test]
    fn find_functions_order_by_name() {
        let (_tmp, seg) = make_segment(&[
            ("main", "function", 10),
            ("X_CONST", "variable", 5),
            ("helper", "function", 20),
        ]);
        let reader = SegmentReader::open(&seg).expect("open");

        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "fql_kind".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String("function".to_owned()),
            }],
            order_by: Some(OrderBy {
                field: "name".to_owned(),
                direction: SortDirection::Asc,
            }),
            ..Clauses::default()
        };

        let result = reader.find_symbols(&clauses, None).expect("find");
        assert_eq!(names(&result), ["helper", "main"]);
    }

    #[test]
    fn find_by_enrichment_field() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seg = tmp.path().join("seg.fqsf");
        let content_id = [0x11_u8; 20];
        let mut b = SegmentBuilder::new("test", &content_id);
        let row = b.emit_row(SymbolRow {
            name: "foo",
            fql_kind: "function",
            language: "rust",
            line: 1,
            byte_start: 0,
            byte_end: 50,
            usages_count: 0,
        });
        b.set_field(row, "param_count", "2");
        let row2 = b.emit_row(SymbolRow {
            name: "bar",
            fql_kind: "function",
            language: "rust",
            line: 5,
            byte_start: 51,
            byte_end: 100,
            usages_count: 0,
        });
        b.set_field(row2, "param_count", "0");
        b.flush(&seg).expect("flush");

        let reader = SegmentReader::open(&seg).expect("open");

        // WHERE param_count = '2' should return only "foo"
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "param_count".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String("2".to_owned()),
            }],
            ..Clauses::default()
        };
        let result = reader.find_symbols(&clauses, None).expect("find");
        assert_eq!(names(&result), ["foo"]);

        // The enrichment field should appear in the fields map.
        assert_eq!(result[0].fields.get("param_count"), Some(&"2".to_owned()));
    }

    #[test]
    fn group_by_kind_having_count() {
        let (_tmp, seg) = make_segment(&[
            ("f1", "function", 1),
            ("f2", "function", 2),
            ("S1", "struct", 3),
        ]);
        let reader = SegmentReader::open(&seg).expect("open");

        let clauses = Clauses {
            group_by: Some(GroupBy::Field("fql_kind".to_owned())),
            having_predicates: vec![Predicate {
                field: "count".to_owned(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(2),
            }],
            ..Clauses::default()
        };
        let result = reader.find_symbols(&clauses, None).expect("find");
        // Only "function" has count ≥ 2.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].fql_kind.as_deref(), Some("function"));
        assert_eq!(result[0].count, Some(2));
    }

    #[test]
    fn order_by_line_desc() {
        let (_tmp, seg) = make_segment(&[
            ("a", "function", 30),
            ("b", "function", 10),
            ("c", "function", 20),
        ]);
        let reader = SegmentReader::open(&seg).expect("open");

        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "line".to_owned(),
                direction: SortDirection::Desc,
            }),
            ..Clauses::default()
        };
        let result = reader.find_symbols(&clauses, None).expect("find");
        let lines: Vec<_> = result.iter().map(|r| r.line).collect();
        assert_eq!(lines, [Some(30), Some(20), Some(10)]);
    }

    #[test]
    fn limit_and_offset() {
        let (_tmp, seg) = make_segment(&[
            ("r0", "function", 1),
            ("r1", "function", 2),
            ("r2", "function", 3),
            ("r3", "function", 4),
            ("r4", "function", 5),
        ]);
        let reader = SegmentReader::open(&seg).expect("open");

        // ORDER BY line ASC LIMIT 2 OFFSET 1 → rows at lines 2, 3
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "line".to_owned(),
                direction: SortDirection::Asc,
            }),
            limit: Some(2),
            offset: Some(1),
            ..Clauses::default()
        };
        let result = reader.find_symbols(&clauses, None).expect("find");
        assert_eq!(result.len(), 2);
        let lines: Vec<_> = result.iter().map(|r| r.line).collect();
        assert_eq!(lines, [Some(2), Some(3)]);
    }

    #[test]
    fn lookup_name_via_fst() {
        let (_tmp, seg) = make_segment(&[
            ("foo", "function", 1),
            ("bar", "struct", 5),
            ("foo", "function", 10), // second row with same name
        ]);
        let reader = SegmentReader::open(&seg).expect("open");

        let rows = reader.lookup_name("foo");
        assert_eq!(rows.len(), 2, "two 'foo' rows");
        let mut rows_sorted = rows;
        rows_sorted.sort_unstable();
        assert_eq!(rows_sorted, [0, 2], "rows 0 and 2");

        assert!(reader.lookup_name("nonexistent").is_empty());
    }

    #[test]
    fn roaring_prefilter_returns_empty_for_unknown_kind() {
        let (_tmp, seg) = make_segment(&[("foo", "function", 1)]);
        let reader = SegmentReader::open(&seg).expect("open");

        let clauses = clauses_where_kind("nonexistent_kind");
        let result = reader.find_symbols(&clauses, None).expect("find");
        assert!(result.is_empty());
    }

    #[test]
    fn source_path_propagated_to_symbol_match() {
        let (_tmp, seg) = make_segment(&[("main", "function", 1)]);
        let reader = SegmentReader::open(&seg).expect("open");

        let path = std::path::Path::new("src/main.rs");
        let result = reader
            .find_symbols(&Clauses::default(), Some(path))
            .expect("find");
        assert_eq!(result[0].path.as_deref(), Some(path));
    }

    /// Round-trip: manually build a segment with known content and verify
    /// that `find_symbols` with no clauses returns the same rows.
    #[test]
    fn round_trip_row_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seg = tmp.path().join("seg.fqsf");
        let mut b = SegmentBuilder::new("test", &[0xFFu8; 20]);
        let r0 = b.emit_row(SymbolRow {
            name: "alpha",
            fql_kind: "function",
            language: "rust",
            line: 1,
            byte_start: 0,
            byte_end: 50,
            usages_count: 3,
        });
        b.set_field(r0, "is_const", "false");
        let r1 = b.emit_row(SymbolRow {
            name: "beta",
            fql_kind: "struct",
            language: "rust",
            line: 10,
            byte_start: 51,
            byte_end: 200,
            usages_count: 0,
        });
        b.set_field(r1, "member_count", "4");
        b.flush(&seg).expect("flush");

        let reader = SegmentReader::open(&seg).expect("open");

        // Find all, sorted by name.
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "name".to_owned(),
                direction: SortDirection::Asc,
            }),
            ..Clauses::default()
        };
        let results = reader.find_symbols(&clauses, None).expect("find");
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].name, "alpha");
        assert_eq!(results[0].fql_kind.as_deref(), Some("function"));
        assert_eq!(results[0].line, Some(1));
        assert_eq!(results[0].usages_count, Some(3));
        assert_eq!(
            results[0].fields.get("is_const").map(String::as_str),
            Some("false")
        );

        assert_eq!(results[1].name, "beta");
        assert_eq!(results[1].fql_kind.as_deref(), Some("struct"));
        assert_eq!(results[1].line, Some(10));
        assert_eq!(
            results[1].fields.get("member_count").map(String::as_str),
            Some("4")
        );

        // ── Gap 4: byte_start_of / byte_end_of accessors ──────────────────
        // r0 = row 0 ("alpha"), r1 = row 1 ("beta") — insertion order.
        assert_eq!(reader.byte_start_of(0), 0, "alpha byte_start");
        assert_eq!(reader.byte_end_of(0), 50, "alpha byte_end");
        assert_eq!(reader.byte_start_of(1), 51, "beta byte_start");
        assert_eq!(reader.byte_end_of(1), 200, "beta byte_end");
    }

    // ── Gap 5: empty segment ─────────────────────────────────────────────

    /// A segment with zero rows must open successfully and return an empty
    /// `find_symbols` result without hitting the row-materialisation code.
    #[test]
    fn find_symbols_on_empty_segment_returns_empty_vec() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seg = tmp.path().join("seg.fqsf");
        let b = SegmentBuilder::new("test", &[0xAAu8; 20]);
        b.flush(&seg).expect("flush");

        let reader = SegmentReader::open(&seg).expect("open");
        assert_eq!(reader.row_count, 0);

        let result = reader
            .find_symbols(&Clauses::default(), None)
            .expect("find on empty segment");
        assert!(result.is_empty(), "expected empty vec for zero-row segment");
    }

    // ── Gap 3: error-path tests ──────────────────────────────────────────

    /// Opening a path that does not exist must return `Err`.
    #[test]
    fn open_nonexistent_path_returns_err() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does_not_exist.fqsf");
        assert!(
            SegmentReader::open(&missing).is_err(),
            "expected Err for missing file"
        );
    }

    /// A segment with a corrupted FQSF outer magic must return `Err` at `open`.
    #[test]
    fn open_corrupt_magic_returns_err() {
        let (_tmp, seg) = make_segment(&[("foo", "function", 1)]);

        // Overwrite the first 4 bytes of the .fqsf file with garbage.
        let mut bytes = std::fs::read(&seg).expect("read segment");
        bytes[0] = b'X';
        bytes[1] = b'X';
        bytes[2] = b'X';
        bytes[3] = b'X';
        std::fs::write(&seg, &bytes).expect("write segment");

        assert!(
            SegmentReader::open(&seg).is_err(),
            "expected Err for corrupt FQSF magic"
        );
    }

    /// A segment with non-monotone string pool offsets must return `Err` at `open`.
    #[test]
    fn open_nonmonotone_string_pool_returns_err() {
        // Build a segment with at least two strings so the monotonicity check fires.
        let (_tmp, seg) = make_segment(&[("alpha", "function", 1), ("beta", "struct", 2)]);

        let mut bytes = std::fs::read(&seg).expect("read segment");

        // Find the "strings_offsets" blob in the TOC and corrupt its first two offsets.
        // TOC starts at byte 12; each entry is TOC_ENTRY_SIZE (64) bytes.
        // Entry layout: [name: ENTRY_NAME_LEN bytes][offset: u32 LE][len: u32 LE]
        let entry_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let toc_start = 12;
        for i in 0..entry_count {
            let es = toc_start + i * TOC_ENTRY_SIZE;
            let name_end = bytes[es..es + ENTRY_NAME_LEN]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(ENTRY_NAME_LEN);
            if &bytes[es..es + name_end] == b"strings_offsets" {
                let offset = u32::from_le_bytes(
                    bytes[es + ENTRY_NAME_LEN..es + ENTRY_NAME_LEN + 4]
                        .try_into()
                        .unwrap(),
                ) as usize;
                let len = u32::from_le_bytes(
                    bytes[es + ENTRY_NAME_LEN + 4..es + ENTRY_NAME_LEN + 8]
                        .try_into()
                        .unwrap(),
                ) as usize;
                // Corrupt: make offset[1] < offset[0] to break monotonicity.
                if len >= 8 {
                    let off0 = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                    let bad: u32 = if off0 > 0 { 0 } else { u32::MAX };
                    bytes[offset + 4..offset + 8].copy_from_slice(&bad.to_le_bytes());
                    std::fs::write(&seg, &bytes).expect("write segment");
                    assert!(
                        SegmentReader::open(&seg).is_err(),
                        "expected Err for non-monotone string pool offsets"
                    );
                }
                return;
            }
        }
        // blob not found — test passes vacuously (shouldn't happen with real segments)
    }
}
