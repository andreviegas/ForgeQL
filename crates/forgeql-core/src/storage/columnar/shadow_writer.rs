//! [`ShadowWriter`] — drives per-file shadow-write from a built [`SymbolTable`].
//!
//! After the legacy index build completes, `ShadowWriter::run` iterates
//! every source file present in the symbol table, computes the content hash
//! via a caller-supplied hash function (Issue 1: no more `git_blob_sha1`
//! coupling), and writes the corresponding columnar segment to
//! `<segments_base>/<provider_id>/<hex>/`.
//!
//! **Content-ID caching (Issue 3)**: callers may supply a pre-computed
//! `HashMap<PathBuf, Vec<u8>>` populated inline during `index_file` via
//! [`SegmentBuildCtx`].  When a pre-computed ID is available, the source
//! file is **not** re-read; only the per-symbol enrichment fields and core
//! columns are extracted from the already-built `SymbolTable`.
//!
//! **Enrichment fields (Issue 2)**: each [`IndexRow`]'s `fields` map
//! (populated by enrichers during the AST build) is transferred to the
//! segment via [`SegmentBuilder::set_field`].
//!
//! **Background flush (Issue 4)**: a background [`std::thread`] flushes
//! segment directories to disk while the main loop builds the next one,
//! overlapping CPU and I/O.  The flusher is joined before `run` returns.
//!
//! **Manifest (Issue 5)**: after a successful flush run, the manifest at
//! `<segments_base>/../manifest.json` is updated with newly discovered
//! enrichment column names and the cumulative segment count.
//!
//! Shadow-write is **idempotent**: existing valid segments are skipped.
//! Per-file errors are logged as warnings and never abort the build.
//!
//! [`SegmentBuildCtx`]: crate::ast::index::SegmentBuildCtx
//! [`IndexRow`]: crate::ast::index::IndexRow
//! [`SymbolTable`]: crate::ast::index::SymbolTable

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;
use tracing::{debug, warn};

use crate::ast::index::SymbolTable;

use super::bytes_to_hex;
use super::manifest::Manifest;
use super::segment_builder::{RowId, SegmentBuilder, SymbolRow, is_valid_segment};

/// Iterates a [`SymbolTable`] and writes one columnar segment per source file.
pub struct ShadowWriter<'a> {
    table: &'a SymbolTable,
    segments_base: &'a Path,
    /// Provider identifier (e.g. `"git-sha1"`), used as the segment
    /// sub-directory name and embedded in `header.bin`.
    provider_id: &'a str,
    /// Content-addressing hash function supplied by the caller.
    ///
    /// For `GitSha1Provider` this wraps `git_blob_sha1`.  The closure is
    /// called only when `pre_computed` does not contain the file's path.
    hash_content: &'a (dyn Fn(&[u8]) -> Vec<u8> + Send + Sync),
    /// Pre-computed content IDs populated inline during `index_file` via
    /// [`SegmentBuildCtx::emit_fn`].  Keys are the **absolute** source
    /// file paths stored in the symbol table.
    ///
    /// When a path is found here the source file is not re-read, avoiding
    /// the double-read overhead (Issue 3).
    pre_computed: HashMap<PathBuf, Vec<u8>>,
}

impl<'a> ShadowWriter<'a> {
    /// Create a shadow writer.
    ///
    /// - `table`: fully-built symbol table.
    /// - `segments_base`: path to `<bare-repo>/forgeql/segments/`.
    /// - `provider_id`: stable string identifying the hash algorithm
    ///   (e.g. `"git-sha1"`).  Used as the sub-directory name.
    /// - `hash_content`: closure that maps raw file bytes to the raw
    ///   content-ID bytes.
    /// - `pre_computed`: content IDs collected inline during the build
    ///   (may be empty).
    #[must_use]
    pub fn new(
        table: &'a SymbolTable,
        segments_base: &'a Path,
        provider_id: &'a str,
        hash_content: &'a (dyn Fn(&[u8]) -> Vec<u8> + Send + Sync),
        pre_computed: HashMap<PathBuf, Vec<u8>>,
    ) -> Self {
        Self {
            table,
            segments_base,
            provider_id,
            hash_content,
            pre_computed,
        }
    }

    /// Write one columnar segment per source file in the symbol table.
    ///
    /// Returns a [`ShadowWriteResult`] containing the count of newly-written
    /// segments and a map from absolute source path to content-ID bytes for
    /// every file that was processed (including previously-valid segments).
    /// The segment map is used by the overlay builder to know which segments
    /// exist without re-hashing every file.
    ///
    /// # Errors
    /// Returns `Err` only for fatal infrastructure failures (e.g. unable to
    /// create the provider directory).  Per-file errors are logged as
    /// warnings and skipped.
    #[allow(clippy::too_many_lines)]
    pub fn run(self) -> Result<ShadowWriteResult> {
        // Group row indices by path_id so each file is processed once.
        let mut by_path: HashMap<u32, Vec<usize>> = HashMap::new();
        for (idx, row) in self.table.rows.iter().enumerate() {
            by_path.entry(row.path_id).or_default().push(idx);
        }

        if by_path.is_empty() {
            return Ok(ShadowWriteResult {
                count: 0,
                segment_map: HashMap::new(),
            });
        }

        // Ensure the versioned provider-specific segment directory exists.
        let provider_dir =
            self.segments_base
                .join(format!("{}-v{}", self.provider_id, super::ENRICH_VER));
        std::fs::create_dir_all(&provider_dir)?;

        // ── Parallel build + flush (Issue 4 replacement) ──────────────────────
        // Each file is fully independent: compute content-ID, check idempotency,
        // build SegmentBuilder, flush to disk.  Rayon distributes across all
        // available cores.  Results are collected and merged sequentially after.
        //
        // Each worker returns:
        //   (abs_path, content_id, Option<BTreeSet<String>>, flushed: bool)
        // where the Option is Some(columns) when a new segment was written.
        //
        // `pre_computed` lookup is via shared-ref `get` (no remove) so the
        // HashMap can be shared immutably across workers.  The 20-byte clone
        // overhead per file is negligible.
        let table = self.table;
        let provider_id = self.provider_id;
        let hash_content = self.hash_content;
        let pre_computed = &self.pre_computed;

        // Usage postings (BUG-006): group the merged table's usage sites by
        // path_id ONCE up front — scanning the whole usages map per file
        // inside the parallel loop would be quadratic at repo scale.
        let mut usages_by_path: HashMap<u32, Vec<(&str, u32)>> = HashMap::new();
        for (name, sites) in &table.usages {
            for site in sites {
                usages_by_path
                    .entry(site.path_id)
                    .or_default()
                    .push((name.as_str(), u32::try_from(site.line).unwrap_or(u32::MAX)));
            }
        }
        let usages_by_path = &usages_by_path;

        let results: Vec<WorkResult> = by_path
            .values()
            .collect::<Vec<_>>()
            .into_par_iter()
            .filter_map(|row_indices| {
                build_file_segment(
                    row_indices,
                    table,
                    provider_id,
                    hash_content,
                    pre_computed,
                    usages_by_path,
                    &provider_dir,
                )
            })
            .collect();

        // ── Merge results (sequential, fast) ─────────────────────────────────
        let mut all_columns: BTreeSet<String> = BTreeSet::new();
        let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::with_capacity(results.len());
        let mut written: usize = 0;

        for (abs_path, content_id, columns_opt) in results {
            let _ = segment_map.insert(abs_path, content_id);
            if let Some(cols) = columns_opt {
                all_columns.extend(cols);
                written += 1;
            }
        }

        // Update the manifest with newly discovered column names (Issue 5).
        if written > 0 {
            let manifest_path = self
                .segments_base
                .parent()
                .unwrap_or(self.segments_base)
                .join(format!(
                    "manifest-{}-v{}.json",
                    self.provider_id,
                    super::ENRICH_VER
                ));
            if let Err(e) = Manifest::update(
                &manifest_path,
                self.provider_id,
                &all_columns,
                written as u64,
            ) {
                warn!("shadow-write: manifest update failed: {e}");
            }
        }

        Ok(ShadowWriteResult {
            count: written,
            segment_map,
        })
    }
}

/// One worker result: the source file's absolute path, its content id, and the
/// set of enrichment columns written (`Some` only when a new segment was built).
type WorkResult = (PathBuf, Vec<u8>, Option<BTreeSet<String>>);

/// Build (and flush) one shadow segment for all rows of a single source file.
/// Runs on a Rayon worker — fully independent per file. Returns `None` when the
/// file is unreadable; `Some((path, content_id, None))` when an up-to-date
/// segment already exists or the flush failed; `Some((.., Some(columns)))` when
/// a fresh segment was written.
fn build_file_segment(
    row_indices: &[usize],
    table: &SymbolTable,
    provider_id: &str,
    hash_content: &(dyn Fn(&[u8]) -> Vec<u8> + Send + Sync),
    pre_computed: &HashMap<PathBuf, Vec<u8>>,
    usages_by_path: &HashMap<u32, Vec<(&str, u32)>>,
    provider_dir: &Path,
) -> Option<WorkResult> {
    // row_indices is non-empty by construction.
    let first_row = &table.rows[row_indices[0]];
    let abs_path = table.path_of(first_row).to_path_buf();

    // Content ID: use the pre-computed value when available, otherwise read + hash.
    let content_id: Vec<u8> = if let Some(cid) = pre_computed.get(&abs_path) {
        cid.clone()
    } else {
        match std::fs::read(&abs_path) {
            Ok(bytes) => hash_content(&bytes),
            Err(e) => {
                warn!(
                    path = %abs_path.display(),
                    "shadow-write: skipping unreadable file: {e}"
                );
                return None;
            }
        }
    };

    let hex = bytes_to_hex(&content_id);
    // 2-char git-style prefix sharding to avoid flat directories.
    let target_path = provider_dir
        .join(&hex[..2])
        .join(format!("{}.fqsf", &hex[2..]));

    // Idempotent: skip already-valid segments.
    if is_valid_segment(&target_path) {
        debug!(
            path = %abs_path.display(),
            hex = %hex,
            "shadow-write: segment already valid, skipping"
        );
        return Some((abs_path, content_id, None));
    }

    // Build segment: core columns + enrichment fields.
    let mut builder = SegmentBuilder::new(provider_id, &content_id);
    let mut local_columns: BTreeSet<String> = BTreeSet::new();
    // (ordinal, row_id, parent_ordinal) for the navigation post-pass.
    let mut ordinal_row: Vec<(u32, u32, u32)> = Vec::new();
    for &idx in row_indices {
        let row = &table.rows[idx];
        let row_id = builder.emit_row(SymbolRow {
            name: table.name_of(row),
            fql_kind: table.fql_kind_of(row),
            language: table.language_of(row),
            line: u32::try_from(row.line).unwrap_or(u32::MAX),
            byte_start: u32::try_from(row.byte_range.start).unwrap_or(u32::MAX),
            byte_end: u32::try_from(row.byte_range.end).unwrap_or(u32::MAX),
            usages_count: row.usages_count,
        });
        if let Some(ordinal) = row.ordinal {
            builder.set_ordinal(row_id, ordinal);
            builder.set_parent_ordinal(row_id, row.parent_ordinal);
            builder.set_rev(row_id, row.rev);
            ordinal_row.push((ordinal, row_id.0, row.parent_ordinal));
        }
        for (key, value) in table.resolve_fields(&row.fields) {
            if key == "parent_ordinal" {
                continue;
            } // now a typed column
            let _ = local_columns.insert(key.clone());
            builder.set_field(row_id, &key, value);
        }
    }

    fill_navigation(&mut builder, &ordinal_row);

    // Usage postings (BUG-006): pre-grouped by path_id in ShadowWriter::run.
    if let Some(sites) = usages_by_path.get(&first_row.path_id) {
        for &(name, line) in sites {
            builder.add_usage(name, line);
        }
    }

    // Flush to disk inside the worker.
    match builder.flush(&target_path) {
        Ok(()) => Some((abs_path, content_id, Some(local_columns))),
        Err(e) => {
            warn!(
                target = %target_path.display(),
                "shadow-write: flush failed: {e}"
            );
            Some((abs_path, content_id, None))
        }
    }
}

/// Navigation post-pass: fill `first_child` / `prev_sibling` / `next_sibling`
/// links on a freshly built segment. Groups addressable rows by parent ordinal
/// and orders each sibling group by ordinal (= DFS order).
fn fill_navigation(builder: &mut SegmentBuilder, ordinal_row: &[(u32, u32, u32)]) {
    let mut by_parent: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    let mut ord_to_row: HashMap<u32, u32> = HashMap::new();
    for &(ord, rid, parent) in ordinal_row {
        by_parent.entry(parent).or_default().push((ord, rid));
        let _ = ord_to_row.insert(ord, rid);
    }
    for (parent_ord, mut children) in by_parent {
        children.sort_unstable_by_key(|&(ord, _)| ord);
        if let Some(&parent_rid) = ord_to_row.get(&parent_ord)
            && let Some(&(first_ord, _)) = children.first()
        {
            builder.set_first_child_ordinal(RowId(parent_rid), first_ord);
        }
        for i in 0..children.len() {
            let (_, this_rid) = children[i];
            if i > 0 {
                builder.set_prev_sibling_ordinal(RowId(this_rid), children[i - 1].0);
            }
            if i + 1 < children.len() {
                builder.set_next_sibling_ordinal(RowId(this_rid), children[i + 1].0);
            }
        }
    }
}

/// Result returned by [`ShadowWriter::run`].
///
/// Contains the count of newly-written segments and the full mapping from
/// absolute source file path to content-ID bytes for every file processed
/// (including files whose segments already existed on disk and were skipped).
///
/// The `segment_map` is consumed by [`OverlayBuilder`] immediately after
/// the shadow write completes, while the mapping is still fresh in memory,
/// to avoid re-hashing source files when building the workspace overlay.
///
/// [`OverlayBuilder`]: super::overlay_builder::OverlayBuilder
pub struct ShadowWriteResult {
    /// Number of new segments actually flushed to disk.
    pub count: usize,
    /// `abs_source_path → content_id_bytes` for every file in the symbol table
    /// that was successfully processed (whether or not a new segment was written).
    pub segment_map: HashMap<PathBuf, Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Tests (Issue 6)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements
)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ast::index::{IndexRow, SymbolTable};

    /// Build a minimal `SymbolTable` with one row for `file_name` in `dir`.
    fn make_table(
        dir: &Path,
        file_name: &str,
        content: &[u8],
        name: &str,
        fql_kind: &str,
        enrichment: HashMap<String, String>,
    ) -> SymbolTable {
        std::fs::write(dir.join(file_name), content).expect("write source file");
        let mut table = SymbolTable::default();
        let path = dir.join(file_name);
        let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = table
            .strings
            .intern_row(name, fql_kind, fql_kind, "rust", &path);
        let fields = table.strings.intern_fields(enrichment);
        table.push_row(IndexRow {
            byte_range: 0..content.len(),
            line: 1,
            usages_count: 0,
            ordinal: None,
            parent_ordinal: u32::MAX,
            rev: 0,
            fields,
            name_id,
            node_kind_id,
            fql_kind_id,
            language_id,
            path_id,
        });
        table
    }

    /// Simple identity hash: content bytes → content bytes (deterministic for tests).
    fn identity_hash(b: &[u8]) -> Vec<u8> {
        // Use a fixed short hash to keep directory names short.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        b.hash(&mut h);
        h.finish().to_le_bytes().to_vec()
    }

    #[test]
    fn empty_table_writes_no_segments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let table = SymbolTable::default();
        let segments_base = tmp.path().join("segments");
        let writer = ShadowWriter::new(
            &table,
            &segments_base,
            "test",
            &identity_hash,
            HashMap::new(),
        );
        let result = writer.run().expect("run");
        assert_eq!(result.count, 0, "no segments for empty table");
        assert!(
            result.segment_map.is_empty(),
            "no segment_map entries for empty table"
        );
        assert!(
            !segments_base.exists(),
            "segments dir should not be created for empty table"
        );
    }

    #[test]
    fn writes_one_segment_per_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let table = make_table(
            tmp.path(),
            "lib.rs",
            b"fn hello() {}",
            "hello",
            "function",
            HashMap::new(),
        );
        let segments_base = tmp.path().join("segments");
        let writer = ShadowWriter::new(
            &table,
            &segments_base,
            "test",
            &identity_hash,
            HashMap::new(),
        );
        let result = writer.run().expect("run");
        assert_eq!(result.count, 1, "one segment written");
        assert_eq!(result.segment_map.len(), 1, "segment_map has one entry");

        // Verify the provider directory and one .fqsf segment file exist.
        let provider_dir =
            segments_base.join(format!("test-v{}", crate::storage::columnar::ENRICH_VER));
        let entries: Vec<_> = std::fs::read_dir(&provider_dir)
            .expect("read provider_dir")
            .collect();
        assert_eq!(entries.len(), 1, "exactly one prefix shard dir");

        // The 2-char prefix dir contains the actual .fqsf segment file.
        let prefix_dir = entries[0].as_ref().expect("prefix dir entry").path();
        let seg_entries: Vec<_> = std::fs::read_dir(&prefix_dir)
            .expect("read prefix_dir")
            .collect();
        assert_eq!(seg_entries.len(), 1, "exactly one segment file");
        let seg_path = seg_entries[0].as_ref().expect("file entry").path();
        assert!(
            seg_path.extension().is_some_and(|e| e == "fqsf"),
            "segment has .fqsf extension"
        );
        let header_magic = &std::fs::read(&seg_path).expect("read .fqsf")[..4];
        assert_eq!(header_magic, b"FQSF", "file has FQSF magic");
    }

    #[test]
    fn enrichment_fields_written_to_extra_columns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut enrichment = HashMap::new();
        enrichment.insert("is_const".to_owned(), "true".to_owned());
        enrichment.insert("naming".to_owned(), "UPPER_SNAKE".to_owned());

        let table = make_table(
            tmp.path(),
            "consts.rs",
            b"const X: u32 = 42;",
            "X",
            "variable",
            enrichment,
        );
        let segments_base = tmp.path().join("segments");
        let writer = ShadowWriter::new(
            &table,
            &segments_base,
            "test",
            &identity_hash,
            HashMap::new(),
        );
        writer.run().expect("run");

        // Verify the segment directory has extra enrichment column files.
        let provider_dir =
            segments_base.join(format!("test-v{}", crate::storage::columnar::ENRICH_VER));
        let prefix_dir = std::fs::read_dir(&provider_dir)
            .expect("provider_dir")
            .next()
            .expect("one prefix entry")
            .expect("dir entry")
            .path();
        let seg_path = std::fs::read_dir(&prefix_dir)
            .expect("prefix_dir")
            .next()
            .expect("one entry")
            .expect("file entry")
            .path();

        // Open the .fqsf and verify extra column count via SegmentReader.
        let reader =
            crate::storage::columnar::SegmentReader::open(&seg_path).expect("open .fqsf segment");
        // extra_col_names() should have at least 2 enrichment fields.
        assert!(
            reader.extra_col_count() >= 2,
            "enrichment columns present (got {})",
            reader.extra_col_count()
        );
    }

    #[test]
    fn pre_computed_avoids_file_read() {
        // Write a table but delete the source file before running the writer.
        // With a pre-computed content ID, shadow-write should succeed anyway.
        let tmp = tempfile::tempdir().expect("tempdir");
        let content = b"fn gone() {}";
        let file_path = tmp.path().join("gone.rs");
        std::fs::write(&file_path, content).expect("write");

        let mut table = SymbolTable::default();
        let (name_id, node_kind_id, fql_kind_id, language_id, path_id) =
            table
                .strings
                .intern_row("gone", "function_item", "function", "rust", &file_path);
        table.push_row(IndexRow {
            byte_range: 0..content.len(),
            line: 1,
            usages_count: 0,
            ordinal: None,
            parent_ordinal: u32::MAX,
            rev: 0,
            fields: HashMap::new(),
            name_id,
            node_kind_id,
            fql_kind_id,
            language_id,
            path_id,
        });

        // Delete the source file — the writer must use the pre-computed ID.
        std::fs::remove_file(&file_path).expect("remove");

        let mut pre_computed = HashMap::new();
        pre_computed.insert(file_path.clone(), identity_hash(content));

        let segments_base = tmp.path().join("segments");
        let writer =
            ShadowWriter::new(&table, &segments_base, "test", &identity_hash, pre_computed);
        let result = writer.run().expect("run without re-reading file");
        assert_eq!(
            result.count, 1,
            "segment written via pre-computed content ID"
        );
        assert_eq!(result.segment_map.len(), 1, "segment_map has one entry");
    }

    #[test]
    fn manifest_written_after_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let forgeql_dir = tmp.path().join("forgeql");
        let segments_base = forgeql_dir.join("segments");

        let mut enrichment = HashMap::new();
        enrichment.insert("param_count".to_owned(), "2".to_owned());
        let table = make_table(
            tmp.path(),
            "main.rs",
            b"fn main() {}",
            "main",
            "function",
            enrichment,
        );

        let writer = ShadowWriter::new(
            &table,
            &segments_base,
            "test",
            &identity_hash,
            HashMap::new(),
        );
        writer.run().expect("run");

        let manifest_path = forgeql_dir.join(format!(
            "manifest-test-v{}.json",
            crate::storage::columnar::ENRICH_VER
        ));
        assert!(manifest_path.exists(), "versioned manifest written");

        let manifest: crate::storage::columnar::manifest::Manifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read"))
                .expect("parse manifest");
        assert_eq!(manifest.provider_id, "test");
        assert_eq!(manifest.segment_count, 1);
        assert!(
            manifest.column_registry.contains("param_count"),
            "enrichment column in registry"
        );
    }
}
