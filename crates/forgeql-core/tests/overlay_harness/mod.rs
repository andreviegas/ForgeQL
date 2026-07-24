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
    unreachable_pub
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
