//! Unit tests for the shadow segment writer: that an empty table writes no
//! segments, that one segment is written per file, that enrichment fields land
//! in the extra columns, that pre-computed content skips the file read, and
//! that the manifest is written once the run completes.

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
        tmp.path(),
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
        tmp.path(),
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
        tmp.path(),
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
    let writer = ShadowWriter::new(
        &table,
        &segments_base,
        "test",
        &identity_hash,
        pre_computed,
        tmp.path(),
    );
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
        tmp.path(),
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
