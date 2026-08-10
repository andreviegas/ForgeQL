//! Shared test harness for the overlay / columnar parity suites.
//!
//! Indexes canonical fixtures, builds segments, and flattens results to
//! comparable key tuples. Each parity suite includes this module and pulls in
//! its helpers plus the re-exported types via `use overlay_harness::*;`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    unreachable_pub,
    unused_imports,
    dead_code
)]

pub use std::collections::HashMap;
pub use std::path::PathBuf;
pub use std::sync::Arc;

pub use forgeql_core::ast::enrich::default_enrichers;
pub use forgeql_core::ast::index::{IndexContext, SymbolTable, index_file};
pub use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
pub use forgeql_core::ir::Clauses;
pub use forgeql_core::result::SymbolMatch;
pub use forgeql_core::storage::columnar::{
    OverlayBuilder, SegmentBuilder, SegmentReader, SymbolRow,
};
pub use forgeql_lang_cpp::CppLanguage;
pub use forgeql_lang_rust::RustLanguage;
pub use tempfile::TempDir;
// ── fixtures ─────────────────────────────────────────────────────────────────

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

pub fn fixture_path(filename: &str) -> PathBuf {
    let p = fixtures_dir().join(filename);
    assert!(p.exists(), "fixture missing: {}", p.display());
    p
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Index an arbitrary file by absolute path and return the `SymbolTable`.
pub fn index_at_path(lang: &dyn LanguageSupport, path: &std::path::Path) -> SymbolTable {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path,
            language: lang,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
            workspace_root: None,
        };
        let _ = index_file(&mut parser, &mut ctx, None).expect("index_file should succeed");
    }
    table
}
/// Index a fixture file with the given language and return the `SymbolTable`.
pub fn index_fixture(lang: &dyn LanguageSupport, filename: &str) -> SymbolTable {
    let path = fixture_path(filename);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &path,
            language: lang,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
            workspace_root: None,
        };
        let count = index_file(&mut parser, &mut ctx, None).expect("index_file should succeed");
        assert!(count > 0, "expected at least one row in {filename}");
    }
    table
}

/// Returns the versioned test provider directory name (e.g. `"test-v3"`).
pub fn vp() -> String {
    format!("test-v{}", forgeql_core::storage::columnar::ENRICH_VER)
}

/// Path to a specific segment file, keyed by (path, content) exactly as the
/// engine keys it — via the engine's own helper, so this can never drift from
/// the rule it asserts.
///
/// `source_path` must be the path the overlay stores: worktree-relative. Every
/// fixture here writes its file directly into the worktree root, so the file
/// name *is* the relative path.
pub fn seg_path(
    segments_base: &std::path::Path,
    source_path: &std::path::Path,
    hex: &str,
) -> std::path::PathBuf {
    segments_base
        .join(vp())
        .join(forgeql_core::storage::columnar::segment_rel_path(
            source_path,
            hex,
        ))
}

/// Build a segment for `table`, store it under `segments_dir/<provider>/<hex>/`,
/// and return `(abs_source_path, content_id_bytes)`.
pub fn build_segment(
    table: &SymbolTable,
    abs_source_path: &std::path::Path,
    segments_dir: &std::path::Path,
) -> Vec<u8> {
    // Deterministic content ID based on source path hash (for test only).
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    abs_source_path.hash(&mut h);
    let hash_u64 = h.finish();
    let content_id: Vec<u8> = hash_u64.to_le_bytes().to_vec();

    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    // The overlay stores paths worktree-relative, and every fixture writes its
    // file into the worktree root, so the file name is that relative path.
    let rel_source_path = std::path::Path::new(
        abs_source_path
            .file_name()
            .expect("source path has a file name"),
    );
    let seg_path = seg_path(segments_dir, rel_source_path, &hex);

    let mut builder = SegmentBuilder::new("test", &content_id);
    for row in &table.rows {
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
        }
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder.flush(&seg_path).expect("segment flush");

    content_id
}

/// Flatten a legacy `SymbolTable` to canonical key tuples.
#[allow(dead_code)]
pub fn legacy_key_tuples(table: &SymbolTable) -> Vec<(String, String, usize)> {
    let mut v: Vec<_> = table
        .rows
        .iter()
        .map(|r| {
            (
                table.name_of(r).to_owned(),
                table.fql_kind_of(r).to_owned(),
                r.line,
            )
        })
        .collect();
    v.sort_unstable();
    v
}

/// Flatten `find_symbols` results to canonical key tuples.
pub fn columnar_key_tuples(results: &[SymbolMatch]) -> Vec<(String, String, usize)> {
    let mut v: Vec<_> = results
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.fql_kind.clone().unwrap_or_default(),
                r.line.unwrap_or(0),
            )
        })
        .collect();
    v.sort_unstable();
    v
}
/// Shared setup used by the name-lookup, LIKE, ORDER BY and enrichment tests.
pub fn single_segment_cpp_overlay() -> (
    SymbolTable,
    TempDir,
    forgeql_core::storage::columnar::ColumnarStorage,
) {
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let table = index_fixture(&CppLanguage, "canonical.cpp");
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let cid = build_segment(&table, &cpp_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid);

    let overlay_path = overlays_dir.join("test").join("cpp_single.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_path(&segments_dir, &m.source_path, &m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new_unshared(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
    (table, tmp, storage)
}
/// Helper: build a `LanguageRegistry` with C++ support and parse `canonical.cpp`
/// into a `ParseCache`, returning the `Arc<CachedParse>`.
pub fn cpp_cached_parse() -> std::sync::Arc<forgeql_core::ast::parse_cache::CachedParse> {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::ParseCache;

    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
    let mut cache = ParseCache::with_capacity(1);
    cache
        .get_or_parse(&fixture_path("canonical.cpp"), &registry)
        .expect("parse canonical.cpp")
}
/// Build a minimal segment from raw (name, fql_kind, line) tuples.
/// Returns an opened `SegmentReader` stored at `dir`.
pub fn build_dirty_segment(
    rows: &[(&str, &str, u32)],
    content_id_bytes: &[u8],
    dir: &std::path::Path,
) -> SegmentReader {
    let mut builder = SegmentBuilder::new("test", content_id_bytes);
    for &(name, kind, line) in rows {
        let _ = builder.emit_row(SymbolRow {
            name,
            fql_kind: kind,
            language: "rust",
            line,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
    }
    builder.flush(dir).expect("dirty segment flush");
    SegmentReader::open(dir).expect("dirty SegmentReader::open")
}

/// Like [`build_dirty_segment`], but records a usage posting per row
/// (BUG-006 U2: `find_usages` reads usage postings, not definition rows).
pub fn build_dirty_segment_with_usages(
    rows: &[(&str, &str, u32)],
    content_id_bytes: &[u8],
    dir: &std::path::Path,
) -> SegmentReader {
    let mut builder = SegmentBuilder::new("test", content_id_bytes);
    for &(name, kind, line) in rows {
        let _ = builder.emit_row(SymbolRow {
            name,
            fql_kind: kind,
            language: "rust",
            line,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        builder.add_usage(name, line);
    }
    builder.flush(dir).expect("dirty segment flush");
    SegmentReader::open(dir).expect("dirty SegmentReader::open")
}
