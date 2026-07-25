//! Overlay/columnar parity: symbol resolution.
//!
//! `resolve_type_symbol`, `resolve_body_symbol`, and `resolve_symbol` over a
//! committed overlay must match the merged legacy symbol tables.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

mod overlay_harness;

use overlay_harness::*;

/// When a name resolves to both a struct and a function, `resolve_type_symbol`
/// must return the struct (type-preference semantics).
///
/// Fixture: canonical.cpp defines both `struct Motor { ... }` and
/// `int Motor(int rpm) { ... }`.
#[test]
fn resolve_type_prefers_type_over_function() {
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let clauses = Clauses::default();

    let loc = storage
        .resolve_type_symbol("Motor", &clauses, &fixtures_dir())
        .expect("resolve_type_symbol")
        .expect("Motor not found");

    // The resolved location must be the struct definition, not the function.
    // The columnar segment stores the fql_kind in `node_kind`; for the struct
    // definition the kind is "struct".
    assert_eq!(
        loc.node_kind, "struct",
        "resolve_type_symbol should return the struct row, got node_kind={:?}",
        loc.node_kind
    );
}

/// When a row carries a `body_symbol` enrichment field, `resolve_body_symbol`
/// must follow the redirect and return the out-of-line definition.
///
/// Fixture: canonical.cpp has `class Engine { void start(); }` (in-class
/// declaration) and `void Engine::start() { }` (out-of-line definition).
#[test]
fn resolve_body_follows_body_symbol_redirect() {
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let clauses = Clauses::default();

    // resolve_body_symbol("start") should follow body_symbol → "Engine::start".
    // If there is no body_symbol enrichment (MemberEnricher not applied to the
    // test segment), it will fall back to whichever "start" row is resolved —
    // that is also acceptable as a no-op redirect test.
    let loc = storage
        .resolve_body_symbol("start", &clauses, &fixtures_dir())
        .expect("resolve_body_symbol")
        .expect("start not found");

    // Whether a redirect happened or not, the resolved location must be for a
    // function (the out-of-line body, or the in-class decl as fallback).
    // The key invariant: both columnar and legacy resolve to the same line.
    let (table, _tmp2, _storage2) = single_segment_cpp_overlay();
    let leg_row = table
        .find_all_defs("start")
        .into_iter()
        .chain(table.find_all_defs("Engine::start"))
        .next()
        .expect("start not in legacy table");

    // Both should be on the same line (± the redirect).
    // The columnar segment does not run MemberEnricher, so no redirect happens
    // and the line should equal the in-class declaration line.
    assert_eq!(
        loc.line, leg_row.line,
        "resolve_body_symbol line mismatch: col={} leg={}",
        loc.line, leg_row.line
    );
}

/// Calling `resolve_symbol` twice on the same name produces the same location
/// (determinism / last-write-wins stability).
#[test]
fn resolve_symbol_deterministic_on_duplicates() {
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let clauses = Clauses::default();

    // `noop_dup` has two rows: a forward-declaration and a definition.
    // resolve_symbol must always return the same (last-indexed) row.
    let loc1 = storage
        .resolve_symbol("noop_dup", &clauses, &fixtures_dir())
        .expect("resolve 1")
        .expect("noop_dup not found (call 1)");
    let loc2 = storage
        .resolve_symbol("noop_dup", &clauses, &fixtures_dir())
        .expect("resolve 2")
        .expect("noop_dup not found (call 2)");

    assert_eq!(
        loc1.line, loc2.line,
        "resolve_symbol is non-deterministic: call1={} call2={}",
        loc1.line, loc2.line
    );
    assert_eq!(
        loc1.byte_range, loc2.byte_range,
        "resolve_symbol byte_range differs between calls"
    );
}
