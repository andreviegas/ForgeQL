//! An `Eq` predicate on a posted enrichment field whose VALUE exists nowhere
//! must answer empty without scanning the corpus — and must never answer
//! empty when the overlay is not in a position to prove the value absent.
//!
//! Asking for a value that does not exist used to cost a full-corpus scan:
//! the overlay held no bitmap for `field=value`, the prefilter reported "no
//! opinion", and every row in the index was materialised so the residual
//! filter could reject it one at a time. On a 3M-symbol corpus
//! `guard_kind = 'ifdef'` took 7.3 s to return nothing at all — `guard_kind`
//! is only ever `preprocessor`, `attribute` or `heuristic`.
//!
//! The speed-up asserts something, though: an empty candidate bitmap says
//! "no such row exists". These tests pin the cases where that claim is NOT
//! the overlay's to make, because a wrong claim there is a silent false
//! negative rather than a slow query.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_results
)]

use std::fmt::Write as _;
use std::fs;

use forgeql_core::engine::ForgeQLEngine;
use tempfile::tempdir;

mod common;
mod enrichment_harness;
use enrichment_harness::*;

/// Two files with short guard chains: every segment stays inside the
/// per-segment cardinality cap, so `guard_branch` is fully posted.
fn engine_fully_posted() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");

    fs::write(
        dir.path().join("a_short.cpp"),
        r"
#if defined(OPT_A)
int short_a;
#else
int short_b;
#endif
",
    )
    .expect("write a_short.cpp");

    fs::write(
        dir.path().join("b_plain.cpp"),
        r"
int plain_one(int x) { return x + 1; }
int plain_two(int x) { return x + 2; }
",
    )
    .expect("write b_plain.cpp");

    let registry = common::make_registry();
    let mut engine = ForgeQLEngine::new(dir.path().join("data"), registry).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");
    (engine, session_id, dir)
}

#[test]
fn absent_value_on_posted_field_answers_empty() {
    let (mut e, sid, _d) = engine_fully_posted();

    // Control: a value that DOES exist still returns its rows, so the empty
    // answer below is about the value, not about the field being unserved.
    let present = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE guard_kind = 'preprocessor'",
    );
    assert!(
        !common::as_query(&present).results.is_empty(),
        "guard_kind = 'preprocessor' must return the guarded rows"
    );

    // `ifdef` is not a guard_kind — the enricher only ever writes
    // preprocessor / attribute / heuristic.
    let absent = exec(&mut e, &sid, "FIND symbols WHERE guard_kind = 'ifdef'");
    assert!(
        common::as_query(&absent).results.is_empty(),
        "a value the corpus never stores must answer empty"
    );
}

#[test]
fn absent_value_does_not_hide_uncommitted_rows() {
    // The empty candidate bitmap covers PERSISTENT rows only. Dirty rows are
    // materialised in their own stage, downstream of the candidate set, so a
    // value introduced by an uncommitted edit must still be found.
    let (mut e, sid, _d) = engine_fully_posted();

    // Before the edit the value is genuinely absent.
    let before = exec(&mut e, &sid, "FIND symbols WHERE naming = 'UPPER_SNAKE'");
    assert!(
        common::as_query(&before).results.is_empty(),
        "no UPPER_SNAKE symbol exists yet"
    );

    // Introduce one with an in-session mutation, leaving it uncommitted.
    // Appending at a file handle needs no rev — creation cannot clobber.
    let file = common::path_handle("b_plain.cpp");
    exec(
        &mut e,
        &sid,
        &format!("INSERT AFTER NODE '{file}' WITH 'int NEWLY_ADDED_FLAG = 7;'"),
    );

    let after = exec(&mut e, &sid, "FIND symbols WHERE naming = 'UPPER_SNAKE'");
    let found = names(&common::as_query(&after).results);
    assert!(
        found.contains(&"NEWLY_ADDED_FLAG"),
        "an uncommitted row carrying a value absent from the persistent \
         overlay must still be found; got {found:?}"
    );
}

#[test]
fn unposted_segment_keeps_the_complete_scan() {
    // A segment whose per-segment cardinality for a field exceeds the cap
    // writes NO postings blob for it, so the overlay never sees that
    // segment's values. The overlay's key set is then incomplete and
    // "no key" no longer proves "no row" — the query must fall back to the
    // complete scan rather than answering empty.
    //
    // A ten-arm preprocessor chain gives `guard_branch` ten distinct values
    // in one file, over the cap of eight.
    let dir = tempdir().expect("tempdir");
    let mut many = String::from("\n#if defined(ARM_0)\nint arm_0;\n");
    for i in 1..10 {
        let _ = write!(many, "#elif defined(ARM_{i})\nint arm_{i};\n");
    }
    many.push_str("#endif\n");
    fs::write(dir.path().join("a_many_arms.cpp"), &many).expect("write a_many_arms.cpp");

    // A second file well inside the cap, so `guard_branch` is posted here and
    // the field exists in the overlay with a PARTIAL key set.
    fs::write(
        dir.path().join("b_two_arms.cpp"),
        r"
#if defined(OPT_A)
int two_a;
#else
int two_b;
#endif
",
    )
    .expect("write b_two_arms.cpp");

    let registry = common::make_registry();
    let mut e = ForgeQLEngine::new(dir.path().join("data"), registry).expect("engine");
    let sid = e
        .register_local_session(dir.path())
        .expect("register session");

    // Ground truth, computed without the enrichment predicate: read
    // guard_branch off every arm_* row and count each distinct value.
    let all = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name LIKE 'arm_%' LIMIT 100",
    );
    let all_q = common::as_query(&all);
    assert!(
        !all_q.results.is_empty(),
        "the ten-arm fixture must produce arm_* rows"
    );

    for m in &all_q.results {
        let branch = field(m, "guard_branch").to_owned();
        let filtered = exec(
            &mut e,
            &sid,
            &format!("FIND symbols WHERE guard_branch = '{branch}' LIMIT 100"),
        );
        let got = names(&common::as_query(&filtered).results);
        assert!(
            got.contains(&m.name.as_str()),
            "guard_branch = '{branch}' dropped '{}' — the overlay cannot \
             prove a value absent for a field it has not fully posted; got {got:?}",
            m.name
        );
    }
}

#[test]
fn value_carried_only_by_a_shadowed_row_is_still_found() {
    // The workspace overlay keys a value only from rows that survive a
    // per-segment `(name, fql_kind, line)` dedup. Put two variables with the
    // same name and kind on ONE line and the second loses that tie, so any
    // enrichment value carried only by it is keyed nowhere — while the row
    // itself exists and the scan returns it.
    //
    // This is why absence is decided from the per-segment postings, which are
    // keyed by raw row id, and not from the overlay's key set.
    let dir = tempdir().expect("tempdir");

    // `dup` twice on one line: file scope first (it wins the dedup), local
    // scope second (it loses). `scope = 'local'` is therefore carried only by
    // a shadowed row.
    fs::write(
        dir.path().join("a_shadowed.cpp"),
        "int dup; void g() { int dup; }\n",
    )
    .expect("write a_shadowed.cpp");

    // Gives `scope` a key of its own, so the field is present in the overlay
    // and only the queried VALUE is missing — the exact shape that used to be
    // reported as a confident zero.
    fs::write(dir.path().join("b_other.cpp"), "int other;\n").expect("write b_other.cpp");

    let registry = common::make_registry();
    let mut e = ForgeQLEngine::new(dir.path().join("data"), registry).expect("engine");
    let sid = e
        .register_local_session(dir.path())
        .expect("register session");

    // Which twin wins the dedup is the builder's business, so assert BOTH
    // values: whichever one is carried solely by the shadowed row is the one
    // the overlay cannot key, and that query is the one that used to answer a
    // confident zero.
    for scope in ["file", "local"] {
        let r = exec(
            &mut e,
            &sid,
            &format!("FIND symbols WHERE scope = '{scope}' LIMIT 50"),
        );
        let got = names(&common::as_query(&r).results);
        assert!(
            got.contains(&"dup"),
            "scope = '{scope}' dropped 'dup' — a value carried only by a row \
             that lost the per-segment dedup must still be found, because the \
             overlay's key set cannot prove it absent; got {got:?}"
        );
    }
}

#[test]
fn core_row_fields_are_untouched_by_the_absence_shortcut() {
    // `prefilter_global`'s enrichment arm matches ANY field name except
    // `fql_kind` and `name`, so core row metadata lands there too. The
    // enrichment index stores no column for those fields — which must NOT be
    // read as "no row carries this value", or every `WHERE language = '…'`
    // query answers zero.
    let (mut e, sid, _d) = engine_fully_posted();

    // Take the value off a row the engine itself produced, so the test pins
    // scan-vs-index agreement rather than a guess at how a language name is
    // spelled.
    let seed = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'plain_one' LIMIT 1",
    );
    let seed_q = common::as_query(&seed);
    let row = seed_q.results.first().expect("plain_one must be indexed");
    let language = row.language.clone().expect("language on a symbol row");

    // `language` / `lang` are the pair that actually regressed when the
    // shortcut was not scoped: both returned zero rows corpus-wide.
    for (field, value) in [("language", language.as_str()), ("lang", language.as_str())] {
        let r = exec(
            &mut e,
            &sid,
            &format!("FIND symbols WHERE {field} = '{value}' LIMIT 50"),
        );
        let got = names(&common::as_query(&r).results);
        assert!(
            got.contains(&"plain_one"),
            "{field} = '{value}' dropped 'plain_one' — a core row field is \
             served outside the enrichment index, so that index having nothing \
             to say about it proves nothing about which rows exist; got {got:?}"
        );
    }

    // The counterpart: a core field really can name a value that matches no
    // row, and that must still answer empty rather than erroring.
    let none = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE language = 'cobol' LIMIT 20",
    );
    assert!(
        common::as_query(&none).results.is_empty(),
        "no cobol symbol exists in this workspace"
    );
}

#[test]
fn posted_enrichment_fields_never_collide_with_core_field_names() {
    // The absence shortcut is safe only because the two field universes are
    // disjoint: a core row field is served outside the enrichment index, so
    // that index's silence about it proves nothing. The query path refuses
    // core names outright, but this pins the invariant at its source — the day
    // an enricher writes a field named `language` or `path`, that refusal is
    // the only thing standing between it and a corpus-wide zero.
    use forgeql_core::filter::CORE_WHERE_FIELDS;
    use forgeql_core::storage::columnar::segment_builder::POSTING_ENRICHMENT_FIELDS;

    let collisions: Vec<&str> = POSTING_ENRICHMENT_FIELDS
        .iter()
        .copied()
        .filter(|f| CORE_WHERE_FIELDS.contains(f))
        .collect();

    assert!(
        collisions.is_empty(),
        "these posted enrichment fields share a name with a core row field, so \
         the enrichment index would be asked to prove absence for a field it \
         does not serve: {collisions:?}"
    );
}
