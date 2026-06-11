//! [`SegmentBuilder`] — assembles and flushes one columnar segment directory.
//!
//! A segment corresponds to one source file.  After all rows are added with
//! [`SegmentBuilder::add_row`], call [`SegmentBuilder::flush`] to write the
//! segment atomically to disk.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use bytemuck::cast_slice;
use fst::MapBuilder;
use roaring::RoaringBitmap;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic bytes at the start of every `header.bin` (inner FQSG blob inside .fqsf).
pub const MAGIC: [u8; 4] = *b"FQSG";

/// Low-cardinality enrichment fields for which `SegmentBuilder` writes
/// per-segment Roaring-bitmap posting files (`postings_<field>.bin`).
///
/// Criterion: boolean flags and low-cardinality string enums with
/// ≤ `MAX_CARDINALITY` (= 8) distinct values per segment.  The builder
/// silently skips any field whose actual cardinality exceeds that cap at
/// flush time, so adding fields here is additive and safe.
///
/// `SegmentReader` discovers the blobs by checking this list so readers
/// always know which fields to attempt to load.
pub const POSTING_ENRICHMENT_FIELDS: &[&str] = &[
    // ── Boolean flags (present only when true) ───────────────────────────
    "has_doc",
    "is_recursive",
    "has_fallthrough",
    "is_const",
    "is_mutable",
    "is_unsafe",
    "is_async",
    "is_generic",
    "has_todo",
    "is_exported",
    "has_catch_all",
    "has_escape",
    "has_shadow",
    "expanded_has_escape",
    "expansion_failed",
    // ── Low-cardinality string enums ─────────────────────────────────────
    "cast_style",
    "cast_safety",
    "scope",
    "binding_kind",
    "naming",
    "comment_style",
    "member_kind",
    "for_style",
    "escape_tier",
    "storage",
    "operator_category",
    "guard_kind",
    "guard_branch",
    "catch_all_kind",
    "shift_direction",
    "increment_op",
    "increment_style",
];
/// Numeric columns for which the builder writes `zonemap_<col>.bin`."
///
/// A zone map is an 8-byte file `[min: u32 LE][max: u32 LE]` that lets the
/// query engine skip segments whose value range cannot satisfy a numeric
/// predicate (e.g. `WHERE line > 500` skips segments with max_line ≤ 500).
///
/// Only columns that carry meaningful range semantics are listed here;
/// columns that use `u32::MAX` as a sentinel for NULL are excluded from
/// zone-map building (the sentinel would inflate the max artificially).
pub const ZONEMAP_NUMERIC_FIELDS: &[(&str, bool)] = &[
    // (column_name, has_null_sentinel)
    ("line", false),
    ("usages_count", false),
    ("byte_start", false),
    ("byte_end", false),
];
/// Current on-disk schema version.  Bump when the format changes.
const SCHEMA_VERSION: u32 = 1;
/// Type-tag for dense `u32` columns (core columns).
const TYPE_TAG_U32: u8 = 3;
/// Type-tag for optional string columns — dense `[u32]` array where
/// `u32::MAX` encodes a missing value; IDs index into the segment string pool.
const TYPE_TAG_STR_OPT: u8 = 5;
/// Type-tag for dense `u64` columns stored as raw 8-byte LE sequences.
/// Sentinel: `0` (used by `col_rev` for analysis-only rows).
const TYPE_TAG_U64: u8 = 7;

/// Magic bytes at the start of every `.fqsf` single-file segment.
pub(crate) const FILE_MAGIC: [u8; 4] = *b"FQSF";
/// Format version of the `.fqsf` outer container (bump on incompatible change).
pub(crate) const FILE_VERSION: u32 = 1;
/// Maximum byte length of a blob name in the TOC (null-padded).
pub(crate) const ENTRY_NAME_LEN: usize = 56;
/// Byte size of each TOC entry: `name[56] + offset[4] + len[4]`.
pub(crate) const TOC_ENTRY_SIZE: usize = 64; // ENTRY_NAME_LEN + 4 + 4

// ---------------------------------------------------------------------------
// RowId — opaque per-row handle
// ---------------------------------------------------------------------------

/// Opaque identifier for a row within a [`SegmentBuilder`].
///
/// Returned by [`SegmentBuilder::emit_row`] and passed to
/// [`SegmentBuilder::set_field`] to attach enrichment fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowId(pub u32);

// ---------------------------------------------------------------------------
// SymbolRow — fixed columns for one row insertion
// ---------------------------------------------------------------------------

/// Describes one symbol row for insertion into a [`SegmentBuilder`].
///
/// Using a named struct rather than 7 positional arguments makes call sites
/// self-documenting and ensures that adding or reordering columns only
/// requires changing this struct and its construction sites.
#[derive(Debug, Clone, Copy)]
pub struct SymbolRow<'a> {
    /// Symbol name (e.g. `"my_function"`).
    pub name: &'a str,
    /// ForgeQL kind tag (e.g. `"function"`, `"class"`).
    pub fql_kind: &'a str,
    /// Source language (e.g. `"rust"`, `"cpp"`).
    pub language: &'a str,
    /// 1-based source line number.
    pub line: u32,
    /// Byte offset of the symbol's start in the source file.
    pub byte_start: u32,
    /// Byte offset of the symbol's end in the source file.
    pub byte_end: u32,
    /// Number of usages / references detected.
    pub usages_count: u32,
}

// ---------------------------------------------------------------------------
// FieldValue — polymorphic enrichment value
// ---------------------------------------------------------------------------

/// A value that can be stored in an optional enrichment column.
///
/// In Phase 03 only [`FieldValue::Str`] is used — all enrichment fields are
/// strings.  The other variants are reserved for later phases when numeric
/// and boolean columns land.
#[derive(Debug, Clone)]
pub enum FieldValue {
    /// String-typed enrichment value (e.g. `"true"`, `"camelCase"`, `"42"`).
    Str(String),
    /// Boolean flag (stored as a `u32` column: `0` = false, `1` = true).
    Bit(bool),
    /// 32-bit unsigned integer enrichment value.
    U32(u32),
}

impl From<bool> for FieldValue {
    fn from(v: bool) -> Self {
        Self::Bit(v)
    }
}
impl From<u32> for FieldValue {
    fn from(v: u32) -> Self {
        Self::U32(v)
    }
}
impl From<String> for FieldValue {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl<'a> From<&'a str> for FieldValue {
    fn from(v: &'a str) -> Self {
        Self::Str(v.to_owned())
    }
}

// ---------------------------------------------------------------------------
// ColumnDraft — per-optional-column row-parallel accumulator
// ---------------------------------------------------------------------------

enum ColumnDraft {
    Str(Vec<Option<String>>),
    Bit(Vec<Option<bool>>),
    U32(Vec<Option<u32>>),
}

impl ColumnDraft {
    /// Append a `None` slot to the column (called when a row does not have
    /// this field).
    fn push_none(&mut self) {
        match self {
            Self::Str(v) => v.push(None),
            Self::Bit(v) => v.push(None),
            Self::U32(v) => v.push(None),
        }
    }

    /// Current length (number of entries, including `None` slots).
    const fn len(&self) -> usize {
        match self {
            Self::Str(v) => v.len(),
            Self::Bit(v) => v.len(),
            Self::U32(v) => v.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// SegmentBuilder
// ---------------------------------------------------------------------------

/// Assembles the rows for one source file and flushes them to a segment dir.
///
/// Usage:
/// 1. Create with [`SegmentBuilder::new`].
/// 2. Call [`SegmentBuilder::emit_row`] once per `IndexRow` in the file;
///    capture the returned [`RowId`] to attach enrichment fields.
/// 3. Call [`SegmentBuilder::set_field`] to write per-row enrichment values.
/// 4. Call [`SegmentBuilder::flush`] to atomically write the segment.
pub struct SegmentBuilder {
    provider_id: [u8; 16],
    content_id: [u8; 32],
    content_id_len: u8,
    // Per-segment string interning (shared pool: names + kinds + languages).
    strings: Vec<String>,
    string_map: HashMap<String, u32>,
    // Column data (parallel arrays, one element per row).
    col_name_id: Vec<u32>,
    col_fql_kind_id: Vec<u32>,
    col_line: Vec<u32>,
    col_ordinal: Vec<u32>,
    /// Nearest indexed ancestor ordinal; `u32::MAX` = top-level.
    col_parent_ordinal: Vec<u32>,
    /// First 8 bytes of SHA-256 of the node byte span, as LE u64. 0 = n/a.
    col_rev: Vec<u64>,
    /// Ordinal of the first addressable child; `u32::MAX` = leaf.
    col_first_child_ordinal: Vec<u32>,
    /// Ordinal of the next addressable sibling; `u32::MAX` = last.
    col_next_sibling_ordinal: Vec<u32>,
    /// Ordinal of the previous addressable sibling; `u32::MAX` = first.
    col_prev_sibling_ordinal: Vec<u32>,
    col_byte_start: Vec<u32>,
    col_byte_end: Vec<u32>,
    col_usages_count: Vec<u32>,
    col_language_id: Vec<u32>,
    kind_postings: HashMap<u32, RoaringBitmap>,
    // symbol name → list of row indices, sorted by key for FST insertion.
    name_to_rows: BTreeMap<String, Vec<u32>>,
    /// Optional enrichment columns, keyed by field name.
    /// Each column is a parallel array with one slot per row.
    extra_cols: HashMap<String, ColumnDraft>,
}

impl SegmentBuilder {
    /// Create a builder for one segment.
    ///
    /// - `provider_id`: ASCII id for the content-address scheme (e.g. `"git-sha1"`),
    ///   null-padded to 16 bytes.
    /// - `content_id`: raw hash bytes identifying this file's content.
    #[must_use]
    pub fn new(provider_id: &str, content_id: &[u8]) -> Self {
        let mut pid = [0u8; 16];
        let pid_bytes = provider_id.as_bytes();
        let pid_len = pid_bytes.len().min(16);
        pid[..pid_len].copy_from_slice(&pid_bytes[..pid_len]);

        // len().min(32) is ≤ 32, which always fits in u8.
        let cid_len = u8::try_from(content_id.len().min(32)).unwrap_or(32u8);
        let mut cid = [0u8; 32];
        cid[..cid_len as usize].copy_from_slice(&content_id[..cid_len as usize]);

        Self {
            provider_id: pid,
            content_id: cid,
            content_id_len: cid_len,
            strings: Vec::new(),
            string_map: HashMap::new(),
            col_name_id: Vec::new(),
            col_fql_kind_id: Vec::new(),
            col_line: Vec::new(),
            col_ordinal: Vec::new(),
            col_parent_ordinal: Vec::new(),
            col_rev: Vec::new(),
            col_first_child_ordinal: Vec::new(),
            col_next_sibling_ordinal: Vec::new(),
            col_prev_sibling_ordinal: Vec::new(),
            col_byte_start: Vec::new(),
            col_byte_end: Vec::new(),
            col_usages_count: Vec::new(),
            col_language_id: Vec::new(),
            kind_postings: HashMap::new(),
            name_to_rows: BTreeMap::new(),
            extra_cols: HashMap::new(),
        }
    }

    /// Number of rows added so far.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "row count will never exceed u32::MAX (4 billion rows) in a real index"
    )]
    pub const fn row_count(&self) -> u32 {
        self.col_name_id.len() as u32
    }

    /// Add one symbol row; returns an opaque [`RowId`] for use with
    /// [`set_field`](Self::set_field).
    ///
    /// This is the canonical row-insertion method.  [`add_row`](Self::add_row)
    /// is a convenience wrapper that discards the returned [`RowId`].
    pub fn emit_row(&mut self, row: SymbolRow<'_>) -> RowId {
        let row_id = self.row_count();

        let name_id = self.intern(row.name);
        let kind_id = self.intern(row.fql_kind);
        let lang_id = self.intern(row.language);

        self.col_name_id.push(name_id);
        self.col_fql_kind_id.push(kind_id);
        self.col_line.push(row.line);
        self.col_ordinal.push(u32::MAX);
        self.col_parent_ordinal.push(u32::MAX);
        self.col_rev.push(0u64);
        self.col_first_child_ordinal.push(u32::MAX);
        self.col_next_sibling_ordinal.push(u32::MAX);
        self.col_prev_sibling_ordinal.push(u32::MAX);
        self.col_byte_start.push(row.byte_start);
        self.col_byte_end.push(row.byte_end);
        self.col_usages_count.push(row.usages_count);
        self.col_language_id.push(lang_id);
        let _inserted = self
            .kind_postings
            .entry(kind_id)
            .or_default()
            .insert(row_id);
        self.name_to_rows
            .entry(row.name.to_owned())
            .or_default()
            .push(row_id);

        // NOTE: do NOT pre-append None to extra_cols here.
        // `set_field` handles gap-filling, and `flush` pads all columns to
        // `row_count` via `resize(row_count, None)`.  Pre-appending caused
        // a double-append bug when `set_field` was called after `emit_row`
        // for the same row.

        RowId(row_id)
    }

    /// Convenience wrapper: add a row without returning the [`RowId`].
    ///
    /// Use [`emit_row`](Self::emit_row) when you need to attach enrichment
    /// fields via [`set_field`](Self::set_field).
    pub fn add_row(&mut self, row: SymbolRow<'_>) {
        let _ = self.emit_row(row);
    }

    /// Attach the stable row ordinal for node-id projection.
    ///
    /// Ordinals are stored in a dedicated fixed-width column (`col_ordinal`).
    pub fn set_ordinal(&mut self, row: RowId, ordinal: u32) {
        let row_idx = row.0 as usize;
        if let Some(slot) = self.col_ordinal.get_mut(row_idx) {
            *slot = ordinal;
        }
    }

    /// Set the parent-ordinal for `row` (nearest indexed ancestor; `u32::MAX` = top-level).
    pub fn set_parent_ordinal(&mut self, row: RowId, parent_ordinal: u32) {
        if let Some(s) = self.col_parent_ordinal.get_mut(row.0 as usize) {
            *s = parent_ordinal;
        }
    }

    /// Set the rev (first 8 bytes of SHA-256 of node bytes, LE u64) for `row`.
    pub fn set_rev(&mut self, row: RowId, rev: u64) {
        if let Some(s) = self.col_rev.get_mut(row.0 as usize) {
            *s = rev;
        }
    }

    /// Set the first-child ordinal for `row`.
    pub fn set_first_child_ordinal(&mut self, row: RowId, child_ordinal: u32) {
        if let Some(s) = self.col_first_child_ordinal.get_mut(row.0 as usize) {
            *s = child_ordinal;
        }
    }

    /// Set the next-sibling ordinal for `row`.
    pub fn set_next_sibling_ordinal(&mut self, row: RowId, sibling_ordinal: u32) {
        if let Some(s) = self.col_next_sibling_ordinal.get_mut(row.0 as usize) {
            *s = sibling_ordinal;
        }
    }

    /// Set the prev-sibling ordinal for `row`.
    pub fn set_prev_sibling_ordinal(&mut self, row: RowId, sibling_ordinal: u32) {
        if let Some(s) = self.col_prev_sibling_ordinal.get_mut(row.0 as usize) {
            *s = sibling_ordinal;
        }
    }

    /// Attach an enrichment field value to the row identified by `row`.
    ///
    /// - If the column `field` does not yet exist it is created and all rows
    ///   before `row` receive a `None` sentinel.
    /// - If the row index is beyond the column's current length, the gap is
    ///   filled with `None` sentinels.
    /// - A type mismatch between an existing column and the new value is
    ///   treated as absent (defensive; should not happen in practice).
    pub fn set_field(&mut self, row: RowId, field: &str, value: impl Into<FieldValue>) {
        let row_idx = row.0 as usize;
        let value = value.into();

        let col = self.extra_cols.entry(field.to_owned()).or_insert_with(|| {
            // Back-fill all rows before `row` with None.
            match &value {
                FieldValue::Str(_) => ColumnDraft::Str(vec![None; row_idx]),
                FieldValue::Bit(_) => ColumnDraft::Bit(vec![None; row_idx]),
                FieldValue::U32(_) => ColumnDraft::U32(vec![None; row_idx]),
            }
        });

        // Fill any gap up to `row_idx` with None.
        while col.len() < row_idx {
            col.push_none();
        }

        // Push the actual value (or None on type mismatch).
        match (col, value) {
            (ColumnDraft::Str(v), FieldValue::Str(s)) => v.push(Some(s)),
            (ColumnDraft::Bit(v), FieldValue::Bit(b)) => v.push(Some(b)),
            (ColumnDraft::U32(v), FieldValue::U32(u)) => v.push(Some(u)),
            (col, _) => col.push_none(), // type mismatch → absent
        }
    }

    /// Flush the segment to a single `.fqsf` file at `target_path`, atomically.
    ///
    /// Encodes all column data in memory and writes it as a single binary file
    /// with a table-of-contents (TOC) header, then renames into place.
    /// Returns `Ok(())` immediately when `target_path` already contains a
    /// valid `.fqsf` file.
    ///
    /// # Errors
    /// Propagates I/O errors from file creation / renaming.
    pub fn flush(mut self, target_path: &Path) -> Result<()> {
        if is_valid_segment(target_path) {
            return Ok(());
        }

        let row_count = self.col_name_id.len();

        // Pre-process extra enrichment columns into dense u32 arrays. MUST run
        // before encode_string_table so all string values are in the pool.
        let extra_arrays = self.dense_extra_columns(row_count);

        // Encode the fallible blobs before assembling the vec.
        let (offsets_bytes, data_bytes) = encode_string_table(&self.strings)?;
        let kind_postings_bytes = encode_kind_postings(&self.kind_postings)?;
        let (fst_bytes, name_post_bytes) = encode_name_fst(&self.name_to_rows)?;
        let name_prefix_bytes = encode_name_prefix(&self.name_to_rows)?;

        let mut blobs = self.core_column_blobs();
        blobs.extend([
            ("strings_offsets".to_owned(), offsets_bytes),
            ("strings_data".to_owned(), data_bytes),
            ("postings_fql_kind".to_owned(), kind_postings_bytes),
            ("name_fst".to_owned(), fst_bytes),
            ("name_postings".to_owned(), name_post_bytes),
            ("name_prefix".to_owned(), name_prefix_bytes),
        ]);

        // Extra enrichment columns.
        for (name, ids) in &extra_arrays {
            blobs.push((format!("col_{name}"), encode_u32_col(ids)));
        }
        // Per-field enrichment postings.
        for (name, bytes) in encode_enrichment_postings(&extra_arrays)? {
            blobs.push((name, bytes));
        }
        // Zone maps for numeric columns.
        let zone_cols: &[(&str, &[u32])] = &[
            ("line", &self.col_line),
            ("ordinal", &self.col_ordinal),
            ("usages_count", &self.col_usages_count),
            ("byte_start", &self.col_byte_start),
            ("byte_end", &self.col_byte_end),
        ];
        for (name, bytes) in encode_zone_maps(zone_cols) {
            blobs.push((name, bytes));
        }

        // FQSG header blob (encodes row_count, string_count, column list).
        let col_meta = column_metadata(&extra_arrays);
        blobs.push((
            "header".to_owned(),
            encode_header(
                &self.provider_id,
                &self.content_id,
                self.content_id_len,
                u32::try_from(row_count).context("row count overflow")?,
                u32::try_from(self.strings.len()).context("string count overflow")?,
                &col_meta,
            )?,
        ));

        write_segment_file(target_path, &blobs)
    }

    // --- private ---

    #[expect(
        clippy::expect_used,
        reason = "string pool overflow (> 4 billion unique strings) indicates a corrupt index; panic is the correct response"
    )]
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.string_map.get(s) {
            return id;
        }
        // This pool can hold 2^32 unique strings, which no real source file
        // will ever reach.  Panic is intentional — it signals a corrupt index.
        let id = u32::try_from(self.strings.len())
            .expect("segment string pool overflow (> 4 billion unique strings)");
        self.strings.push(s.to_owned());
        let _ = self.string_map.insert(s.to_owned(), id);
        id
    }
}

impl SegmentBuilder {
    /// Resolve the draft enrichment columns into dense `u32` arrays
    /// (`u32::MAX` = absent), interning string values into the shared pool.
    /// Must run before `encode_string_table` so every value is in the pool.
    fn dense_extra_columns(&mut self, row_count: usize) -> Vec<(String, Vec<u32>)> {
        let extra = std::mem::take(&mut self.extra_cols);
        extra
            .into_iter()
            .map(|(name, draft)| {
                let ids: Vec<u32> = match draft {
                    ColumnDraft::Str(mut vals) => {
                        vals.resize(row_count, None);
                        vals.into_iter()
                            .map(|v| v.map_or(u32::MAX, |s| self.intern(&s)))
                            .collect()
                    }
                    ColumnDraft::Bit(mut vals) => {
                        vals.resize(row_count, None);
                        vals.into_iter()
                            .map(|v| v.map_or(u32::MAX, u32::from))
                            .collect()
                    }
                    ColumnDraft::U32(mut vals) => {
                        vals.resize(row_count, None);
                        vals.into_iter().map(|v| v.unwrap_or(u32::MAX)).collect()
                    }
                };
                (name, ids)
            })
            .collect()
    }

    /// Encode the fixed core columns into `(name, bytes)` blob pairs.
    fn core_column_blobs(&self) -> Vec<(String, Vec<u8>)> {
        vec![
            ("col_name_id".to_owned(), encode_u32_col(&self.col_name_id)),
            (
                "col_fql_kind_id".to_owned(),
                encode_u32_col(&self.col_fql_kind_id),
            ),
            ("col_line".to_owned(), encode_u32_col(&self.col_line)),
            ("col_ordinal".to_owned(), encode_u32_col(&self.col_ordinal)),
            (
                "col_parent_ordinal".to_owned(),
                encode_u32_col(&self.col_parent_ordinal),
            ),
            ("col_rev".to_owned(), encode_u64_col(&self.col_rev)),
            (
                "col_first_child_ordinal".to_owned(),
                encode_u32_col(&self.col_first_child_ordinal),
            ),
            (
                "col_next_sibling_ordinal".to_owned(),
                encode_u32_col(&self.col_next_sibling_ordinal),
            ),
            (
                "col_prev_sibling_ordinal".to_owned(),
                encode_u32_col(&self.col_prev_sibling_ordinal),
            ),
            (
                "col_byte_start".to_owned(),
                encode_u32_col(&self.col_byte_start),
            ),
            (
                "col_byte_end".to_owned(),
                encode_u32_col(&self.col_byte_end),
            ),
            (
                "col_usages_count".to_owned(),
                encode_u32_col(&self.col_usages_count),
            ),
            (
                "col_language_id".to_owned(),
                encode_u32_col(&self.col_language_id),
            ),
        ]
    }
}

/// Column metadata (name, type tag) for the inner FQSG header blob: the fixed
/// core columns followed by one `STR_OPT` entry per extra enrichment column.
fn column_metadata(extra_arrays: &[(String, Vec<u32>)]) -> Vec<(&str, u8)> {
    let mut col_meta: Vec<(&str, u8)> = vec![
        ("name_id", TYPE_TAG_U32),
        ("fql_kind_id", TYPE_TAG_U32),
        ("line", TYPE_TAG_U32),
        ("ordinal", TYPE_TAG_U32),
        ("parent_ordinal", TYPE_TAG_U32),
        ("rev", TYPE_TAG_U64),
        ("first_child_ordinal", TYPE_TAG_U32),
        ("next_sibling_ordinal", TYPE_TAG_U32),
        ("prev_sibling_ordinal", TYPE_TAG_U32),
        ("byte_start", TYPE_TAG_U32),
        ("byte_end", TYPE_TAG_U32),
        ("usages_count", TYPE_TAG_U32),
        ("language_id", TYPE_TAG_U32),
    ];
    for (name, _) in extra_arrays {
        col_meta.push((name.as_str(), TYPE_TAG_STR_OPT));
    }
    col_meta
}

// ---------------------------------------------------------------------------
// Public helpers for use by shadow_writer / columnar_storage
// ---------------------------------------------------------------------------

/// Returns `true` if `path` is a valid `.fqsf` single-file segment.
pub(crate) fn is_valid_segment(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == FILE_MAGIC
}

// ---------------------------------------------------------------------------
// Encode helpers (in-memory encoding, no I/O)
// ---------------------------------------------------------------------------

/// Encode a `[u32]` column as raw little-endian bytes.
fn encode_u32_col(data: &[u32]) -> Vec<u8> {
    cast_slice::<u32, u8>(data).to_vec()
}

/// Encode a dense `u64` column as raw 8-byte LE sequences (zero-copy-safe on read).
fn encode_u64_col(data: &[u64]) -> Vec<u8> {
    data.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Encode the string intern table into `(offsets_bytes, data_bytes)`.
///
/// `offsets_bytes` is `[u32; string_count + 1]` where `offsets[i]` is the
/// byte start of string `i` in `data_bytes`; `offsets[string_count]` equals
/// the total length of `data_bytes`.
fn encode_string_table(strings: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut offsets: Vec<u32> = Vec::with_capacity(strings.len() + 1);
    let mut data: Vec<u8> = Vec::new();

    for s in strings {
        offsets.push(u32::try_from(data.len()).context("string table byte offset overflow")?);
        data.extend_from_slice(s.as_bytes());
    }
    offsets.push(u32::try_from(data.len()).context("string table final offset overflow")?);

    Ok((cast_slice::<u32, u8>(&offsets).to_vec(), data))
}

/// Encode `postings_fql_kind` bytes.
///
/// Format: `[kind_count: u32] (kind_id: u32, bitmap_len: u32, bitmap_bytes)*`
fn encode_kind_postings(kind_postings: &HashMap<u32, RoaringBitmap>) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let kind_count = u32::try_from(kind_postings.len()).context("too many fql kinds")?;
    buf.extend_from_slice(&kind_count.to_le_bytes());

    // Sort by kind_id for deterministic output.
    let mut sorted: Vec<(&u32, &RoaringBitmap)> = kind_postings.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);

    for (kind_id, bitmap) in &sorted {
        let mut bitmap_bytes: Vec<u8> = Vec::new();
        bitmap
            .serialize_into(&mut bitmap_bytes)
            .context("serialising Roaring bitmap")?;
        let len = u32::try_from(bitmap_bytes.len()).context("bitmap too large")?;
        buf.extend_from_slice(&kind_id.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&bitmap_bytes);
    }

    Ok(buf)
}

/// Encode per-field enrichment posting blobs.
///
/// Returns `(blob_name, blob_bytes)` pairs only for fields that have data
/// within the cardinality limit.  Same wire format as `encode_kind_postings`.
fn encode_enrichment_postings(
    extra_arrays: &[(String, Vec<u32>)],
) -> Result<Vec<(String, Vec<u8>)>> {
    const MAX_CARDINALITY: usize = 8;
    let mut result: Vec<(String, Vec<u8>)> = Vec::new();

    for field in POSTING_ENRICHMENT_FIELDS {
        let Some((_, ids)) = extra_arrays.iter().find(|(n, _)| n == field) else {
            continue;
        };

        // Build per-value bitmaps.
        let mut by_value: HashMap<u32, RoaringBitmap> = HashMap::new();
        for (row_id, &value_id) in ids.iter().enumerate() {
            if value_id == u32::MAX {
                continue; // NULL slot
            }
            if let Ok(row_u32) = u32::try_from(row_id) {
                let _ = by_value.entry(value_id).or_default().insert(row_u32);
            }
        }

        if by_value.is_empty() || by_value.len() > MAX_CARDINALITY {
            continue;
        }

        let mut buf: Vec<u8> = Vec::new();
        let value_count = u32::try_from(by_value.len()).context("too many enrichment values")?;
        buf.extend_from_slice(&value_count.to_le_bytes());

        let mut sorted: Vec<(&u32, &RoaringBitmap)> = by_value.iter().collect();
        sorted.sort_by_key(|(k, _)| *k);

        for (value_id, bitmap) in &sorted {
            let mut bitmap_bytes: Vec<u8> = Vec::new();
            bitmap
                .serialize_into(&mut bitmap_bytes)
                .context("serialising enrichment Roaring bitmap")?;
            let len = u32::try_from(bitmap_bytes.len()).context("enrichment bitmap too large")?;
            buf.extend_from_slice(&value_id.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&bitmap_bytes);
        }

        result.push((format!("postings_{field}"), buf));
    }

    Ok(result)
}

/// Encode zone-map blobs for columns listed in [`ZONEMAP_NUMERIC_FIELDS`].
///
/// Returns `(blob_name, blob_bytes)` pairs; columns with no data are omitted.
/// Each blob is exactly 8 bytes: `[min: u32 LE][max: u32 LE]`.
fn encode_zone_maps(core_cols: &[(&str, &[u32])]) -> Vec<(String, Vec<u8>)> {
    let mut result: Vec<(String, Vec<u8>)> = Vec::new();

    for (col_name, has_sentinel) in ZONEMAP_NUMERIC_FIELDS {
        let Some((_, data)) = core_cols.iter().find(|(n, _)| n == col_name) else {
            continue;
        };
        let mut min = u32::MAX;
        let mut max = 0u32;
        let mut found_any = false;
        for &v in *data {
            if *has_sentinel && v == u32::MAX {
                continue;
            }
            if !found_any || v < min {
                min = v;
            }
            if !found_any || v > max {
                max = v;
            }
            found_any = true;
        }
        if !found_any {
            continue;
        }
        let mut buf = [0u8; 8];
        buf[..4].copy_from_slice(&min.to_le_bytes());
        buf[4..].copy_from_slice(&max.to_le_bytes());
        result.push((format!("zonemap_{col_name}"), buf.to_vec()));
    }

    result
}

/// Encode the name prefix index blob.
///
/// Wire format: `[entry_count: u32 LE]`
/// followed by `( [prefix_len: u8] [prefix_bytes] [bitmap_len: u32 LE] [bitmap_bytes] )*`
fn encode_name_prefix(name_to_rows: &BTreeMap<String, Vec<u32>>) -> Result<Vec<u8>> {
    let mut prefix_map: HashMap<Vec<u8>, RoaringBitmap> = HashMap::new();

    for (name, rows) in name_to_rows {
        let lower = name.to_lowercase();
        let lower_bytes = lower.as_bytes();
        if lower_bytes.is_empty() {
            continue;
        }

        let first_char_len = lower.chars().next().map_or(0, char::len_utf8);
        let second_char_end = lower
            .char_indices()
            .nth(1)
            .map_or(first_char_len, |(i, c)| i + c.len_utf8());

        if first_char_len > 0 {
            let pfx1 = lower_bytes[..first_char_len].to_vec();
            let bm = prefix_map.entry(pfx1).or_default();
            for &row in rows {
                let _ = bm.insert(row);
            }
        }

        if second_char_end > first_char_len {
            let pfx2 = lower_bytes[..second_char_end].to_vec();
            let bm = prefix_map.entry(pfx2).or_default();
            for &row in rows {
                let _ = bm.insert(row);
            }
        }
    }

    let entry_count =
        u32::try_from(prefix_map.len()).context("name_prefix entry count overflow")?;
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&entry_count.to_le_bytes());

    let mut sorted: Vec<(&Vec<u8>, &RoaringBitmap)> = prefix_map.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);

    for (prefix, bitmap) in sorted {
        let prefix_len = u8::try_from(prefix.len()).context("prefix too long")?;
        buf.push(prefix_len);
        buf.extend_from_slice(prefix);
        let mut bitmap_bytes: Vec<u8> = Vec::new();
        bitmap
            .serialize_into(&mut bitmap_bytes)
            .context("serialising name_prefix bitmap")?;
        let len = u32::try_from(bitmap_bytes.len()).context("bitmap too large")?;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&bitmap_bytes);
    }

    Ok(buf)
}

/// Encode the name FST and name postings blobs.
///
/// Returns `(fst_bytes, postings_bytes)`.
///
/// FST value encoding: `(count as u64) | ((byte_offset as u64) << 32)`
/// where `byte_offset` indexes into `postings_bytes` (a flat `[u32 LE]` array).
fn encode_name_fst(name_to_rows: &BTreeMap<String, Vec<u32>>) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut postings_bytes: Vec<u8> = Vec::new();
    let mut fst_pairs: Vec<(&str, u64)> = Vec::with_capacity(name_to_rows.len());

    for (name, rows) in name_to_rows {
        let byte_offset =
            u32::try_from(postings_bytes.len()).context("name_postings byte offset overflow")?;
        let count = u32::try_from(rows.len()).context("row list too long")?;
        for &row_id in rows {
            postings_bytes.extend_from_slice(&row_id.to_le_bytes());
        }
        let encoded = u64::from(count) | (u64::from(byte_offset) << 32);
        fst_pairs.push((name.as_str(), encoded));
    }

    let fst_bytes = {
        let mut builder = MapBuilder::memory();
        for (name, encoded) in &fst_pairs {
            builder
                .insert(name.as_bytes(), *encoded)
                .with_context(|| format!("FST insert '{name}'"))?;
        }
        builder.into_inner().context("finalising FST")?
    };

    Ok((fst_bytes, postings_bytes))
}

/// Encode the inner FQSG header blob.
///
/// # Preamble layout (80 bytes, all little-endian)
///
/// | Offset | Size | Field           |
/// |--------|------|-----------------|
/// | 0      | 4    | magic `"FQSG"`  |
/// | 4      | 4    | schema_version  |
/// | 8      | 16   | provider_id     |
/// | 24     | 1    | content_id_len  |
/// | 25     | 3    | padding (zeros) |
/// | 28     | 32   | content_id      |
/// | 60     | 4    | row_count       |
/// | 64     | 4    | string_count    |
/// | 68     | 4    | column_count    |
/// | 72     | 8    | reserved        |
///
/// Followed by `column_count` variable-length entries:
/// `[u8: name_len][u8 × name_len: name][u8: type_tag][u64 LE: element_count]`
fn encode_header(
    provider_id: &[u8; 16],
    content_id: &[u8; 32],
    content_id_len: u8,
    row_count: u32,
    string_count: u32,
    cols: &[(&str, u8)],
) -> Result<Vec<u8>> {
    let column_count = u32::try_from(cols.len()).context("too many columns")?;
    let mut buf: Vec<u8> = Vec::with_capacity(80 + cols.len() * 20);

    buf.extend_from_slice(&MAGIC); // [0..4]
    buf.extend_from_slice(&SCHEMA_VERSION.to_le_bytes()); // [4..8]
    buf.extend_from_slice(provider_id); // [8..24]
    buf.push(content_id_len); // [24]
    buf.extend_from_slice(&[0u8; 3]); // [25..28] pad
    buf.extend_from_slice(content_id); // [28..60]
    buf.extend_from_slice(&row_count.to_le_bytes()); // [60..64]
    buf.extend_from_slice(&string_count.to_le_bytes()); // [64..68]
    buf.extend_from_slice(&column_count.to_le_bytes()); // [68..72]
    buf.extend_from_slice(&[0u8; 8]); // [72..80] reserved
    debug_assert_eq!(buf.len(), 80, "preamble must be exactly 80 bytes");

    for &(name, type_tag) in cols {
        let name_bytes = name.as_bytes();
        let name_len = u8::try_from(name_bytes.len()).context("column name too long")?;
        buf.push(name_len);
        buf.extend_from_slice(name_bytes);
        buf.push(type_tag);
        buf.extend_from_slice(&u64::from(row_count).to_le_bytes());
    }

    Ok(buf)
}

// ---------------------------------------------------------------------------
// Single-file writer
// ---------------------------------------------------------------------------

/// Assemble all blobs into a `.fqsf` file and write it atomically to
/// `target_path`.
///
/// File layout:
/// ```text
/// [0..4]   b"FQSF"          outer magic
/// [4..8]   version: u32 LE  = 1
/// [8..12]  entry_count: u32 LE
/// [12..]   TOC: entry_count × 64 bytes each:
///            name[56] (null-padded) + offset[4] LE + len[4] LE
/// [..]     data blobs concatenated
/// ```
fn write_segment_file(target_path: &Path, blobs: &[(String, Vec<u8>)]) -> Result<()> {
    let entry_count = u32::try_from(blobs.len()).context("too many blobs")?;
    let data_start = 12usize + blobs.len() * TOC_ENTRY_SIZE;

    // Compute absolute byte offsets (from file start) for each blob.
    // Blobs are 4-byte aligned so that cast_slice::<u8,u32> works on mmap slices.
    let mut offsets: Vec<u32> = Vec::with_capacity(blobs.len());
    let mut cursor = data_start;
    for (_, bytes) in blobs {
        offsets.push(u32::try_from(cursor).context("blob offset overflow")?);
        cursor += bytes.len();
        // Pad to next 4-byte boundary.
        cursor = (cursor + 3) & !3;
    }

    // Assemble the whole file in one allocation.
    let mut file_buf: Vec<u8> = Vec::with_capacity(cursor);

    // 12-byte outer header.
    file_buf.extend_from_slice(&FILE_MAGIC);
    file_buf.extend_from_slice(&FILE_VERSION.to_le_bytes());
    file_buf.extend_from_slice(&entry_count.to_le_bytes());

    // TOC entries (64 bytes each).
    for ((name, bytes), &offset) in blobs.iter().zip(offsets.iter()) {
        let mut entry = [0u8; TOC_ENTRY_SIZE];
        let nb = name.as_bytes();
        let copy_len = nb.len().min(ENTRY_NAME_LEN);
        entry[..copy_len].copy_from_slice(&nb[..copy_len]);
        entry[ENTRY_NAME_LEN..ENTRY_NAME_LEN + 4].copy_from_slice(&offset.to_le_bytes());
        let blob_len = u32::try_from(bytes.len()).context("blob too large")?;
        entry[ENTRY_NAME_LEN + 4..ENTRY_NAME_LEN + 8].copy_from_slice(&blob_len.to_le_bytes());
        file_buf.extend_from_slice(&entry);
    }

    // Data section: blobs concatenated with 4-byte alignment padding.
    for (_, bytes) in blobs {
        file_buf.extend_from_slice(bytes);
        // Pad to next 4-byte boundary with zeros.
        let pad = (4 - (bytes.len() & 3)) & 3;
        file_buf.extend_from_slice(&[0u8; 4][..pad]);
    }

    // Atomic write: tmp file → rename.
    let parent = target_path.parent().context("target_path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating parent dir {}", parent.display()))?;
    let stem = target_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let tmp_path = parent.join(format!(".tmp.{stem}.{}.fqsf", std::process::id()));

    std::fs::write(&tmp_path, &file_buf)
        .with_context(|| format!("writing tmp {}", tmp_path.display()))?;

    match std::fs::rename(&tmp_path, target_path) {
        Ok(()) => {}
        Err(_) if is_valid_segment(target_path) => {
            // Another writer won the race — our copy is redundant.
            let _ = std::fs::remove_file(&tmp_path);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| format!("renaming to {}", target_path.display()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_builder() -> SegmentBuilder {
        let content_id = [0xAB_u8; 20];
        SegmentBuilder::new("git-sha1", &content_id)
    }

    #[test]
    fn empty_segment_flushes_valid_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg.fqsf");
        let builder = make_builder();
        builder.flush(&target).expect("flush");

        assert!(target.exists(), "segment file not created");
        assert!(is_valid_segment(&target), "not a valid .fqsf segment");

        // Outer FQSF magic at byte 0.
        let bytes = std::fs::read(&target).expect("read");
        assert!(bytes.starts_with(b"FQSF"), "outer FQSF magic missing");
    }

    #[test]
    fn segment_with_rows_writes_all_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg.fqsf");

        let mut builder = make_builder();
        builder.add_row(SymbolRow {
            name: "foo",
            fql_kind: "function",
            language: "rust",
            line: 10,
            byte_start: 0,
            byte_end: 100,
            usages_count: 3,
        });
        builder.add_row(SymbolRow {
            name: "bar",
            fql_kind: "struct",
            language: "rust",
            line: 20,
            byte_start: 200,
            byte_end: 350,
            usages_count: 1,
        });
        builder.add_row(SymbolRow {
            name: "foo",
            fql_kind: "function",
            language: "rust",
            line: 30,
            byte_start: 400,
            byte_end: 500,
            usages_count: 3,
        });
        builder.flush(&target).expect("flush");

        assert!(is_valid_segment(&target), "not a valid .fqsf segment");

        // Verify outer format — FQSF magic present.
        let file_bytes = std::fs::read(&target).expect("read .fqsf");
        assert!(file_bytes.starts_with(b"FQSF"), "outer FQSF magic missing");

        // File must be large enough to hold 3 rows of column data (+TOC +header blob).
        // 3 rows × 7 core columns × 4 bytes = 84 bytes minimum of column data.
        assert!(file_bytes.len() > 200, "file too small for 3 rows");
    }

    #[test]
    fn flush_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg.fqsf");
        let mut builder = make_builder();
        builder.add_row(SymbolRow {
            name: "x",
            fql_kind: "variable",
            language: "cpp",
            line: 1,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        builder.flush(&target).expect("first flush");

        let first_size = std::fs::metadata(&target).expect("metadata").len();

        // Second flush with a different builder should be a no-op.
        let builder2 = make_builder();
        builder2
            .flush(&target)
            .expect("second flush should succeed");

        let second_size = std::fs::metadata(&target).expect("metadata").len();
        assert_eq!(
            first_size, second_size,
            "idempotent: second flush must not overwrite"
        );
    }

    #[test]
    fn provider_id_in_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg.fqsf");
        let content_id = [0xCD_u8; 20];
        let builder = SegmentBuilder::new("git-sha1", &content_id);
        builder.flush(&target).expect("flush");

        assert!(is_valid_segment(&target), "not a valid .fqsf segment");
        // provider_id is verified via SegmentReader in segment_reader tests.
    }
}
