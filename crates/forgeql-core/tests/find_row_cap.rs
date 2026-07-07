//! `FORGEQL_FIND_MAX_ROWS` — the hard row budget for FIND materialisation.
//!
//! The budget is read from the process environment, and the workspace denies
//! `unsafe` (so no `std::env::set_var`).  The driver test therefore re-invokes
//! this very test binary as a child process with the variable set; the
//! `#[ignore]`d probe test runs inside the child and asserts the behaviour
//! that matches the inherited environment.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{IndexContext, SymbolTable, index_file};
use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry, LanguageSupport};
use forgeql_core::ir::Clauses;
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{
    ColumnarStorage, OverlayBuilder, SegmentBuilder, SegmentReader, SymbolRow,
};
use tempfile::TempDir;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

fn vp() -> String {
    format!("test-v{}", forgeql_core::storage::columnar::ENRICH_VER)
}

fn seg_path(segments_base: &std::path::Path, hex: &str) -> std::path::PathBuf {
    segments_base
        .join(vp())
        .join(&hex[..2])
        .join(format!("{}.fqsf", &hex[2..]))
}

/// Index `canonical.cpp` and build a single-segment `ColumnarStorage`
/// around it — a miniature of the `overlay_parity` harness.
fn single_segment_cpp_storage() -> (TempDir, ColumnarStorage) {
    let lang = CppLanguageInline;
    let src = fixtures_dir().join("canonical.cpp");
    assert!(src.exists(), "fixture missing: {}", src.display());

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &src,
            language: &lang,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
        };
        let _ = index_file(&mut parser, &mut ctx, None).expect("index_file");
    }

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");

    // Deterministic content ID based on the source path hash (test only).
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    src.hash(&mut h);
    let content_id: Vec<u8> = h.finish().to_le_bytes().to_vec();
    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

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
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder
        .flush(&seg_path(&segments_dir, &hex))
        .expect("segment flush");

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(src, content_id);
    let overlay_path = tmp.path().join("overlays").join("test").join("cap.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_path(&segments_dir, &m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
    (tmp, storage)
}

/// Probe run inside the child process: asserts the behaviour matching the
/// `FORGEQL_FIND_MAX_ROWS` value inherited from the driver's `Command::env`.
#[test]
#[ignore = "driver-invoked probe; behaviour depends on FORGEQL_FIND_MAX_ROWS"]
fn row_budget_probe() {
    let (_tmp, storage) = single_segment_cpp_storage();
    let clauses = Clauses::default();
    let result = storage.find_symbols(&clauses, std::path::Path::new("."));

    match std::env::var("FORGEQL_FIND_MAX_ROWS").as_deref() {
        Ok("1") => {
            let err = result.expect_err("a cap of 1 must refuse a whole-index scan");
            assert!(
                err.to_string().contains("FORGEQL_FIND_MAX_ROWS"),
                "error should name the knob: {err}"
            );
        }
        Ok("0") | Err(_) => {
            let rows = result.expect("scan must pass without an effective cap");
            assert!(rows.len() > 1, "fixture should materialise multiple rows");
        }
        Ok(other) => panic!("unexpected probe configuration: {other}"),
    }
}

/// Run the probe in a child process for each knob state: `1` refuses an
/// unscoped scan with guidance, `0` disables the bound, unset uses the
/// (ample) default.
#[test]
fn row_budget_refuses_oversized_scans_and_zero_disables() {
    let exe = std::env::current_exe().expect("current_exe");
    let run = |cap: Option<&str>| {
        let mut cmd = std::process::Command::new(&exe);
        let _ = cmd.args(["--exact", "row_budget_probe", "--ignored"]);
        match cap {
            Some(v) => {
                let _ = cmd.env("FORGEQL_FIND_MAX_ROWS", v);
            }
            None => {
                let _ = cmd.env_remove("FORGEQL_FIND_MAX_ROWS");
            }
        }
        let out = cmd.output().expect("spawn probe");
        assert!(
            out.status.success(),
            "probe with cap {cap:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(Some("1"));
    run(Some("0"));
    run(None);
}
