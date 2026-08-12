//! `FORGEQL_FIND_MAX_ROWS` and `FORGEQL_FIND_MAX_ROW_IDS` — the two hard
//! budgets a `FIND` is held to: the rows it materialises, and the candidate row
//! IDs it holds to materialise them from. They are separate numbers because
//! they are separate costs — about 1,600 bytes against four — and a query can
//! be inside one and past the other.
//!
//! Both are read from the process environment, and the workspace denies
//! `unsafe` (so no `std::env::set_var`).  Each driver test therefore re-invokes
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
use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
use forgeql_core::ir::Clauses;
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{
    ColumnarStorage, OverlayBuilder, SegmentBuilder, SegmentReader, SymbolRow,
};
use forgeql_lang_cpp::CppLanguage;
use tempfile::TempDir;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

fn vp() -> String {
    format!("test-v{}", forgeql_core::storage::columnar::ENRICH_VER)
}
/// Keyed by (path, content) via the engine's own helper — `source_path` is the
/// worktree-relative path the overlay stores.
fn seg_path(
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

/// Index `canonical.cpp` and build a single-segment `ColumnarStorage`
/// around it — a miniature of the `overlay_parity` harness.
fn single_segment_cpp_storage() -> (TempDir, ColumnarStorage) {
    let lang = CppLanguage;
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
            workspace_root: None,
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
        .flush(&seg_path(
            &segments_dir,
            std::path::Path::new("canonical.cpp"),
            &hex,
        ))
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

/// Probe run inside the child process for the row-ID budget: asserts the
/// behaviour matching the `FORGEQL_FIND_MAX_ROW_IDS` value inherited from the
/// driver's `Command::env`.
#[test]
#[ignore = "driver-invoked probe; behaviour depends on FORGEQL_FIND_MAX_ROW_IDS"]
fn row_id_budget_probe() {
    let (_tmp, storage) = single_segment_cpp_storage();
    let clauses = Clauses::default();
    let result = storage.find_symbols(&clauses, std::path::Path::new("."));

    match std::env::var("FORGEQL_FIND_MAX_ROW_IDS").as_deref() {
        Ok("1") => {
            let err = result.expect_err("a row-ID cap of 1 must refuse a whole-index scan");
            assert!(
                err.to_string().contains("FORGEQL_FIND_MAX_ROW_IDS"),
                "error should name the knob: {err}"
            );
            assert!(
                !err.to_string().contains("FORGEQL_FIND_MAX_ROWS "),
                "the row-ID refusal must not be the row refusal: {err}"
            );
        }
        Ok("0") | Err(_) => {
            let rows = result.expect("scan must pass without an effective cap");
            assert!(rows.len() > 1, "fixture should materialise multiple rows");
        }
        Ok(other) => panic!("unexpected probe configuration: {other}"),
    }
}

/// The candidate row IDs a scan holds are bounded separately from the rows it
/// builds from them, because they cost four bytes against about 1,600. The two
/// knobs are independent: this drives the row-ID one while the row one stays at
/// its ample default, so a refusal here can only have come from the row-ID
/// bound.
#[test]
fn row_id_budget_refuses_oversized_candidate_sets_and_zero_disables() {
    let exe = std::env::current_exe().expect("current_exe");
    let run = |cap: Option<&str>| {
        let mut cmd = std::process::Command::new(&exe);
        let _ = cmd.args(["--exact", "row_id_budget_probe", "--ignored"]);
        let _ = cmd.env_remove("FORGEQL_FIND_MAX_ROWS");
        match cap {
            Some(v) => {
                let _ = cmd.env("FORGEQL_FIND_MAX_ROW_IDS", v);
            }
            None => {
                let _ = cmd.env_remove("FORGEQL_FIND_MAX_ROW_IDS");
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

/// Cap the dirty-union driver passes to its probe: above the committed
/// fixture's row count (the driver guards that), below committed + the 512
/// dirty rows the probe stages — so a refusal can only come from the check
/// over the union.
const DIRTY_UNION_CAP: &str = "400";

#[test]
#[ignore = "driver-invoked probe; behaviour depends on FORGEQL_FIND_MAX_ROWS"]
fn dirty_union_budget_probe() {
    let (tmp, mut storage) = single_segment_cpp_storage();

    // 512 dirty rows in one staged segment — enough to cross the cap wherever
    // the committed side lands below it.
    let names: Vec<String> = (0..512).map(|i| format!("dirty_fn_{i}")).collect();
    let mut builder = SegmentBuilder::new("test", &[0x44u8; 8]);
    for (i, name) in names.iter().enumerate() {
        let _ = builder.emit_row(SymbolRow {
            name,
            fql_kind: "function",
            language: "rust",
            line: u32::try_from(i + 1).unwrap_or(u32::MAX),
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
    }
    let dirty_dir = tmp.path().join("staging").join("dirty_extra");
    builder.flush(&dirty_dir).expect("dirty segment flush");
    let reader = SegmentReader::open(&dirty_dir).expect("dirty SegmentReader::open");
    storage
        .dirty_mut()
        .add_segment(Arc::new(reader), PathBuf::from("extra.rs"), String::new());

    let clauses = Clauses::default();
    let result = storage.find_symbols(&clauses, std::path::Path::new("."));

    match std::env::var("FORGEQL_FIND_MAX_ROWS").as_deref() {
        Ok(v) if v == DIRTY_UNION_CAP => {
            let err = result.expect_err("the union must count against the row budget");
            assert!(
                err.to_string().contains("FORGEQL_FIND_MAX_ROWS"),
                "error should name the knob: {err}"
            );
        }
        Ok("0") | Err(_) => {
            let rows = result.expect("scan must pass without an effective cap");
            assert!(
                rows.iter().any(|r| r.name == "dirty_fn_0"),
                "dirty rows must be part of the answer"
            );
        }
        Ok(other) => panic!("unexpected probe configuration: {other}"),
    }
}

#[test]
fn dirty_union_rows_count_against_the_row_budget() {
    // Guard: the cap must sit between the committed half and the union, or
    // the probe measures the wrong check.
    let (_tmp, storage) = single_segment_cpp_storage();
    let committed = storage
        .find_symbols(&Clauses::default(), std::path::Path::new("."))
        .expect("uncapped committed scan")
        .len();
    let cap: usize = DIRTY_UNION_CAP.parse().expect("cap parses");
    assert!(
        committed <= cap,
        "fixture outgrew the probe's cap: {committed} > {cap}"
    );
    assert!(committed + 512 > cap, "dirty rows no longer cross the cap");

    let exe = std::env::current_exe().expect("current_exe");
    let run = |cap: Option<&str>| {
        let mut cmd = std::process::Command::new(&exe);
        let _ = cmd.args(["--exact", "dirty_union_budget_probe", "--ignored"]);
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
    run(Some(DIRTY_UNION_CAP));
    run(Some("0"));
    run(None);
}

#[test]
#[ignore = "driver-invoked probe; behaviour depends on FORGEQL_FIND_MAX_ROWS"]
fn usages_budget_probe() {
    let (_tmp, storage) = single_segment_cpp_storage();
    let clauses = Clauses::default();
    let result = storage.find_usages("int", &clauses, &fixtures_dir());

    match std::env::var("FORGEQL_FIND_MAX_ROWS").as_deref() {
        Ok("1") => {
            let err = result.expect_err("a cap of 1 must refuse a multi-site read");
            assert!(
                err.to_string().contains("FORGEQL_FIND_MAX_ROWS"),
                "error should name the knob: {err}"
            );
        }
        Ok("0") | Err(_) => {
            let (rows, _hint) = result.expect("usages must answer without an effective cap");
            assert!(
                rows.len() > 1,
                "fixture should hold several sites for 'int'; got {}",
                rows.len()
            );
        }
        Ok(other) => panic!("unexpected probe configuration: {other}"),
    }
}

#[test]
fn usages_sites_count_against_the_row_budget() {
    let exe = std::env::current_exe().expect("current_exe");
    let run = |cap: Option<&str>| {
        let mut cmd = std::process::Command::new(&exe);
        let _ = cmd.args(["--exact", "usages_budget_probe", "--ignored"]);
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
