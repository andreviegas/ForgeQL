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

use super::bytes_to_hex;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic bytes at the start of every `header.bin`.
pub const MAGIC: [u8; 4] = *b"FQSG";

/// Low-cardinality enrichment fields for which `SegmentBuilder` writes
/// per-segment Roaring-bitmap posting files (`postings_<field>.bin`).
///
/// Criterion: boolean flags and small enums with ≤ 8 distinct values per
/// segment.  The builder silently skips any field whose actual cardinality
/// exceeds that cap at flush time.
///
/// `SegmentReader` discovers the files by checking this list so readers
/// always know which fields to attempt to load.
pub const POSTING_ENRICHMENT_FIELDS: &[&str] = &[
    "has_doc",
    "is_recursive",
    "has_fallthrough",
    "is_const",
    "is_mutable",
    "is_unsafe",
    "is_async",
    "is_generic",
];
/// Current on-disk schema version.  Bump when the format changes.
const SCHEMA_VERSION: u32 = 1;
/// Type-tag for dense `u32` columns (core columns).
const TYPE_TAG_U32: u8 = 3;
/// Type-tag for optional string columns — dense `[u32]` array where
/// `u32::MAX` encodes a missing value; IDs index into the segment string pool.
const TYPE_TAG_STR_OPT: u8 = 5;

// ---------------------------------------------------------------------------
// RowId — opaque per-row handle
// ---------------------------------------------------------------------------

/// Opaque identifier for a row within a [`SegmentBuilder`].
///
/// Returned by [`SegmentBuilder::emit_row`] and passed to
/// [`SegmentBuilder::set_field`] to attach enrichment fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowId(u32);

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
    #[allow(clippy::missing_const_for_fn)] // match on &self enum is not const in stable Rust
    fn len(&self) -> usize {
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
    col_byte_start: Vec<u32>,
    col_byte_end: Vec<u32>,
    col_usages_count: Vec<u32>,
    col_language_id: Vec<u32>,
    // fql_kind_id → set of row indices, for `postings_fql_kind.bin`.
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
        #[allow(clippy::cast_possible_truncation)]
        let cid_len = content_id.len().min(32) as u8;
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
    #[allow(clippy::cast_possible_truncation)] // row counts fit in u32
    pub const fn row_count(&self) -> u32 {
        self.col_name_id.len() as u32
    }

    /// Add one symbol row; returns an opaque [`RowId`] for use with
    /// [`set_field`](Self::set_field).
    ///
    /// This is the canonical row-insertion method.  [`add_row`](Self::add_row)
    /// is a convenience wrapper that discards the returned [`RowId`].
    #[allow(clippy::too_many_arguments)] // 7 physical columns is intentional
    pub fn emit_row(
        &mut self,
        name: &str,
        fql_kind: &str,
        language: &str,
        line: u32,
        byte_start: u32,
        byte_end: u32,
        usages_count: u32,
    ) -> RowId {
        let row_id = self.row_count();

        let name_id = self.intern(name);
        let kind_id = self.intern(fql_kind);
        let lang_id = self.intern(language);

        self.col_name_id.push(name_id);
        self.col_fql_kind_id.push(kind_id);
        self.col_line.push(line);
        self.col_byte_start.push(byte_start);
        self.col_byte_end.push(byte_end);
        self.col_usages_count.push(usages_count);
        self.col_language_id.push(lang_id);

        let _inserted = self
            .kind_postings
            .entry(kind_id)
            .or_default()
            .insert(row_id);
        self.name_to_rows
            .entry(name.to_owned())
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
    #[allow(clippy::too_many_arguments)]
    pub fn add_row(
        &mut self,
        name: &str,
        fql_kind: &str,
        language: &str,
        line: u32,
        byte_start: u32,
        byte_end: u32,
        usages_count: u32,
    ) {
        let _ = self.emit_row(
            name,
            fql_kind,
            language,
            line,
            byte_start,
            byte_end,
            usages_count,
        );
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

    /// Flush the segment to `target_dir` atomically.
    ///
    /// Writes all files to a sibling `.tmp.*` directory, then renames it to
    /// `target_dir`.  If `target_dir` already contains a valid `header.bin`
    /// the function returns immediately (`Ok(())`) without re-writing.
    ///
    /// # Errors
    /// Propagates I/O errors from file creation / renaming.
    pub fn flush(mut self, target_dir: &Path) -> Result<()> {
        if is_valid_segment(target_dir) {
            return Ok(());
        }

        let row_count = self.col_name_id.len();

        // Pre-process extra enrichment columns: intern string values into the
        // shared pool, then convert to dense `u32` arrays (u32::MAX = absent).
        // This MUST run before `write_string_table` so all string values are
        // included in the pool.
        let extra_arrays: Vec<(String, Vec<u32>)> = {
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
        };

        let parent = target_dir.parent().context("target_dir has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;

        // Use pid + hex to make the temp name unique enough for concurrent writes.
        let hex = bytes_to_hex(&self.content_id[..self.content_id_len as usize]);
        let tmp = parent.join(format!(".tmp.{hex}.{}", std::process::id()));

        // Remove any stale temp dir from a previous crash.
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .with_context(|| format!("removing stale tmp {}", tmp.display()))?;
        }
        std::fs::create_dir_all(&tmp)
            .with_context(|| format!("creating tmp dir {}", tmp.display()))?;

        // --- write core columns ---
        write_u32_col(&tmp, "name_id", &self.col_name_id)?;
        write_u32_col(&tmp, "fql_kind_id", &self.col_fql_kind_id)?;
        write_u32_col(&tmp, "line", &self.col_line)?;
        write_u32_col(&tmp, "byte_start", &self.col_byte_start)?;
        write_u32_col(&tmp, "byte_end", &self.col_byte_end)?;
        write_u32_col(&tmp, "usages_count", &self.col_usages_count)?;
        write_u32_col(&tmp, "language_id", &self.col_language_id)?;

        // --- write string table (includes extra-column values) ---
        write_string_table(&tmp, &self.strings)?;

        // --- write kind postings ---
        write_kind_postings(&tmp, &self.kind_postings)?;

        // --- write FST + name postings ---
        write_name_fst(&tmp, &self.name_to_rows)?;

        // --- write extra enrichment columns ---
        for (name, ids) in &extra_arrays {
            write_u32_col(&tmp, name, ids)?;
        }

        // --- write per-field enrichment postings (additive, optional) ---
        write_enrichment_postings(&tmp, &extra_arrays)?;

        // --- build column metadata (dynamic to include extra cols) ---
        let mut col_meta: Vec<(&str, u8)> = vec![
            ("name_id", TYPE_TAG_U32),
            ("fql_kind_id", TYPE_TAG_U32),
            ("line", TYPE_TAG_U32),
            ("byte_start", TYPE_TAG_U32),
            ("byte_end", TYPE_TAG_U32),
            ("usages_count", TYPE_TAG_U32),
            ("language_id", TYPE_TAG_U32),
        ];
        for (name, _) in &extra_arrays {
            col_meta.push((name.as_str(), TYPE_TAG_STR_OPT));
        }

        // --- write header LAST (signals a complete segment) ---
        write_header(
            &tmp,
            &self.provider_id,
            &self.content_id,
            self.content_id_len,
            u32::try_from(row_count).context("row count overflow")?,
            u32::try_from(self.strings.len()).context("string count overflow")?,
            &col_meta,
        )?;

        // --- atomic rename ---
        match std::fs::rename(&tmp, target_dir) {
            Ok(()) => {}
            Err(_) if is_valid_segment(target_dir) => {
                // Another writer won the race — our segment is redundant.
                let _ = std::fs::remove_dir_all(&tmp);
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e).with_context(|| {
                    format!("renaming {} to {}", tmp.display(), target_dir.display())
                });
            }
        }

        Ok(())
    }

    // --- private ---

    #[allow(clippy::expect_used)]
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

// ---------------------------------------------------------------------------
// Public helpers for use by shadow_writer
// ---------------------------------------------------------------------------

/// Returns `true` if `dir` contains a `header.bin` with the `FQSG` magic.
pub(crate) fn is_valid_segment(dir: &Path) -> bool {
    std::fs::read(dir.join("header.bin"))
        .ok()
        .is_some_and(|b| b.starts_with(&MAGIC))
}

// ---------------------------------------------------------------------------
// File-writing helpers
// ---------------------------------------------------------------------------

/// Write `col_<name>.bin` as a little-endian `[u32]` array.
fn write_u32_col(dir: &Path, name: &str, data: &[u32]) -> Result<()> {
    // cast_slice is safe: Vec<u32> is u32-aligned; we're on little-endian x86/ARM.
    let bytes: &[u8] = cast_slice(data);
    std::fs::write(dir.join(format!("col_{name}.bin")), bytes)
        .with_context(|| format!("writing col_{name}.bin"))
}

/// Write `strings_offsets.bin` + `strings_data.bin`.
///
/// `strings_offsets.bin` is `[u32; string_count + 1]` where
/// `offsets[i]` is the byte start of string `i` in `strings_data.bin` and
/// `offsets[string_count]` equals the total size of `strings_data.bin`.
fn write_string_table(dir: &Path, strings: &[String]) -> Result<()> {
    let mut offsets: Vec<u32> = Vec::with_capacity(strings.len() + 1);
    let mut data: Vec<u8> = Vec::new();

    for s in strings {
        offsets.push(u32::try_from(data.len()).context("string table byte offset overflow")?);
        data.extend_from_slice(s.as_bytes());
    }
    offsets.push(u32::try_from(data.len()).context("string table final offset overflow")?);

    std::fs::write(
        dir.join("strings_offsets.bin"),
        cast_slice::<u32, u8>(&offsets),
    )
    .context("writing strings_offsets.bin")?;
    std::fs::write(dir.join("strings_data.bin"), &data).context("writing strings_data.bin")?;
    Ok(())
}

/// Write `postings_fql_kind.bin`.
///
/// Format: `[kind_count: u32] (kind_id: u32, bitmap_len: u32, bitmap_bytes)*`
fn write_kind_postings(dir: &Path, kind_postings: &HashMap<u32, RoaringBitmap>) -> Result<()> {
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

    std::fs::write(dir.join("postings_fql_kind.bin"), &buf).context("writing postings_fql_kind.bin")
}

/// Write per-field Roaring-bitmap posting files for low-cardinality enrichment
/// fields listed in [`POSTING_ENRICHMENT_FIELDS`].
///
/// For each field in the allowlist the builder checks whether the field is
/// present in `extra_arrays` and whether its cardinality (distinct non-NULL
/// values) does not exceed `MAX_CARDINALITY`.  If both conditions hold it
/// writes a `postings_<field>.bin` file using the same wire format as
/// `postings_fql_kind.bin`:
///
/// ```text
/// [value_count: u32 LE]
/// ( [value_id: u32 LE]
///   [bitmap_len: u32 LE]
///   [bitmap_bytes: roaring serialized] )*
/// ```
///
/// Old readers that don't know about these files safely ignore them; new
/// readers that find them use them to skip rows that don't match a WHERE
/// predicate before materialisation.
fn write_enrichment_postings(dir: &Path, extra_arrays: &[(String, Vec<u32>)]) -> Result<()> {
    const MAX_CARDINALITY: usize = 8;

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

        // Skip if empty or cardinality too high.
        if by_value.is_empty() || by_value.len() > MAX_CARDINALITY {
            continue;
        }

        // Serialise: same wire format as `write_kind_postings`.
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

        let filename = format!("postings_{field}.bin");
        std::fs::write(dir.join(&filename), &buf).with_context(|| format!("writing {filename}"))?;
    }
    Ok(())
}

/// Write `name.fst` + `name_postings.bin`.
///
/// FST value encoding: `(count as u64) | ((byte_offset as u64) << 32)`
/// where `byte_offset` points into `name_postings.bin` (a flat `[u32 LE]`
/// array of row IDs).
fn write_name_fst(dir: &Path, name_to_rows: &BTreeMap<String, Vec<u32>>) -> Result<()> {
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

    // Write name_postings.bin.
    std::fs::write(dir.join("name_postings.bin"), &postings_bytes)
        .context("writing name_postings.bin")?;

    // Build and write FST (keys are already sorted because BTreeMap gives
    // them in lexicographic order).
    let fst_bytes = {
        let mut builder = MapBuilder::memory();
        for (name, encoded) in &fst_pairs {
            builder
                .insert(name.as_bytes(), *encoded)
                .with_context(|| format!("FST insert '{name}'"))?;
        }
        builder.into_inner().context("finalising FST")?
    };

    std::fs::write(dir.join("name.fst"), &fst_bytes).context("writing name.fst")
}

/// Write `header.bin`: fixed 80-byte preamble followed by column entries.
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
fn write_header(
    dir: &Path,
    provider_id: &[u8; 16],
    content_id: &[u8; 32],
    content_id_len: u8,
    row_count: u32,
    string_count: u32,
    cols: &[(&str, u8)],
) -> Result<()> {
    let column_count = u32::try_from(cols.len()).context("too many columns")?;
    let mut buf: Vec<u8> = Vec::with_capacity(80 + cols.len() * 20);

    // --- fixed 80-byte preamble ---
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

    // --- column entries (variable length) ---
    for &(name, type_tag) in cols {
        let name_bytes = name.as_bytes();
        let name_len = u8::try_from(name_bytes.len()).context("column name too long")?;
        buf.push(name_len);
        buf.extend_from_slice(name_bytes);
        buf.push(type_tag);
        buf.extend_from_slice(&u64::from(row_count).to_le_bytes()); // element_count
    }

    std::fs::write(dir.join("header.bin"), &buf).context("writing header.bin")
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
        let target = dir.path().join("seg");
        let builder = make_builder();
        builder.flush(&target).expect("flush");

        let header = std::fs::read(target.join("header.bin")).expect("header.bin");
        assert!(header.starts_with(b"FQSG"), "magic missing");

        // schema_version == 1
        let ver = u32::from_le_bytes(header[4..8].try_into().unwrap());
        assert_eq!(ver, 1);
    }

    #[test]
    fn segment_with_rows_writes_all_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg");

        let mut builder = make_builder();
        builder.add_row("foo", "function", "rust", 10, 0, 100, 3);
        builder.add_row("bar", "struct", "rust", 20, 200, 350, 1);
        builder.add_row("foo", "function", "rust", 30, 400, 500, 3);
        builder.flush(&target).expect("flush");

        for name in &[
            "header.bin",
            "col_name_id.bin",
            "col_fql_kind_id.bin",
            "col_line.bin",
            "col_byte_start.bin",
            "col_byte_end.bin",
            "col_usages_count.bin",
            "col_language_id.bin",
            "strings_offsets.bin",
            "strings_data.bin",
            "postings_fql_kind.bin",
            "name.fst",
            "name_postings.bin",
        ] {
            assert!(target.join(name).exists(), "missing file: {name}");
        }

        // col_line.bin should have 3 × 4 = 12 bytes.
        let col_line = std::fs::read(target.join("col_line.bin")).expect("col_line.bin");
        assert_eq!(col_line.len(), 12);
        let lines: &[u32] = cast_slice(&col_line);
        assert_eq!(lines, [10, 20, 30]);
    }

    #[test]
    fn flush_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg");
        let mut builder = make_builder();
        builder.add_row("x", "variable", "cpp", 1, 0, 10, 0);
        builder.flush(&target).expect("first flush");

        // Second flush with a different builder should be a no-op.
        let builder2 = make_builder();
        builder2
            .flush(&target)
            .expect("second flush should succeed");

        // col_line.bin was written by first flush (1 row) and must not be overwritten.
        let col_line = std::fs::read(target.join("col_line.bin")).expect("col_line.bin");
        assert_eq!(
            col_line.len(),
            4,
            "idempotent: second flush must not truncate"
        );
    }

    #[test]
    fn provider_id_in_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("seg");
        let content_id = [0xCD_u8; 20];
        let builder = SegmentBuilder::new("git-sha1", &content_id);
        builder.flush(&target).expect("flush");

        let header = std::fs::read(target.join("header.bin")).expect("header.bin");
        // provider_id is at bytes [8..24].
        let provider_bytes = &header[8..24];
        let provider = std::str::from_utf8(provider_bytes.split(|&b| b == 0).next().unwrap_or(b""))
            .expect("provider_id not UTF-8");
        assert_eq!(provider, "git-sha1");
    }
}
