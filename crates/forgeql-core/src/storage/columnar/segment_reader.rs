//! [`SegmentReader`] — mmap-based reader for Phase 03 columnar segments.
//!
//! Opens a segment directory written by [`SegmentBuilder`], validates the
//! `header.bin` magic and schema version, mmaps all column files, and
//! deserialises the Roaring bitmap posting lists and the FST.
//!
//! # Query pipeline
//!
//! 1. **Roaring prefilter** — for `WHERE fql_kind = 'X'` predicates the
//!    per-segment `postings_fql_kind.bin` bitmap narrows the candidate row
//!    set in O(n/64) time without touching any column data.
//! 2. **Materialise** — surviving row IDs are materialised into
//!    [`SymbolMatch`] values by reading the mmap'd column arrays.
//! 3. **`apply_clauses`** — the full shared pipeline (residual WHERE,
//!    GROUP BY, ORDER BY, LIMIT, OFFSET, HAVING) runs over the materialised
//!    results.  This guarantees clause-pipeline parity with the legacy
//!    backend.
//!
//! # Phase 04 scope
//!
//! This reader operates on a single segment directory in isolation — no
//! overlay, no cross-segment merging, and no session integration.
//! Production queries via `FIND … USING 'columnar'` are wired in Phase 05.
//!
//! [`SegmentBuilder`]: super::segment_builder::SegmentBuilder
//! [`SymbolMatch`]: crate::result::SymbolMatch

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

use anyhow::{Context, Result, ensure};
use bytemuck::cast_slice;
use fst::Map as FstMap;
use memmap2::{Mmap, MmapOptions};
use roaring::RoaringBitmap;

use crate::filter::apply_clauses;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;

use super::segment_builder::MAGIC;

// ─────────────────────────────────────────────────────────────────────────────
// Format constants (must match segment_builder.rs)
// ─────────────────────────────────────────────────────────────────────────────

const SEGMENT_SCHEMA_VERSION: u32 = 1;
const TYPE_TAG_STR_OPT: u8 = 5;
/// Byte length of the fixed-size `header.bin` preamble.
const HEADER_PREAMBLE_LEN: usize = 80;

// ─────────────────────────────────────────────────────────────────────────────
// StringPool — mmap-backed per-segment string intern table
// ─────────────────────────────────────────────────────────────────────────────

/// Mmap-backed string intern table matching what `SegmentBuilder` writes to
/// `strings_offsets.bin` and `strings_data.bin`.
struct StringPool {
    offsets: Option<Mmap>, // [u32; string_count + 1]
    data: Option<Mmap>,    // UTF-8 bytes
    string_count: u32,
    /// Pre-built reverse map for O(1) kind-prefilter lookups.
    reverse: HashMap<String, u32>,
}

impl StringPool {
    fn build(dir: &Path, string_count: u32) -> Result<Self> {
        let offsets =
            mmap_file(&dir.join("strings_offsets.bin")).context("opening strings_offsets.bin")?;
        let data = mmap_file(&dir.join("strings_data.bin")).context("opening strings_data.bin")?;

        // Validate string pool at open time so corrupt data is detected early
        // rather than causing a panic mid-query inside `get()`.
        //
        // Required invariants:
        //  1. `strings_offsets.bin` has exactly `(string_count + 1) * 4` bytes.
        //  2. Offsets are monotonically non-decreasing.
        //  3. The last offset (`offsets[string_count]`) ≤ `strings_data.bin` length.
        if string_count > 0 {
            let expected_offset_bytes = (string_count as usize + 1) * 4;
            let actual_offset_bytes = offsets.as_ref().map_or(0, |m| m.len());
            ensure!(
                actual_offset_bytes >= expected_offset_bytes,
                "strings_offsets.bin in {} has {} bytes; expected ≥ {} for {} strings",
                dir.display(),
                actual_offset_bytes,
                expected_offset_bytes,
                string_count
            );

            #[allow(clippy::indexing_slicing)] // length validated by ensure! above
            if let (Some(off_mmap), Some(dat_mmap)) = (&offsets, &data) {
                let off_slice: &[u32] = cast_slice(off_mmap.as_ref());
                // Monotonicity check.
                for i in 0..string_count as usize {
                    let lo = off_slice[i] as usize;
                    let hi = off_slice[i + 1] as usize;
                    ensure!(
                        lo <= hi,
                        "strings_offsets.bin in {} is not monotone at index {i}: {lo} > {hi}",
                        dir.display()
                    );
                }
                // Last offset must not exceed data length.
                let last = off_slice[string_count as usize] as usize;
                ensure!(
                    last <= dat_mmap.len(),
                    "strings_offsets.bin in {}: last offset {last} > strings_data.bin length {}",
                    dir.display(),
                    dat_mmap.len()
                );
            }
        }

        let mut pool = Self {
            offsets,
            data,
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
    ///
    /// The returned `&str` borrows from the mmap, so its lifetime is `'_`
    /// (tied to `&self`).
    fn get(&self, id: u32) -> &str {
        if id == u32::MAX || id >= self.string_count {
            return "";
        }
        let (Some(off_mmap), Some(dat_mmap)) = (&self.offsets, &self.data) else {
            return "";
        };
        // SAFETY: cast_slice requires the slice to be u32-aligned.
        // Mmap is always page-aligned (≥ 4 bytes), so this is safe.
        // The file is written as a `[u32]` array so the length is always a
        // multiple of 4; cast_slice panics otherwise — treated as corrupt data.
        let offsets: &[u32] = cast_slice(off_mmap);
        let id_usize = id as usize;
        let (Some(&start_u32), Some(&end_u32)) = (offsets.get(id_usize), offsets.get(id_usize + 1))
        else {
            return "";
        };
        let (start, end) = (start_u32 as usize, end_u32 as usize);
        if end > dat_mmap.len() || start > end {
            return "";
        }
        std::str::from_utf8(&dat_mmap[start..end]).unwrap_or("")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SegmentReader
// ─────────────────────────────────────────────────────────────────────────────

/// Mmap-based read-only view of a single columnar segment directory.
///
/// Open with [`SegmentReader::open`].  The reader holds all mmaps for its
/// lifetime — drop it to release OS resources.
pub struct SegmentReader {
    /// Absolute path of the opened segment directory (for diagnostics).
    pub dir: PathBuf,
    /// Number of rows stored in this segment.
    pub row_count: u32,
    /// Provider ID decoded from the header, e.g. `"git-sha1"`.
    pub provider_id: String,
    /// Raw content ID bytes (length matches the provider's hash width).
    pub content_id: Vec<u8>,
    // Core u32 columns — all present when `row_count > 0`.
    col_name_id: Option<Mmap>,
    col_fql_kind_id: Option<Mmap>,
    col_line: Option<Mmap>,
    col_byte_start: Option<Mmap>,
    col_byte_end: Option<Mmap>,
    col_usages_count: Option<Mmap>,
    col_language_id: Option<Mmap>,
    /// Extra enrichment columns stored as nullable u32 ID arrays
    /// (u32::MAX = absent).  Key is the enrichment field name.
    extra_cols: HashMap<String, Mmap>,
    strings: StringPool,
    /// Per-fql_kind Roaring bitmaps loaded from `postings_fql_kind.bin`.
    kind_postings: HashMap<u32, RoaringBitmap>,
    /// FST map: symbol name bytes → packed `(count | byte_offset << 32)`.
    name_fst: FstMap<Vec<u8>>,
    /// Flat `[u32 LE]` array of row IDs indexed by `name_fst`.
    name_postings: Option<Mmap>,
}

impl SegmentReader {
    /// Open and validate a segment directory.
    ///
    /// Reads `header.bin`, validates the `FQSG` magic and schema version,
    /// mmaps all column files, deserialises Roaring bitmap postings, and
    /// loads the FST into memory.
    ///
    /// # Errors
    /// Returns `Err` on I/O failure, missing `header.bin`, format mismatch,
    /// or schema version mismatch.
    pub fn open(dir: &Path) -> Result<Self> {
        let header_bytes = std::fs::read(dir.join("header.bin"))
            .with_context(|| format!("reading header.bin in {}", dir.display()))?;

        ensure!(
            header_bytes.len() >= HEADER_PREAMBLE_LEN,
            "header.bin in {} is only {} bytes (need ≥ {})",
            dir.display(),
            header_bytes.len(),
            HEADER_PREAMBLE_LEN,
        );
        ensure!(
            header_bytes[..4] == MAGIC,
            "invalid magic in {}; expected FQSG",
            dir.display()
        );

        // Segments are encoded little-endian.  Refuse to open on a big-endian
        // host rather than silently producing garbage.
        if cfg!(target_endian = "big") {
            anyhow::bail!(
                "segment format is little-endian only; cannot open {} on a big-endian host",
                dir.display()
            );
        }

        #[allow(clippy::indexing_slicing)] // bounds checked by ensure! above
        let schema_version = u32::from_le_bytes(
            header_bytes[4..8]
                .try_into()
                .context("schema_version bytes")?,
        );
        ensure!(
            schema_version == SEGMENT_SCHEMA_VERSION,
            "schema version mismatch in {}: expected {}, got {}",
            dir.display(),
            SEGMENT_SCHEMA_VERSION,
            schema_version
        );

        #[allow(clippy::indexing_slicing)]
        let provider_id = {
            let pid_bytes = &header_bytes[8..24];
            let end = pid_bytes.iter().position(|&b| b == 0).unwrap_or(16);
            String::from_utf8_lossy(&pid_bytes[..end]).into_owned()
        };

        #[allow(clippy::indexing_slicing)]
        let content_id_len = header_bytes[24] as usize;
        ensure!(content_id_len <= 32, "content_id_len {content_id_len} > 32");

        #[allow(clippy::indexing_slicing)]
        let content_id = header_bytes[28..28 + content_id_len].to_vec();

        #[allow(clippy::indexing_slicing)]
        let row_count =
            u32::from_le_bytes(header_bytes[60..64].try_into().context("row_count bytes")?);
        #[allow(clippy::indexing_slicing)]
        let string_count = u32::from_le_bytes(
            header_bytes[64..68]
                .try_into()
                .context("string_count bytes")?,
        );
        #[allow(clippy::indexing_slicing)]
        let column_count = u32::from_le_bytes(
            header_bytes[68..72]
                .try_into()
                .context("column_count bytes")?,
        );

        // Parse variable-length column entries from the header.
        let columns = parse_column_entries(&header_bytes, HEADER_PREAMBLE_LEN, column_count)?;

        // Mmap the seven core u32 columns.
        let col_name_id = mmap_col(dir, "name_id")?;
        let col_fql_kind_id = mmap_col(dir, "fql_kind_id")?;
        let col_line = mmap_col(dir, "line")?;
        let col_byte_start = mmap_col(dir, "byte_start")?;
        let col_byte_end = mmap_col(dir, "byte_end")?;
        let col_usages_count = mmap_col(dir, "usages_count")?;
        let col_language_id = mmap_col(dir, "language_id")?;

        // Mmap optional enrichment columns (TYPE_TAG_STR_OPT).
        let core = [
            "name_id",
            "fql_kind_id",
            "line",
            "byte_start",
            "byte_end",
            "usages_count",
            "language_id",
        ];
        let mut extra_cols = HashMap::new();
        for (col_name, type_tag) in &columns {
            if !core.contains(&col_name.as_str()) && *type_tag == TYPE_TAG_STR_OPT {
                if let Some(mmap) = mmap_col(dir, col_name)? {
                    let _ = extra_cols.insert(col_name.clone(), mmap);
                }
            }
        }

        // String pool.
        let strings = StringPool::build(dir, string_count)?;

        // Roaring postings.
        let kind_postings = load_kind_postings(dir)?;

        // FST + name_postings (load FST bytes into a Vec<u8> for simplicity).
        let fst_bytes = std::fs::read(dir.join("name.fst")).context("reading name.fst")?;
        let name_fst = FstMap::new(fst_bytes).context("parsing name.fst")?;
        let name_postings =
            mmap_file(&dir.join("name_postings.bin")).context("opening name_postings.bin")?;

        Ok(Self {
            dir: dir.to_owned(),
            row_count,
            provider_id,
            content_id,
            col_name_id,
            col_fql_kind_id,
            col_line,
            col_byte_start,
            col_byte_end,
            col_usages_count,
            col_language_id,
            extra_cols,
            strings,
            kind_postings,
            name_fst,
            name_postings,
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
        let postings = self.name_postings.as_deref().unwrap_or(&[]);
        decode_name_postings(encoded, postings)
    }

    /// Read the symbol name for row `row`.
    pub fn name_of(&self, row: u32) -> &str {
        self.str_of_core(&self.col_name_id, row)
    }

    /// Read the FQL kind string for row `row`.
    pub fn fql_kind_of(&self, row: u32) -> &str {
        self.str_of_core(&self.col_fql_kind_id, row)
    }

    /// Read the language string for row `row`.
    pub fn language_of(&self, row: u32) -> &str {
        self.str_of_core(&self.col_language_id, row)
    }

    /// Read the 1-based source line for row `row`.
    pub fn line_of(&self, row: u32) -> u32 {
        self.u32_of(&self.col_line, row)
    }

    /// Read the byte-range start for row `row`.
    pub fn byte_start_of(&self, row: u32) -> u32 {
        self.u32_of(&self.col_byte_start, row)
    }

    /// Read the byte-range end for row `row`.
    pub fn byte_end_of(&self, row: u32) -> u32 {
        self.u32_of(&self.col_byte_end, row)
    }

    /// Read the usages count for row `row`.
    pub fn usages_count_of(&self, row: u32) -> u32 {
        self.u32_of(&self.col_usages_count, row)
    }

    /// Read an enrichment field value for row `row`.
    ///
    /// Returns `None` when the column is absent or the row's slot is `NULL`
    /// (encoded as `u32::MAX` in the segment).
    pub fn extra_field_str(&self, col: &str, row: u32) -> Option<&str> {
        let mmap = self.extra_cols.get(col)?;
        let slice: &[u32] = cast_slice(mmap.as_ref());
        let id = slice.get(row as usize).copied()?;
        if id == u32::MAX {
            None
        } else {
            let s = self.strings.get(id);
            if s.is_empty() { None } else { Some(s) }
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Read a u32 from `col` at `row`; returns 0 if absent/out-of-range.
    fn u32_of(&self, col: &Option<Mmap>, row: u32) -> u32 {
        let Some(mmap) = col else { return 0 };
        let slice: &[u32] = cast_slice(mmap.as_ref());
        slice.get(row as usize).copied().unwrap_or(0)
    }

    /// Read a string-intern ID from `col` at `row`, then resolve via the
    /// segment's string pool.  Returns `""` when absent.
    fn str_of_core(&self, col: &Option<Mmap>, row: u32) -> &str {
        let id = self.u32_of(col, row);
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

    /// Materialise `rows` into `Vec<SymbolMatch>`.
    fn materialize_rows(
        &self,
        rows: &RoaringBitmap,
        source_path: Option<&Path>,
    ) -> Vec<SymbolMatch> {
        rows.iter()
            .map(|row| {
                let name = self.str_of_core(&self.col_name_id, row).to_owned();
                let fql_kind = self.str_of_core(&self.col_fql_kind_id, row).to_owned();
                let language = self.str_of_core(&self.col_language_id, row).to_owned();
                let line = self.u32_of(&self.col_line, row);
                let usages = self.u32_of(&self.col_usages_count, row);

                let mut fields: HashMap<String, String> = HashMap::new();
                for (col_name, mmap) in &self.extra_cols {
                    let slice: &[u32] = cast_slice(mmap.as_ref());
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Private file-system helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Open a file for read-only mmap.
///
/// Returns `Ok(None)` for missing or empty files (absent enrichment columns
/// and zero-byte postings are both valid).
///
/// # Safety
/// The caller must ensure the file is not modified while the returned `Mmap`
/// is live.  All segment files are immutable once written (content-addressed,
/// never updated in-place).
#[allow(unsafe_code)]
fn mmap_file(path: &Path) -> Result<Option<Mmap>> {
    let file = match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("opening {}", path.display()));
        }
        Ok(f) => f,
    };
    let len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if len == 0 {
        return Ok(None);
    }
    // SAFETY: the segment directory is immutable once written.  No other
    // thread or process mutates these files while a reader is open.
    let mmap = unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("mmap {}", path.display()))?;
    Ok(Some(mmap))
}

/// Convenience wrapper: mmap `col_<name>.bin`.
fn mmap_col(dir: &Path, col_name: &str) -> Result<Option<Mmap>> {
    mmap_file(&dir.join(format!("col_{col_name}.bin")))
        .with_context(|| format!("mmapping col_{col_name}.bin"))
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

/// Deserialise `postings_fql_kind.bin` into `HashMap<kind_id, RoaringBitmap>`.
///
/// Format: `[kind_count: u32] (kind_id: u32, bitmap_len: u32, bitmap_bytes)*`
fn load_kind_postings(dir: &Path) -> Result<HashMap<u32, RoaringBitmap>> {
    let path = dir.join("postings_fql_kind.bin");
    let data = match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(d) => d,
    };
    if data.len() < 4 {
        return Ok(HashMap::new());
    }

    #[allow(clippy::indexing_slicing)] // length checked above
    let kind_count = u32::from_le_bytes(data[..4].try_into().context("kind_count bytes")?) as usize;
    let mut map = HashMap::with_capacity(kind_count);
    let mut pos = 4usize;

    for entry in 0..kind_count {
        ensure!(
            pos + 8 <= data.len(),
            "postings_fql_kind.bin truncated at entry {entry}"
        );
        #[allow(clippy::indexing_slicing)] // guarded by ensure! above
        let kind_id = u32::from_le_bytes(data[pos..pos + 4].try_into().context("kind_id bytes")?);
        #[allow(clippy::indexing_slicing)]
        let bitmap_len = u32::from_le_bytes(
            data[pos + 4..pos + 8]
                .try_into()
                .context("bitmap_len bytes")?,
        ) as usize;
        pos += 8;
        ensure!(
            pos + bitmap_len <= data.len(),
            "bitmap bytes truncated at entry {entry}"
        );
        #[allow(clippy::indexing_slicing)]
        let bitmap = RoaringBitmap::deserialize_from(&data[pos..pos + bitmap_len])
            .with_context(|| format!("deserialising bitmap for kind_id {kind_id}"))?;
        pos += bitmap_len;
        let _ = map.insert(kind_id, bitmap);
    }

    Ok(map)
}

/// Decode FST-encoded name posting.
///
/// FST value layout: `(count as u64) | ((byte_offset as u64) << 32)` where
/// `byte_offset` is a byte index into `name_postings.bin` pointing to
/// `count` consecutive `u32 LE` row IDs.
fn decode_name_postings(encoded: u64, name_postings: &[u8]) -> Vec<u32> {
    #[allow(clippy::cast_possible_truncation)]
    let count = (encoded & 0xFFFF_FFFF) as usize;
    #[allow(clippy::cast_possible_truncation)]
    let byte_offset = ((encoded >> 32) & 0xFFFF_FFFF) as usize;

    let end = byte_offset + count * 4;
    if end > name_postings.len() {
        return Vec::new();
    }
    #[allow(clippy::indexing_slicing)] // bounds checked above
    cast_slice::<u8, u32>(&name_postings[byte_offset..end]).to_vec()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::wildcard_imports
)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ir::{
        Clauses, CompareOp, GroupBy, OrderBy, Predicate, PredicateValue, SortDirection,
    };
    use crate::storage::columnar::segment_builder::SegmentBuilder;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Write a segment with known rows to a temp dir and return the
    /// (tempdir, segment path) pair.
    fn make_segment(rows: &[(&str, &str, u32)]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seg_dir = tmp.path().join("seg");
        let content_id = [0xAB_u8; 20];
        let mut b = SegmentBuilder::new("test", &content_id);
        for &(name, kind, line) in rows {
            b.add_row(name, kind, "rust", line, 0, 10, 0);
        }
        b.flush(&seg_dir).expect("flush");
        (tmp, seg_dir)
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
        let seg_dir = tmp.path().join("seg");
        let content_id = [0x11_u8; 20];
        let mut b = SegmentBuilder::new("test", &content_id);
        let row = b.emit_row("foo", "function", "rust", 1, 0, 50, 0);
        b.set_field(row, "param_count", "2");
        let row2 = b.emit_row("bar", "function", "rust", 5, 51, 100, 0);
        b.set_field(row2, "param_count", "0");
        b.flush(&seg_dir).expect("flush");

        let reader = SegmentReader::open(&seg_dir).expect("open");

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
        let seg_dir = tmp.path().join("seg");
        let mut b = SegmentBuilder::new("test", &[0xFFu8; 20]);
        let r0 = b.emit_row("alpha", "function", "rust", 1, 0, 50, 3);
        b.set_field(r0, "is_const", "false");
        let r1 = b.emit_row("beta", "struct", "rust", 10, 51, 200, 0);
        b.set_field(r1, "member_count", "4");
        b.flush(&seg_dir).expect("flush");

        let reader = SegmentReader::open(&seg_dir).expect("open");

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
        let seg_dir = tmp.path().join("seg");
        let b = SegmentBuilder::new("test", &[0xAAu8; 20]);
        b.flush(&seg_dir).expect("flush");

        let reader = SegmentReader::open(&seg_dir).expect("open");
        assert_eq!(reader.row_count, 0);

        let result = reader
            .find_symbols(&Clauses::default(), None)
            .expect("find on empty segment");
        assert!(result.is_empty(), "expected empty vec for zero-row segment");
    }

    // ── Gap 3: error-path tests ──────────────────────────────────────────

    /// Opening a path that does not exist must return `Err`.
    #[test]
    fn open_nonexistent_dir_returns_err() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does_not_exist");
        assert!(
            SegmentReader::open(&missing).is_err(),
            "expected Err for missing directory"
        );
    }

    /// A segment with a corrupted FQSG magic must return `Err` at `open`,
    /// not produce garbage results.
    #[test]
    fn open_corrupt_magic_returns_err() {
        let (_tmp, seg_dir) = make_segment(&[("foo", "function", 1)]);

        // Overwrite the first 4 bytes of header.bin with garbage.
        let header_path = seg_dir.join("header.bin");
        let mut bytes = std::fs::read(&header_path).expect("read header");
        bytes[0] = b'X';
        bytes[1] = b'X';
        bytes[2] = b'X';
        bytes[3] = b'X';
        std::fs::write(&header_path, &bytes).expect("write header");

        assert!(
            SegmentReader::open(&seg_dir).is_err(),
            "expected Err for corrupt FQSG magic"
        );
    }

    /// A segment with non-monotone string pool offsets must return `Err` at
    /// `open` (not panic mid-query).
    #[test]
    fn open_nonmonotone_string_pool_returns_err() {
        // Build a segment that has at least two strings in the pool so the
        // monotonicity check fires.
        let (_tmp, seg_dir) = make_segment(&[("alpha", "function", 1), ("beta", "struct", 2)]);

        let offsets_path = seg_dir.join("strings_offsets.bin");
        let mut bytes = std::fs::read(&offsets_path).expect("read offsets");

        // strings_offsets.bin is a [u32] array (little-endian).  Make
        // offset[1] less than offset[0] to break monotonicity.
        // offset[0] is bytes 0..4; offset[1] is bytes 4..8.
        if bytes.len() >= 8 {
            // Write 0xFFFF_FFFF into offset[1] if offset[0] < 0xFFFF_FFFF,
            // otherwise write 0 into offset[1].
            let off0 = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
            let bad: u32 = if off0 > 0 { 0 } else { u32::MAX };
            bytes[4..8].copy_from_slice(&bad.to_le_bytes());
            std::fs::write(&offsets_path, &bytes).expect("write offsets");

            assert!(
                SegmentReader::open(&seg_dir).is_err(),
                "expected Err for non-monotone string pool offsets"
            );
        }
        // If the offsets file is too short to corrupt, the test passes vacuously
        // (the segment has fewer than two offset entries, so monotonicity is
        // trivially satisfied — not a realistic production case).
    }
}
