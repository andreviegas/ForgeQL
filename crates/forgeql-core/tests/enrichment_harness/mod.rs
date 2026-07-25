//! Shared test prelude for the `enrichment_*` integration suites.
//!
//! Holds the thin `(engine, session_id, TempDir)` adapters over the shared
//! `tests/common` harness that every enrichment suite's bodies use, plus the
//! `*_case!` table macros that generate the uniform assertion families. Each
//! suite file is just `mod common; mod enrichment_harness; use
//! enrichment_harness::*;` — the glob prelude brings the adapters and the
//! `HashSet` re-export into scope with no per-file import curation.
#![allow(
    dead_code,
    unreachable_pub,
    unused_imports,
    unused_macros,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::map_unwrap_or,
    clippy::single_char_pattern,
    clippy::unnecessary_get_then_check,
    clippy::uninlined_format_args,
    clippy::too_long_first_doc_paragraph,
    missing_docs
)]

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, SymbolMatch};
use forgeql_core::session::SessionCoords;

use crate::common;

pub use std::collections::HashSet;
// -----------------------------------------------------------------------
// Helpers — the shared harness lives in `tests/common`; these thin adapters
// keep the `(engine, session_id, TempDir)` tuple idiom this suite's bodies use.
//
// This suite stays on the LEGACY backend: columnar local sessions do not carry
// enrichment columns yet (the inline segment emit skips the enrichment
// post-pass), so every session here is a `common::legacy_session`.
// -----------------------------------------------------------------------

/// Temp workspace with the `motor_control` + `enrichment_patterns` fixtures on
/// the legacy backend. Returns `(engine, session_id, TempDir)`.
pub fn engine_with_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    common::legacy_session(&[
        "motor_control.h",
        "motor_control.cpp",
        "enrichment_patterns.cpp",
    ])
    .into_parts()
}

/// Temp workspace with ONLY `enrichment_patterns.cpp`.
pub fn engine_enrichment_only() -> (ForgeQLEngine, String, tempfile::TempDir) {
    common::legacy_session(&["enrichment_patterns.cpp"]).into_parts()
}

/// Engine that indexes only `enrichment_bug2b.c` — the Bug-2b regression
/// fixture containing `module_param` / `MODULE_PARM_DESC` calls that trigger
/// tree-sitter ERROR-recovery phantom `number_literal` nodes.
pub fn engine_bug2b_only() -> (ForgeQLEngine, String, tempfile::TempDir) {
    common::legacy_session(&["enrichment_bug2b.c"]).into_parts()
}

pub fn exec(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).unwrap_or_else(|e| panic!("parse failed for: {fql}: {e}"));
    let op = ops.first().expect("at least one op");
    let coords = SessionCoords::from_session_id(sid).expect("valid sid");
    engine
        .execute(auth(AuthContext::Tester), Some(&coords), op)
        .result
        .unwrap_or_else(|e| panic!("execute failed for: {fql}: {e}"))
}

/// Find first result matching a given name.
pub fn find_by_name<'a>(results: &'a [SymbolMatch], name: &str) -> &'a SymbolMatch {
    results
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no result with name '{name}'"))
}

/// Collect all names from query results.
pub fn names(results: &[SymbolMatch]) -> Vec<&str> {
    results.iter().map(|r| r.name.as_str()).collect()
}

/// Get a field value from a SymbolMatch, panicking with a clear message if missing.
pub fn field<'a>(m: &'a SymbolMatch, key: &str) -> &'a str {
    m.fields
        .get(key)
        .unwrap_or_else(|| {
            panic!(
                "field '{key}' missing on '{}' (available: {:?})",
                m.name,
                m.fields.keys().collect::<Vec<_>>()
            )
        })
        .as_str()
}

/// Optionally get a field value (returns None if absent).
pub fn field_opt<'a>(m: &'a SymbolMatch, key: &str) -> Option<&'a str> {
    m.fields.get(key).map(String::as_str)
}

// -----------------------------------------------------------------------
// Table macros for the per-enricher coverage families. Each row is one
// #[test] keeping its own name (so `cargo test <name>` selects one case and a
// failure names it). Families with a uniform shape use these; a case with an
// extra check or a negative assertion stays a standalone #[test].
// -----------------------------------------------------------------------

/// `exec` the query, then assert every listed name appears among the results.
/// Some queries carry an explicit `LIMIT` so a target identifier is not crowded
/// past the default limit by comment rows — the query string is kept verbatim.
#[macro_export]
macro_rules! names_contains_case {
    ($($name:ident: $query:literal => [$($expect:literal),+ $(,)?];)*) => {
        $(
            #[test]
            fn $name() {
                let (mut e, sid, _d) = engine_enrichment_only();
                let r = exec(&mut e, &sid, $query);
                let qr = common::as_query(&r);
                let ns: Vec<&str> = names(&qr.results);
                $(
                    assert!(ns.contains(&$expect), "expected {} in {ns:?}", $expect);
                )+
            }
        )*
    };
}

/// `exec` the query, assert it returned rows, then assert every row carries the
/// expected value for the named enrichment field.
#[macro_export]
macro_rules! field_all_case {
    ($($name:ident: $query:literal, field = $key:literal => $val:literal;)*) => {
        $(
            #[test]
            fn $name() {
                let (mut e, sid, _d) = engine_enrichment_only();
                let r = exec(&mut e, &sid, $query);
                let qr = common::as_query(&r);
                assert!(!qr.results.is_empty(), "expected at least one match for `{}`", $query);
                for m in &qr.results {
                    assert_eq!(field(m, $key), $val);
                }
            }
        )*
    };
}

#[macro_export]
macro_rules! field_num_bound_case {
    ($($name:ident: $query:literal, field = $key:literal, |$v:ident| $pred:expr, non_empty = $ne:literal;)*) => {
        $(
            #[test]
            fn $name() {
                let (mut e, sid, _d) = engine_enrichment_only();
                let r = exec(&mut e, &sid, $query);
                let qr = common::as_query(&r);
                if $ne {
                    assert!(!qr.results.is_empty(), "expected at least one match for `{}`", $query);
                }
                for m in &qr.results {
                    let $v: i64 = field(m, $key).parse().unwrap();
                    assert!($pred, "field `{}` bound check failed: got {} for `{}`", $key, $v, m.name);
                }
            }
        )*
    };
}

#[macro_export]
macro_rules! named_field_case {
    ($($name:ident: $query:literal, find = $target:literal, field = $key:literal => $val:literal;)*) => {
        $(
            #[test]
            fn $name() {
                let (mut e, sid, _d) = engine_enrichment_only();
                let r = exec(&mut e, &sid, $query);
                let qr = common::as_query(&r);
                let m = find_by_name(&qr.results, $target);
                assert_eq!(field(m, $key), $val);
            }
        )*
    };
}
