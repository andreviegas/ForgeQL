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

mod load;
use load::{
    blob_slice, decode_name_postings, load_enrichment_postings, load_kind_postings,
    load_name_prefix, load_zone_maps, parse_column_entries, parse_toc,
};

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
/// Fields decoded from the inner FQSG `header` blob of a segment file.
struct HeaderFields {
    provider_id: String,
    content_id: Vec<u8>,
    row_count: u32,
    string_count: u32,
    extra_col_names: Vec<String>,
}
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
    /// Usage postings FST (BUG-006): identifier text → 1-based source lines.
    /// `None` when the file produced no usage sites (blob omitted at flush).
    pub(crate) usages_fst: Option<FstMap<MmapSlice>>,
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
        // ── 1-2. Mmap + validate the outer FQSF header ────────────────────
        let (mmap, file_len) = Self::map_and_validate(path)?;

        // ── 3. Parse TOC ──────────────────────────────────────────────────
        let blobs = parse_toc(&mmap, file_len, path)?;

        // ── 4-5. Inner FQSG header blob + extra enrichment columns ────────
        let hdr = Self::parse_header_blob(&mmap, &blobs, path)?;

        // ── 6. String pool ────────────────────────────────────────────────
        let off_range = blobs.get("strings_offsets").copied().unwrap_or((0, 0));
        let dat_range = blobs.get("strings_data").copied().unwrap_or((0, 0));
        let strings =
            StringPool::from_blobs(Arc::clone(&mmap), off_range, dat_range, hdr.string_count)?;

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

        // ── 8b. Usage postings FST (BUG-006; blob absent = no usages) ─────
        let usages_fst = match blobs.get("usages_fst").copied() {
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

        Ok(Self {
            mmap,
            blobs,
            path: path.to_owned(),
            row_count: hdr.row_count,
            provider_id: hdr.provider_id,
            content_id: hdr.content_id,
            extra_col_names: hdr.extra_col_names,
            strings,
            kind_postings,
            field_postings,
            zone_maps,
            name_prefix,
            name_fst,
            usages_fst,
        })
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
    fn parse_header_blob(
        mmap: &Arc<Mmap>,
        blobs: &HashMap<String, (usize, usize)>,
        path: &Path,
    ) -> Result<HeaderFields> {
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
        decode_name_postings(encoded, self.blob_bytes("usages_postings"))
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

    /// Whether this segment stores an enrichment column named `name`.
    #[must_use]
    pub fn has_extra_col(&self, name: &str) -> bool {
        self.extra_col_names.iter().any(|c| c == name)
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

    /// Read the parent ordinal for `row` (`u32::MAX` = top-level node).
    pub fn parent_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.blob_bytes("col_parent_ordinal");
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read the rev handle for `row` (first 8 bytes of SHA-256 of node bytes, LE u64).
    /// Returns `0` for analysis-only rows or when the column is absent.
    pub fn rev_of(&self, row: u32) -> u64 {
        let blob = self.blob_bytes("col_rev");
        let start = row as usize * 8;
        let end = start + 8;
        if blob.len() < end {
            return 0;
        }
        u64::from_le_bytes(blob[start..end].try_into().unwrap_or([0u8; 8]))
    }

    /// Read the first-child ordinal for `row` (`u32::MAX` = no children).
    pub fn first_child_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.blob_bytes("col_first_child_ordinal");
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read the next-sibling ordinal for `row` (`u32::MAX` = no next sibling).
    pub fn next_sibling_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.blob_bytes("col_next_sibling_ordinal");
        if blob.is_empty() {
            return u32::MAX;
        }
        let slice: &[u32] = cast_slice(blob);
        slice.get(row as usize).copied().unwrap_or(u32::MAX)
    }

    /// Read the prev-sibling ordinal for `row` (`u32::MAX` = no prev sibling).
    pub fn prev_sibling_ordinal_of(&self, row: u32) -> u32 {
        let blob = self.blob_bytes("col_prev_sibling_ordinal");
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
                    node_id: source_path.and_then(|p| {
                        self.ordinal_of(row)
                            .map(|ord| crate::node_id::make_node_id(&p.to_string_lossy(), ord))
                    }),
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
            node_id: self
                .ordinal_of(row)
                .map(|ord| crate::node_id::make_node_id(&source_path.to_string_lossy(), ord)),
        })
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests;
