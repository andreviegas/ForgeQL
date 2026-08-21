//! The field/tier table must agree with the code it describes.
//!
//! The table is a parallel declaration: nothing reads it at query time yet.
//! What makes it worth having before the collapse is this file — every claim
//! it makes is checked here against the const lists, against the builder's
//! own budget functions, and against what a query actually returns. A
//! disagreement is a test failure rather than a wrong answer at corpus scale.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::fmt::Write as _;

use forgeql_core::field_tiers::{
    ALL_OPS, CATCH_ALL_FIELD, Exactness, FIELD_TIERS, FieldTier, Gap, OpClass, Serving, Source,
    Then, Tier, canonical, lookup, refused_fields,
};
use forgeql_core::filter::{CORE_WHERE_FIELDS, ClauseTarget};
use forgeql_core::result::{CallGraphEntry, FileEntry, MemberEntry, OutlineEntry, SymbolMatch};
use forgeql_core::storage::columnar::{
    POSTING_ENRICHMENT_FIELDS, ZONEMAP_NUMERIC_FIELDS, overlay_budget, posting_budget,
};

/// Every name the table answers to, canonical or alias.
fn declared_names() -> BTreeSet<&'static str> {
    FIELD_TIERS
        .iter()
        .flat_map(|t| std::iter::once(t.field).chain(t.aliases.iter().copied()))
        .collect()
}

fn rows_except_catch_all() -> impl Iterator<Item = &'static FieldTier> {
    FIELD_TIERS
        .iter()
        .filter(|t| t.field != CATCH_ALL_FIELD.field)
}

fn eq_serving(row: &FieldTier) -> &'static Serving {
    row.serving
        .iter()
        .find(|s| s.ops.contains(&OpClass::Eq))
        .unwrap_or_else(|| panic!("{} declares no serving for `=`", row.field))
}

// ── Structure: a row cannot quietly forget an operator ──────────────────────

#[test]
fn every_row_accounts_for_every_operator_class() {
    // The Q3 defect in one assertion. Five fields joined the posting index and
    // the compensation was written on the `=` and pattern arms; the four
    // numeric arms read the same keys and were missed by two reviews. A row
    // that names six of seven operator classes is exactly that bug, and here
    // it does not compile past this test.
    for row in FIELD_TIERS {
        let mut seen: Vec<OpClass> = Vec::new();
        for serving in row.serving {
            for op in serving.ops {
                assert!(
                    !seen.contains(op),
                    "{}: operator class {op:?} is declared twice",
                    row.field
                );
                seen.push(*op);
            }
        }
        for op in ALL_OPS {
            assert!(
                seen.contains(op),
                "{}: operator class {op:?} has no declared serving — every class \
                 must be accounted for, scan included",
                row.field
            );
        }
    }
}

#[test]
fn a_tier_that_can_stand_aside_names_what_takes_over() {
    for row in FIELD_TIERS {
        for serving in row.serving {
            match serving.tier {
                Tier::Scan | Tier::Refused => assert!(
                    serving.then == Then::Nothing,
                    "{}: {:?} has nothing to fall through to",
                    row.field,
                    serving.tier
                ),
                Tier::StoredColumn
                | Tier::KeyBitmap
                | Tier::ValueUniverse
                | Tier::NumericIndex
                | Tier::ZoneMap => assert!(
                    serving.then != Then::Nothing,
                    "{}: {:?} proposes candidates, so the row must name what \
                     decides them",
                    row.field,
                    serving.tier
                ),
                Tier::NameFst | Tier::NamePrefix | Tier::Trigram | Tier::KindBitmap => {}
            }
        }
    }
}

// ── Agreement with the four const lists ────────────────────────────────────

#[test]
fn every_posted_field_is_declared_with_the_builder_s_own_budgets() {
    for &field in POSTING_ENRICHMENT_FIELDS {
        let row = lookup(field).unwrap_or_else(|| {
            panic!("{field} is posted by the segment builder but the table does not name it")
        });
        assert_eq!(
            eq_serving(row).tier,
            Tier::KeyBitmap,
            "{field} is posted, so `=` on it reads one key"
        );
        let budget = row
            .budget
            .unwrap_or_else(|| panic!("{field} is posted under a budget the table does not state"));
        assert_eq!(
            budget.per_file,
            posting_budget(field),
            "{field}: the table's per-file budget disagrees with posting_budget"
        );
        assert_eq!(
            budget.per_workspace,
            overlay_budget(field),
            "{field}: the table's per-workspace budget disagrees with overlay_budget"
        );
        assert!(
            row.gaps.contains(&Gap::OverBudgetFile),
            "{field} is keyed from segment postings, so a file over its budget \
             writes none and this gap is real"
        );
    }
}

#[test]
fn only_posted_fields_claim_the_per_file_gap() {
    // The other direction, and the one that bites. A field keyed by the
    // complete row walk has no per-file gap: claiming one would send the
    // query path looking for rows to add back, which for a row-walk field is
    // the whole corpus. That is the trap Q2 fell into and had to be gated
    // out; here the two directions are asserted together.
    for row in rows_except_catch_all() {
        let posted = POSTING_ENRICHMENT_FIELDS.contains(&row.field);
        assert_eq!(
            row.gaps.contains(&Gap::OverBudgetFile),
            posted,
            "{}: the per-file posting gap and POSTING_ENRICHMENT_FIELDS disagree",
            row.field
        );
    }
    assert!(
        !CATCH_ALL_FIELD.gaps.contains(&Gap::OverBudgetFile),
        "the catch-all stands for row-walk-keyed fields, which have no per-file gap"
    );
}

#[test]
fn every_zone_mapped_column_is_declared() {
    let declared: BTreeSet<&str> = FIELD_TIERS.iter().filter_map(|t| t.column).collect();
    for &(col, _) in ZONEMAP_NUMERIC_FIELDS {
        assert!(
            declared.contains(col),
            "{col} carries a zone map but no table row names it as its column — \
             note the zone-map list is keyed by COLUMN, and `usages` is stored \
             as `usages_count`"
        );
    }
}

#[test]
fn every_validated_field_is_declared() {
    let declared = declared_names();
    for &field in CORE_WHERE_FIELDS {
        assert!(
            declared.contains(field),
            "{field} passes WHERE validation but the table does not say what \
             serves it — that gap is where every defect in this campaign lived"
        );
    }
    for field in symbol_row_fields() {
        assert!(
            declared.contains(field),
            "{field} resolves on a symbol row but the table does not name it"
        );
    }
}

/// One `Shape` per `ClauseTarget` implementation: what that row answers to.
struct Shape {
    row: &'static str,
    str_fields: &'static [&'static str],
    num_fields: &'static [&'static str],
}

impl Shape {
    const fn of<T: ClauseTarget>() -> Self {
        Self {
            row: T::ROW,
            str_fields: T::STR_FIELDS,
            num_fields: T::NUM_FIELDS,
        }
    }

    fn resolves(&self, field: &str) -> bool {
        self.str_fields.contains(&field) || self.num_fields.contains(&field)
    }
}

/// Every `ClauseTarget` implementation a query result can be, both backends'
/// symbol rows included.
///
/// Written out rather than derived, because it cannot be derived: nothing
/// enumerates a trait's implementors. So this is the set a reader has to check
/// against `filter/impls.rs` by hand, and the one place a missing result shape
/// would go unnoticed.
///
/// Three implementations there are deliberately absent. `CommitRow` is a git
/// listing, not a queryable row shape. `SegRowRef` is a segment row viewed in
/// place, used only to test a residual `WHERE` before the row is built, and
/// `RowView` is the same row carried through the collapse, the ordering and the
/// page cut: neither is ever validated against, and both are crate-private, so
/// a test outside the crate cannot name them. What matters about them is not
/// which names they declare but that they answer them exactly as the
/// `SymbolMatch` they stand in for — pinned by
/// `a_row_view_answers_a_prefilterable_predicate_as_the_built_row_does` in
/// `storage::columnar::segment_reader::tests` and by
/// `a_view_reads_every_field_as_the_row_it_builds` in
/// `storage::columnar::columnar_storage::fast_paths::tests`.
fn row_shapes() -> Vec<Shape> {
    vec![
        Shape::of::<SymbolMatch>(),
        Shape::of::<forgeql_core::ast::index::RowRef<'_>>(),
        Shape::of::<FileEntry>(),
        Shape::of::<OutlineEntry>(),
        Shape::of::<MemberEntry>(),
        Shape::of::<CallGraphEntry>(),
        Shape::of::<forgeql_core::result::SourceLine>(),
        Shape::of::<forgeql_core::result::DiffFileEntry>(),
    ]
}

/// Every field name a symbol row resolves, both accessors.
fn symbol_row_fields() -> Vec<&'static str> {
    SymbolMatch::STR_FIELDS
        .iter()
        .chain(SymbolMatch::NUM_FIELDS)
        .copied()
        .collect()
}

#[test]
fn every_name_the_union_accepts_is_carried_somewhere_or_refused() {
    // `CORE_WHERE_FIELDS` is a union across result shapes, and a union member
    // that no shape carries and no row refuses is exactly the defect this
    // campaign is about: accepted by validation, resolved by nothing, answered
    // with a confident zero. Being refused is the other honest outcome, and
    // `signature` is the one name in that position — no result shape carries
    // it, and `SHOW signature OF` is what answers instead. The canonical
    // spelling is what is looked up, because that is what reaches a row.
    let shapes = row_shapes();
    for &field in CORE_WHERE_FIELDS {
        let name = canonical(field);
        let carried = shapes.iter().any(|s| s.resolves(name));
        let refused = lookup(name).is_some_and(FieldTier::is_refused);
        assert!(
            carried || refused,
            "'{field}' is accepted by WHERE validation, resolves on no row shape \
             — not {} — and no table row refuses it. Either a shape should carry \
             it, or the table should refuse it, or the union should not list it.",
            shapes.iter().map(|s| s.row).collect::<Vec<_>>().join(", ")
        );
    }
}

#[test]
fn no_row_is_invented() {
    // The reverse of the above: a table row must correspond to something the
    // engine really has, or the table drifts in the other direction and
    // starts documenting fields that do not exist.
    let zone_map_columns: BTreeSet<&str> = ZONEMAP_NUMERIC_FIELDS.iter().map(|&(c, _)| c).collect();
    let symbol_fields = symbol_row_fields();
    for row in rows_except_catch_all() {
        let known = CORE_WHERE_FIELDS.contains(&row.field)
            || symbol_fields.contains(&row.field)
            || POSTING_ENRICHMENT_FIELDS.contains(&row.field)
            || row.column.is_some_and(|c| zone_map_columns.contains(c));
        assert!(
            known,
            "{} is declared here but is in none of the lists the engine reads",
            row.field
        );
    }
}

// ── Exactness: which tiers are allowed to conclude an absence ──────────────

/// The sources a `SupersetProvenAbsent` proof may name, keyed by the module
/// prefix the declaration writes.
///
/// A proof named in the table and absent from the code is a claim with nothing
/// behind it, and checking only that the string is non-empty checked nothing.
const PROOF_SOURCES: &[(&str, &str)] = &[(
    "fast_paths",
    include_str!("../src/storage/columnar/columnar_storage/fast_paths.rs"),
)];

#[test]
fn only_a_tier_that_reads_every_row_may_call_a_value_absent() {
    // The Q1 defect, as a rule — and as a function rather than a permission
    // list. A tier assembled from keys sees only what was keyed, so an empty
    // result from it is "no candidates", never "no rows", unless a per-segment
    // proof has separately established the absence. `Exactness::of` decides
    // which of those a `(tier, then)` pair is; the table may not choose.
    for row in FIELD_TIERS {
        for s in row.serving {
            let required = Exactness::of(s.tier, s.then);
            let legal = s.exactness == required
                || matches!(
                    (required, s.exactness, s.tier),
                    (
                        Exactness::Superset,
                        Exactness::SupersetProvenAbsent(_),
                        Tier::KeyBitmap,
                    )
                );
            assert!(
                legal,
                "{}: {:?} then {:?} declared {:?}, but that pair can only be \
                 {required:?} — a structure built from keys cannot be exact, \
                 and one that reads every row does not need a proof",
                row.field, s.tier, s.then, s.exactness
            );
            if let Exactness::SupersetProvenAbsent(proof) = s.exactness {
                let (module, func) = proof.split_once("::").unwrap_or_else(|| {
                    panic!("{}: proof '{proof}' is not module::function", row.field)
                });
                let src = PROOF_SOURCES
                    .iter()
                    .find(|(m, _)| *m == module)
                    .unwrap_or_else(|| {
                        panic!(
                            "{}: proof '{proof}' names module '{module}', which this \
                             test does not read — add its source to PROOF_SOURCES",
                            row.field
                        )
                    })
                    .1;
                assert!(
                    src.contains(&format!("fn {func}(")),
                    "{}: proof '{proof}' names a function {module} does not define",
                    row.field
                );
            }
        }
    }
}

#[test]
fn the_exactness_of_a_pair_is_decided_by_what_follows_it() {
    // The rule above is only worth having if the two relationships a `Then`
    // can express actually differ. They do, and on the same tier: a name FST
    // that filters is exact, and the same FST with a fallback is not — which
    // is the pairing the old permission matrix allowed either way round.
    assert_eq!(
        Exactness::of(Tier::NameFst, Then::Nothing),
        Exactness::Exact
    );
    assert_eq!(
        Exactness::of(Tier::NameFst, Then::Fallback(Tier::Trigram)),
        Exactness::Superset
    );
    assert_eq!(
        Exactness::of(Tier::KeyBitmap, Then::Filters(Tier::Scan)),
        Exactness::Superset,
        "a filter decides the candidates but cannot make an empty candidate \
         set into an absence"
    );
    assert_eq!(
        Exactness::of(Tier::StoredColumn, Then::Filters(Tier::Scan)),
        Exactness::Exact
    );
}

#[test]
fn a_refused_field_promises_no_serving() {
    for row in FIELD_TIERS {
        if row.serving.iter().any(|s| s.tier == Tier::Refused) {
            assert!(
                row.is_refused(),
                "{}: a field cannot be served for some operators and refused for \
                 others without saying which",
                row.field
            );
            assert!(
                row.gaps.is_empty() && row.budget.is_none(),
                "{}: nothing is indexed, so there is no gap and no budget to state",
                row.field
            );
        }
    }
}

#[test]
fn every_refused_field_says_where_it_is_answered_or_that_nothing_does() {
    // A refusal that only says no leaves the agent to guess, and the guesses
    // are the same names again. `elsewhere` is what makes the message useful,
    // and `None` is a claim in its own right: nothing answers this anywhere.
    for row in refused_fields() {
        let message = row.refusal(row.field, "WHERE", "FIND symbols");
        assert!(
            message.contains(row.field),
            "{}: the refusal does not name the field the agent wrote",
            row.field
        );
        match row.elsewhere {
            Some(elsewhere) => assert!(
                message.contains(elsewhere),
                "{}: the table says it is answered by '{elsewhere}' and the \
                 refusal does not say so",
                row.field
            ),
            None => assert!(
                message.contains("internal storage column"),
                "{}: nothing answers it, and the refusal should say that \
                 rather than point nowhere",
                row.field
            ),
        }
    }
}

#[test]
fn the_written_spelling_survives_into_the_refusal() {
    // An error that renames the field the agent typed is an error about a
    // different query. `ext` and `content` are the two aliases of refused
    // fields, so they are the two that can go wrong.
    for (written, canonical_name) in [("ext", "extension"), ("content", "text")] {
        let row = lookup(written).expect("alias is declared");
        assert_eq!(row.field, canonical_name);
        let message = row.refusal(written, "WHERE", "FIND symbols");
        assert!(
            message.contains(written),
            "the refusal for '{written}' does not mention it: {message}"
        );
    }
}

// ── Gaps: each one names a fallback, and each one is covered ───────────────

/// The test that proves each declared gap's fallback is reached, or the
/// reason no test can reach it.
///
/// Adding a `Gap` variant to any row fails `every_declared_gap_is_covered`
/// until it is answered for here, which is the point: a gap nobody has tried
/// to reach is indistinguishable from one that is not covered at all.
const GAP_COVERAGE: &[(Gap, &str)] = &[
    (
        Gap::OverBudgetFile,
        "a_field_over_its_budgets_still_answers_completely, and \
     query_set_valued_fields::a_file_over_its_field_budget_still_answers_completely \
     for the wide budget class",
    ),
    (
        Gap::OverBudgetWorkspace,
        "a_field_over_its_budgets_still_answers_completely — \
     at fixture scale a file that exceeds the per-file budget for a branch-indexed \
     field also exhausts the workspace one, so the two are proven together",
    ),
    (
        Gap::EmptyValue,
        "NOT CONSTRUCTIBLE: the overlay skips a value that is the empty string \
     when it writes keys, and no fixture reaches an enricher that stores one — \
     a row either carries a non-empty value for a field or does not carry the \
     field at all. Recorded as unreached rather than proven absent. The \
     adjacent, constructible risk — a pattern answering from the keys instead \
     of from the rows — is covered by \
     a_pattern_answers_the_rows_the_filter_would_accept",
    ),
    (
        Gap::DirtySession,
        "query_set_valued_fields::an_uncommitted_row_is_found_by_a_set_field_query \
     and query_core_field_tier::an_uncommitted_row_is_found_by_a_language_predicate",
    ),
    (
        Gap::ShortColumn,
        "NOT CONSTRUCTIBLE from a well-formed segment: the guard \
     compares the stored column's length against the row count, and a segment the \
     builder wrote always agrees. Reaching it needs a doctored segment, which no \
     public API can produce",
    ),
    (
        Gap::AboveI64,
        "NOT CONSTRUCTIBLE: the values are u64 hashes and nothing \
     chooses which one a guard group receives, so no fixture can place a row above \
     i64::MAX on purpose. Recorded as a limitation with no fallback rather than \
     tested",
    ),
];

#[test]
fn every_declared_gap_is_covered() {
    let covered: BTreeSet<Gap> = GAP_COVERAGE.iter().map(|&(g, _)| g).collect();
    let declared: BTreeSet<Gap> = FIELD_TIERS
        .iter()
        .flat_map(|t| t.gaps.iter().copied())
        .chain(CATCH_ALL_FIELD.gaps.iter().copied())
        .collect();
    for gap in &declared {
        assert!(
            covered.contains(gap),
            "{gap:?} is declared by some field but GAP_COVERAGE does not say which \
             test reaches its fallback, or why none can"
        );
    }
    // Every suite a coverage note is allowed to point at. A note naming a
    // test that has been renamed or deleted is worse than no note: it reads
    // like proof and is not, which is the shape this whole file exists to
    // catch.
    let suites = concat!(
        include_str!("field_tier_table.rs"),
        include_str!("query_set_valued_fields.rs"),
        include_str!("query_core_field_tier.rs"),
    );
    for (gap, note) in GAP_COVERAGE {
        assert!(
            !gap.fallback().is_empty(),
            "{gap:?} names no fallback mechanism"
        );
        if note.contains("NOT CONSTRUCTIBLE") {
            continue;
        }
        // A note may qualify a test by suite (`suite::test_name`); the suite
        // is a module path, not a function, so only the last segment names
        // something a `fn` line can be found for.
        let named: Vec<&str> = note
            .split(|c: char| c.is_whitespace() || ",;()`".contains(c))
            .filter_map(|t| t.rsplit("::").next())
            .filter(|t| t.contains('_') && t.len() > 12)
            .collect();
        assert!(
            !named.is_empty(),
            "{gap:?}: the coverage note must name the test that reaches its \
             fallback, or begin NOT CONSTRUCTIBLE with the reason, got: {note}"
        );
        for test_name in named {
            assert!(
                suites.contains(&format!("fn {test_name}(")),
                "{gap:?}: the note names `{test_name}`, which no suite defines \
                 — it was renamed or deleted and the note still reads like proof"
            );
        }
    }
}

// ── Behaviour: the table's claims, asked of a real workspace ───────────────

mod common;

use forgeql_core::result::{ForgeQLResult, QueryResult};

fn query(t: &mut common::TestSession, fql: &str) -> QueryResult {
    match t.exec(fql) {
        ForgeQLResult::Query(q) => q,
        other => panic!("expected Query from `{fql}`, got {other:?}"),
    }
}

/// Result row names, sorted — the comparable shape of an answer.
fn names(q: &QueryResult) -> Vec<String> {
    let mut v: Vec<String> = q.results.iter().map(|r| r.name.clone()).collect();
    v.sort_unstable();
    v
}

/// One file whose conditional chain carries `branches` distinct
/// `guard_branch` values — the cheapest way to put a real field over a real
/// budget, since the value is the branch's position.
fn guarded_workspace(branches: usize) -> common::TestSession {
    build_workspace(branches, false)
}

/// The same, plus a second file whose one function sits inside no conditional
/// at all — so the workspace holds rows that carry `guard_branch` and rows
/// that do not.
fn guarded_workspace_with_unguarded(branches: usize) -> common::TestSession {
    build_workspace(branches, true)
}

fn build_workspace(branches: usize, plus_unguarded: bool) -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut src = String::new();
    for i in 0..branches {
        let directive = if i == 0 { "#if" } else { "#elif" };
        let _ = writeln!(
            src,
            "{directive} defined(FLAG_{i})\nint sym_{i}(void) {{ return {i}; }}"
        );
    }
    src.push_str("#endif\n");
    std::fs::write(dir.path().join("guards.cpp"), src).expect("write fixture");
    if plus_unguarded {
        std::fs::write(
            dir.path().join("plain.cpp"),
            "int unguarded_sym(void) { return 0; }\n",
        )
        .expect("write fixture");
    }
    common::columnar_session_in(dir)
}

/// A workspace holding one type, for the verbs that need a type symbol.
///
/// Its own file rather than an addition to the guarded fixture: the budget and
/// pattern tests count what that fixture contains, and a struct nobody asked
/// for would move their numbers.
fn typed_workspace() -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    // `point_sum` is deliberately several lines long: a one-line body makes
    // "the filter returned fewer lines than the whole body" unfalsifiable.
    std::fs::write(
        dir.path().join("shapes.cpp"),
        "struct Point {\n    int x;\n    int y;\n};\n\
         int point_sum(void) {\n    int total = 0;\n    total = total + 1;\n    return total;\n}\n",
    )
    .expect("write fixture");
    common::columnar_session_in(dir)
}

/// The `guard_branch` value the row named `name` actually carries, read off an
/// unfiltered scan so the test never assumes how branches are numbered.
fn branch_value_of(t: &mut common::TestSession, name: &str) -> String {
    let all = query(t, "FIND symbols WHERE fql_kind = 'function' LIMIT 500");
    all.results
        .iter()
        .find(|r| r.name == name)
        .and_then(|r| r.fields.get("guard_branch").cloned())
        .unwrap_or_else(|| panic!("{name} carries no guard_branch in the fixture"))
}

#[test]
fn a_field_over_its_budgets_still_answers_completely() {
    // Nine distinct branch values in one file, against a per-file budget of
    // eight: the file writes no postings at all, so none of the field's keys
    // name its rows. Every row in the workspace is such a row here, which is
    // what makes the assertion bite — without the fallback the answer is
    // empty, not merely short.
    let mut t = guarded_workspace(9);
    assert_eq!(
        posting_budget("guard_branch"),
        8,
        "fixture sizing assumption"
    );

    let groups = query(
        &mut t,
        "FIND symbols GROUP BY guard_branch ORDER BY count DESC",
    );
    assert!(
        groups.results.len() > posting_budget("guard_branch"),
        "fixture did not exceed the per-file budget: {} distinct values",
        groups.results.len()
    );

    let wanted = branch_value_of(&mut t, "sym_8");
    let q = query(
        &mut t,
        &format!("FIND symbols WHERE guard_branch = '{wanted}' LIMIT 50"),
    );
    assert!(
        q.results.iter().any(|r| r.name == "sym_8"),
        "an over-budget file's rows were dropped: `guard_branch = '{wanted}'` \
         returned {:?}",
        q.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
}

#[test]
fn a_pattern_answers_the_rows_the_filter_would_accept() {
    // Two things at once, because they fail the same way. The fixture is over
    // its per-file budget, so the field has no keys here at all; and one file
    // carries no `guard_branch` on any row. A pattern tier that answered from
    // its keys would return nothing, and one that stopped narrowing
    // altogether would return the unguarded row too. The answer has to be
    // exactly the rows the row-level filter would accept — which is also why
    // the tier must stand aside for a pattern that accepts the empty string:
    // an empty value is keyed nowhere either.
    let mut t = guarded_workspace_with_unguarded(9);

    let scan = query(&mut t, "FIND symbols WHERE fql_kind = 'function' LIMIT 500");
    let mut expected: Vec<&str> = scan
        .results
        .iter()
        .filter(|r| r.fields.contains_key("guard_branch"))
        .map(|r| r.name.as_str())
        .collect();
    expected.sort_unstable();
    assert!(
        !expected.is_empty() && expected.len() < scan.results.len(),
        "fixture must hold both guarded and unguarded rows, got {}/{}",
        expected.len(),
        scan.results.len()
    );

    let patterned = query(
        &mut t,
        "FIND symbols WHERE fql_kind = 'function' WHERE guard_branch MATCHES '.*' LIMIT 500",
    );
    let mut got: Vec<&str> = patterned.results.iter().map(|r| r.name.as_str()).collect();
    got.sort_unstable();
    assert_eq!(
        got, expected,
        "the pattern answer must be the rows the filter accepts, neither the \
         keyed subset nor the whole scan"
    );
}

#[test]
fn refused_fields_error_rather_than_answering_nothing() {
    let mut t = guarded_workspace(2);
    for row in FIELD_TIERS {
        // `count` is refused in a `WHERE` and answered in an `ORDER BY`, so it
        // is not one field this loop can assert about; its own test covers the
        // split.
        if !row.is_refused_everywhere() {
            continue;
        }
        for name in std::iter::once(row.field).chain(row.aliases.iter().copied()) {
            for fql in [
                format!("FIND symbols WHERE {name} = 'x'"),
                format!("FIND symbols ORDER BY {name} LIMIT 5"),
                format!("FIND symbols GROUP BY {name}"),
            ] {
                assert!(
                    t.try_fql(&fql).is_err(),
                    "{name} is declared Refused but `{fql}` answered instead of \
                     erroring"
                );
            }
        }
    }
}

/// The refusal has to hold on every verb that accepts a clause, not only on
/// the one it was written for.
///
/// `FIND usages` builds occurrence rows that carry no kind at all, and `FIND
/// files` builds file rows that carry none either; both used to run the clause
/// filter over those rows and return a confident empty answer. `SHOW outline`,
/// `SHOW members`, `SHOW callees` and `FIND callees OF` used to answer with a
/// JSON value and had no channel to refuse through at all — they have one now,
/// and this is what says so.
#[test]
fn node_kind_is_refused_on_every_verb_that_accepts_a_clause() {
    let mut t = guarded_workspace(2);
    for fql in [
        "FIND symbols WHERE node_kind = 'x'",
        "FIND symbols ORDER BY node_kind LIMIT 5",
        "FIND symbols GROUP BY node_kind",
        "FIND globals WHERE node_kind = 'x'",
        "FIND globals ORDER BY node_kind LIMIT 5",
        "FIND globals GROUP BY node_kind",
        "FIND usages OF 'sym_0' WHERE node_kind = 'x'",
        "FIND usages OF 'sym_0' ORDER BY node_kind LIMIT 5",
        "FIND usages OF 'sym_0' GROUP BY node_kind",
        "FIND files WHERE node_kind = 'x'",
        "FIND files ORDER BY node_kind LIMIT 5",
        "FIND files GROUP BY node_kind",
        "SHOW outline OF 'guards.cpp' WHERE node_kind = 'x'",
        "SHOW callees OF 'sym_0' WHERE node_kind = 'x'",
        "FIND callees OF 'sym_0' WHERE node_kind = 'x'",
    ] {
        let err = t
            .try_fql(fql)
            .err()
            .unwrap_or_else(|| panic!("`{fql}` answered instead of erroring"));
        let msg = err.to_string();
        assert!(
            msg.contains("node_kind"),
            "`{fql}` was refused without naming the field: {msg}"
        );
    }

    // The verbs that need a type symbol, on a fixture that has one.
    let mut typed = typed_workspace();
    for fql in [
        "SHOW members OF 'Point' WHERE node_kind = 'x'",
        "SHOW members OF 'Point' ORDER BY node_kind",
        "SHOW outline OF 'shapes.cpp' GROUP BY node_kind",
    ] {
        let err = typed
            .try_fql(fql)
            .err()
            .unwrap_or_else(|| panic!("`{fql}` answered instead of erroring"));
        assert!(
            err.to_string().contains("node_kind"),
            "`{fql}` was refused without naming the field: {err}"
        );
    }
}

#[test]
fn a_refusal_reaches_the_caller_as_an_error_not_an_answer() {
    // `SHOW outline`, `SHOW members`, `SHOW callees` and `FIND callees` used to
    // hand back a JSON value with nowhere to put a refusal, so a clause naming
    // a field their rows cannot carry was accepted and dropped every row in
    // silence. This is the pin on that channel: an `Err`, not an empty answer,
    // and not an `Ok` carrying an error payload.
    let mut t = typed_workspace();
    for (fql, field) in [
        // `SHOW outline` filters and nothing else, so its row shape IS the
        // universe and `usages` is refused on that ground alone.
        ("SHOW outline OF 'shapes.cpp' WHERE usages > 0", "usages"),
        // `SHOW members` / `SHOW callees` also resolve a symbol, so what they
        // refuse is what no row of any shape can answer.
        ("SHOW members OF 'Point' WHERE size > 1", "size"),
        ("SHOW callees OF 'point_sum' WHERE marker = 'x'", "marker"),
        (
            "FIND callees OF 'point_sum' ORDER BY declaration",
            "declaration",
        ),
    ] {
        match t.try_fql(fql) {
            Err(e) => assert!(
                e.to_string().contains(field),
                "`{fql}` was refused without naming '{field}': {e}"
            ),
            Ok(answer) => panic!("`{fql}` answered instead of erroring: {answer:?}"),
        }
    }
}

#[test]
fn a_refused_field_errors_in_every_clause_that_cannot_answer_it() {
    // The contract, driven from the table: a clause naming a declared-but-
    // unanswerable field errors. It never returns zero rows, because zero rows
    // is a claim about the corpus and an error is a fact about the query.
    //
    // Every alias is exercised too — `ext` and `content` reach these rows only
    // through the alias table, and an alias that slips past validation is the
    // same false absence under another spelling.
    let mut t = guarded_workspace(3);
    let populated = query(&mut t, "FIND symbols LIMIT 10");
    assert!(!populated.results.is_empty(), "fixture indexed nothing");

    for row in refused_fields() {
        for name in std::iter::once(row.field).chain(row.aliases.iter().copied()) {
            for fql in [
                format!("FIND symbols WHERE {name} != 'zzz' LIMIT 10"),
                format!("FIND symbols GROUP BY {name}"),
            ] {
                let err = t
                    .try_fql(&fql)
                    .err()
                    .unwrap_or_else(|| panic!("`{fql}` answered instead of erroring"));
                assert!(
                    err.to_string().contains(name),
                    "`{fql}` was refused without naming the field: {err}"
                );
            }
        }
    }
}

#[test]
fn count_is_refused_before_the_grouping_pass_and_answered_after_it() {
    // The one field whose answer depends on the clause, not the operator.
    // `GROUP BY file ORDER BY count DESC` is the documented shape and has to
    // keep working; `WHERE count >= 2` before any grouping reads nothing on
    // every row, and used to return an empty answer for that reason.
    let mut t = guarded_workspace(3);

    let grouped = query(
        &mut t,
        "FIND symbols WHERE fql_kind = 'function' GROUP BY path ORDER BY count DESC",
    );
    assert!(
        !grouped.results.is_empty(),
        "ORDER BY count after GROUP BY must still answer"
    );

    let err = t
        .try_fql("FIND symbols WHERE count >= 2 LIMIT 10")
        .expect_err("WHERE count answered instead of erroring");
    let msg = err.to_string();
    assert!(
        msg.contains("count") && msg.contains("HAVING"),
        "the refusal should send the agent to HAVING: {msg}"
    );
}

#[test]
fn the_source_of_a_declared_field_is_consistent_with_its_tier() {
    for row in FIELD_TIERS {
        if row.source == Source::Absent {
            assert!(
                row.serving.iter().all(|s| s.tier == Tier::Refused),
                "{}: nothing stores it, so nothing but a refusal is honest",
                row.field
            );
        }
        if row.source == Source::EnrichmentColumn {
            assert!(
                row.budget.is_some(),
                "{}: enrichment keys are bounded, and the bound belongs here",
                row.field
            );
        }
    }
}

/// `kind` is an alias of `fql_kind` on every row type that prints a kind, and
/// an alias that resolves to nothing is the defect this slice exists to
/// remove.
///
/// It is worth pinning because `kind` is the key a `FIND symbols` row is
/// printed under in JSON output: an agent that copies the field name out of
/// the answer it was just given must get rows back, not a confident zero.
#[test]
fn kind_answers_exactly_as_fql_kind_does() {
    let mut t = guarded_workspace(3);

    let by_alias = query(&mut t, "FIND symbols WHERE kind = 'function' LIMIT 100");
    let by_canonical = query(&mut t, "FIND symbols WHERE fql_kind = 'function' LIMIT 100");
    assert!(
        !by_canonical.results.is_empty(),
        "fixture indexed no functions, so this proves nothing"
    );
    assert_eq!(
        names(&by_alias),
        names(&by_canonical),
        "`kind` must answer as `fql_kind` on symbol rows"
    );

    // The same on the two row types that print the column under that name.
    //
    // Compared on the entries the predicate selected, not on the whole
    // response: `SHOW outline` derives each row's `depth` differently
    // depending on whether the filter ran during the tree walk or after it,
    // so `WHERE kind` reports 0 where `WHERE fql_kind` reports the structural
    // depth. That split predates this alias — it is there for every predicate
    // that is not `fql_kind` — and it is a separate defect, not this claim.
    let alias = "SHOW outline OF 'guards.cpp' WHERE kind = 'function'";
    let canonical = "SHOW outline OF 'guards.cpp' WHERE fql_kind = 'function'";
    let mut selected = |fql: &str| {
        let rendered = format!("{:?}", t.try_fql(fql).expect("query"));
        let mut found: Vec<String> = rendered
            .split("name: \"")
            .skip(1)
            .filter_map(|s| s.split('"').next().map(str::to_owned))
            .collect();
        found.sort_unstable();
        found
    };
    let from_alias = selected(alias);
    let from_canonical = selected(canonical);
    assert!(
        !from_alias.is_empty(),
        "the outline fixture selected nothing, so this proves nothing"
    );
    assert_eq!(
        from_alias, from_canonical,
        "`{alias}` and `{canonical}` must select the same entries"
    );
}

/// A `GROUP BY` this release declares groupable must render real groups, not
/// one fabricated group holding everything.
///
/// CSV is the default output format, and the compact renderer keys each row
/// through `group_key`, falling back to the literal `(empty)` when the row
/// could not resolve the field. That is exactly the shape the columnar
/// backend's own refusal text calls out as indistinguishable from an answer —
/// so a field the grouping check admits has to survive the render too, or the
/// refusal was traded for a fabrication one layer further out.
#[test]
fn newly_groupable_fields_render_real_groups() {
    let mut t = guarded_workspace(3);
    let mut csv = |fql: &str| forgeql_core::compact::to_compact(&t.exec(fql));

    // `kind` is an alias of `fql_kind`: the two spellings must render the
    // same groups, not merely both render something. Grouping on the kind is
    // the one case with no key column at all — the compact layout already
    // groups a symbol listing by kind — so what is compared is the group rows.
    let by_alias = csv("FIND symbols GROUP BY kind");
    let by_canonical = csv("FIND symbols GROUP BY fql_kind");
    assert!(
        !by_canonical.contains("(empty)"),
        "`GROUP BY fql_kind` itself fabricated a group, so this proves nothing:\n{by_canonical}"
    );
    assert_eq!(
        group_lines(&by_alias),
        group_lines(&by_canonical),
        "`GROUP BY kind` must render the same groups as `GROUP BY fql_kind`"
    );
    // Where there IS a key column — every grouping but the kind one — it is
    // labelled with the spelling the agent wrote, while the grouping itself
    // runs on the canonical field. `path` and `file` are the pair that proves
    // both halves: same groups, different label.
    let by_path = csv("FIND symbols GROUP BY path");
    let by_file = csv("FIND symbols GROUP BY file");
    assert_eq!(
        group_lines(&by_path),
        group_lines(&by_file),
        "`GROUP BY file` must render the same groups as `GROUP BY path`"
    );
    assert!(
        by_file.contains("\"file\"") && by_path.contains("\"path\""),
        "the key column must be labelled with the spelling the agent \
         wrote:\n{by_file}\n{by_path}"
    );

    // `node_id` is groupable by the same table and collapsed the same way.
    //
    // A row that genuinely carries no handle — a kind the index does not make
    // addressable — still groups under `(empty)`, and that is an honest key
    // for "this row has no node_id". The defect was every row landing there
    // because the projection resolved the field for none of them.
    // Counted against the ungrouped result rather than asserted to be "more
    // than one": a handle is unique per node, so every row that carries one
    // is its own group, and every row that does not belongs to `(empty)`.
    // Anything looser passes while most rows still fail to resolve.
    let by_node_id = csv("FIND symbols GROUP BY node_id");
    let ungrouped = query(&mut t, "FIND symbols LIMIT 1000");
    let with_handle = ungrouped
        .results
        .iter()
        .filter(|r| r.node_id.is_some())
        .count();
    let without_handle = ungrouped.results.len() - with_handle;

    let resolved_groups = by_node_id
        .lines()
        .filter(|l| l.starts_with("\"n") && l.contains('.'))
        .count();
    assert_eq!(
        resolved_groups, with_handle,
        "every row carrying a handle must be its own group; \
         {with_handle} rows have one and {resolved_groups} groups resolved:\n{by_node_id}"
    );

    let empty_count: usize = by_node_id
        .lines()
        .find_map(|l| l.strip_prefix("\"(empty)\","))
        .map_or(0, |n| n.trim().parse().unwrap_or(0));
    assert_eq!(
        empty_count, without_handle,
        "`(empty)` must hold exactly the rows that carry no handle:\n{by_node_id}"
    );
}

/// The group rows of a grouped CSV answer, without the two header lines.
///
/// The second header line is the key column's label, which is the spelling the
/// agent wrote and so differs between an alias and its canonical name by
/// design. Comparing whole CSVs would call that a difference in the answer.
fn group_lines(csv: &str) -> Vec<&str> {
    csv.lines().skip(2).collect()
}

#[test]
fn every_alias_answers_exactly_as_its_canonical_name_does() {
    // Driven from the table rather than from one hard-coded pair. The failure
    // this guards against is not hypothetical: `kind` reached the grouping
    // renderer unresolved and rendered one `(empty)` group where `fql_kind`
    // rendered three, because the alias was known to one implementation of
    // grouping and not to the other. A second alias added to `FIELD_TIERS`
    // reproduces that the moment any resolver is taught names one at a time.
    let mut t = guarded_workspace(3);

    for row in FIELD_TIERS {
        for &alias in row.aliases {
            for (with_alias, with_canonical) in [
                (
                    format!("FIND symbols GROUP BY {alias}"),
                    format!("FIND symbols GROUP BY {}", row.field),
                ),
                (
                    format!("FIND symbols WHERE {alias} != 'zzz' LIMIT 50"),
                    format!("FIND symbols WHERE {} != 'zzz' LIMIT 50", row.field),
                ),
            ] {
                match (t.try_fql(&with_alias), t.try_fql(&with_canonical)) {
                    (Err(_), Err(_)) => {}
                    (Ok(a), Ok(c)) => {
                        let a = forgeql_core::compact::to_compact(&a);
                        let c = forgeql_core::compact::to_compact(&c);
                        assert_eq!(
                            group_lines(&a),
                            group_lines(&c),
                            "`{with_alias}` and `{with_canonical}` are the same query \
                             written two ways and answered differently"
                        );
                    }
                    (a, c) => panic!(
                        "`{with_alias}` and `{with_canonical}` disagree about whether \
                         the query is answerable at all: {} vs {}",
                        if a.is_ok() { "answered" } else { "refused" },
                        if c.is_ok() { "answered" } else { "refused" },
                    ),
                }
            }
        }
    }
}

/// The set of rows an outline predicate can act on must not depend on which
/// spelling of the kind field the predicate uses.
///
/// `SHOW outline` lists structural declarations only; a `fql_kind` predicate
/// opts back into every node so the filter has the full tree to act on.
/// `kind` is documented as the same field, so it has to open the same
/// universe — otherwise one spelling searches the whole file and the other
/// searches a subset, and the narrower one answers zero for a node that is
/// plainly there.
///
/// Pinned on a kind the structural tree does **not** contain, derived from the
/// fixture rather than hardcoded. A structural kind cannot detect this: it is
/// present in both universes, so the two spellings agree whether or not the
/// wider one was opened.
#[test]
fn kind_opens_the_same_outline_universe_as_fql_kind() {
    fn values(rendered: &str, key: &str) -> Vec<String> {
        let needle = format!("{key}: \"");
        let mut found: Vec<String> = rendered
            .split(needle.as_str())
            .skip(1)
            .filter_map(|s| s.split('"').next().map(str::to_owned))
            .collect();
        found.sort_unstable();
        found
    }

    let mut t = guarded_workspace(3);
    let mut render = |fql: &str| format!("{:?}", t.try_fql(fql).expect("query"));

    let mut structural = values(&render("SHOW outline OF 'guards.cpp'"), "fql_kind");
    structural.dedup();
    let mut every = values(&render("SHOW outline OF 'guards.cpp' ALL"), "fql_kind");
    every.dedup();

    let outside = every
        .into_iter()
        .find(|k| !structural.contains(k))
        .expect("ALL adds no kind beyond the structural tree here, so this proves nothing");

    let alias = format!("SHOW outline OF 'guards.cpp' WHERE kind = '{outside}'");
    let canonical = format!("SHOW outline OF 'guards.cpp' WHERE fql_kind = '{outside}'");
    let from_alias = values(&render(&alias), "name");
    let from_canonical = values(&render(&canonical), "name");

    assert!(
        !from_canonical.is_empty(),
        "`{canonical}` selected nothing, so this proves nothing"
    );
    assert_eq!(
        from_alias, from_canonical,
        "`kind` must open the same outline universe as `fql_kind`: a kind outside \
         the structural tree ('{outside}') has to be reachable under both spellings"
    );
}

#[test]
fn any_where_opens_the_outline_universe_not_only_a_kind_predicate() {
    // Which field the predicate named used to decide which rows existed. A
    // `WHERE name` searched the structural tree while a `WHERE fql_kind`
    // searched the file, so a non-structural node was absent from one and
    // present in the other — and `depth`, which counts the listed ancestors,
    // differed for the same node between the two.
    //
    // The fixture's guard regions are the non-structural rows: `#if` and
    // `#elif` are not declarations, so a bare outline omits them. The needle is
    // the rendered field, not the bare word: the fixture's file is called
    // `guards.cpp`, so `contains("guard")` is true of every answer.
    const GUARD_ROW: &str = r#"fql_kind: "guard""#;

    let mut t = guarded_workspace(3);
    let render =
        |t: &mut common::TestSession, fql: &str| format!("{:?}", t.try_fql(fql).expect("query"));

    let bare = render(&mut t, "SHOW outline OF 'guards.cpp'");
    assert!(
        !bare.contains(GUARD_ROW),
        "a bare outline should list structural declarations only, so this \
         proves nothing:\n{bare}"
    );

    let by_kind = render(
        &mut t,
        "SHOW outline OF 'guards.cpp' WHERE fql_kind = 'guard'",
    );
    assert!(
        by_kind.contains(GUARD_ROW),
        "a kind predicate has always opened the full set:\n{by_kind}"
    );

    // The predicate that used not to: `line` names no kind at all.
    let by_line = render(&mut t, "SHOW outline OF 'guards.cpp' WHERE line >= 1");
    assert!(
        by_line.contains(GUARD_ROW),
        "a WHERE on a field other than the kind must open the same set — a \
         filter that cannot see a row cannot report it:\n{by_line}"
    );
}

/// A workspace where two languages define the same names, on either backend.
///
/// `Point` is a struct in both files with a different set of members, and
/// `shared_fn` is a function in both with a different set of callees. That
/// difference is what makes a `WHERE language` assertion falsifiable: a
/// predicate that is DISCARDED rather than applied still answers — with the
/// other language's rows — and only looking at which rows came back can tell
/// the two apart. Both earlier attempts at this fix shipped one of those two
/// failure modes behind a green gate.
fn two_language_workspace(columnar: bool) -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("a_shapes.cpp"),
        "struct Point {\n    int x;\n    int y;\n};\n\
         int helper_a(void) { return 1; }\n\
         int helper_b(void) { return 2; }\n\
         int shared_fn(void) {\n    return helper_a() + helper_b();\n}\n",
    )
    .expect("write cpp fixture");
    std::fs::write(
        dir.path().join("b_shapes.rs"),
        "pub struct Point {\n    pub only_rust_field: u32,\n}\n\
         pub fn helper_c() -> u32 {\n    3\n}\n\
         pub fn shared_fn() -> u32 {\n    helper_c()\n}\n",
    )
    .expect("write rust fixture");
    if columnar {
        common::columnar_session_in(dir)
    } else {
        common::legacy_session_in(dir)
    }
}

/// The declaration text of every member a `SHOW members` answered with.
fn member_texts(t: &mut common::TestSession, fql: &str) -> Vec<String> {
    match t.try_fql(fql).unwrap_or_else(|e| panic!("`{fql}`: {e}")) {
        ForgeQLResult::Show(show) => match show.content {
            forgeql_core::result::ShowContent::Members { members, .. } => {
                members.iter().map(|m| m.text.clone()).collect()
            }
            other => panic!("`{fql}` did not answer with members: {other:?}"),
        },
        other => panic!("`{fql}` did not answer with a show result: {other:?}"),
    }
}

/// The name of every callee a `SHOW callees` answered with.
fn callee_names(t: &mut common::TestSession, fql: &str) -> Vec<String> {
    match t.try_fql(fql).unwrap_or_else(|e| panic!("`{fql}`: {e}")) {
        ForgeQLResult::Show(show) => match show.content {
            forgeql_core::result::ShowContent::CallGraph { entries, .. } => {
                entries.iter().map(|e| e.name.clone()).collect()
            }
            other => panic!("`{fql}` did not answer with a call graph: {other:?}"),
        },
        other => panic!("`{fql}` did not answer with a show result: {other:?}"),
    }
}

/// The text of every source line a reading verb answered with.
fn line_texts(t: &mut common::TestSession, fql: &str) -> Vec<String> {
    match t.try_fql(fql).unwrap_or_else(|e| panic!("`{fql}`: {e}")) {
        ForgeQLResult::Show(show) => match show.content {
            forgeql_core::result::ShowContent::Lines { lines, .. } => {
                lines.iter().map(|l| l.text.clone()).collect()
            }
            other => panic!("`{fql}` did not answer with lines: {other:?}"),
        },
        other => panic!("`{fql}` did not answer with a show result: {other:?}"),
    }
}

/// The error a query answered with, or a panic naming the query that answered.
fn refusal(t: &mut common::TestSession, fql: &str) -> String {
    t.try_fql(fql)
        .err()
        .unwrap_or_else(|| panic!("`{fql}` answered instead of erroring"))
        .to_string()
}

#[test]
fn a_lookup_predicate_scopes_the_symbol_it_addresses() {
    // `SHOW members OF 'Point'` is ambiguous where two languages both define
    // `Point`. `WHERE language = '…'` is the documented way to say which, and
    // it has to reach the LOOKUP: a members row carries no `language`, so
    // applying it to the rows instead answers with none — a confident zero for
    // a type that exists.
    //
    // Every assertion below names a row that only one of the two candidates
    // could have produced. That is deliberate: an assertion that only counts
    // rows, or only checks the echoed symbol name, passes whether the
    // predicate scoped the lookup or was thrown away.
    for columnar in [false, true] {
        let backend = if columnar { "columnar" } else { "legacy" };
        let mut t = two_language_workspace(columnar);

        let mut langs: Vec<String> = query(&mut t, "FIND symbols WHERE name = 'Point'")
            .results
            .iter()
            .filter_map(|r| r.language.clone())
            .collect();
        langs.sort_unstable();
        langs.dedup();
        assert_eq!(
            langs,
            vec!["cpp".to_string(), "rust".to_string()],
            "{backend}: the fixture no longer defines Point in exactly cpp and rust"
        );

        // Members: each language's own fields, and none of the other's.
        let cpp = member_texts(&mut t, "SHOW members OF 'Point' WHERE language = 'cpp'");
        assert!(
            cpp.iter().any(|m| m.contains("int x")),
            "{backend}: the C++ Point was not the one resolved: {cpp:?}"
        );
        assert!(
            !cpp.iter().any(|m| m.contains("only_rust_field")),
            "{backend}: the predicate was discarded — the Rust Point answered: {cpp:?}"
        );

        let rust = member_texts(&mut t, "SHOW members OF 'Point' WHERE language = 'rust'");
        assert!(
            rust.iter().any(|m| m.contains("only_rust_field")),
            "{backend}: the Rust Point was not the one resolved: {rust:?}"
        );
        assert!(
            !rust.iter().any(|m| m.contains("int x")),
            "{backend}: the predicate was discarded — the C++ Point answered: {rust:?}"
        );

        // Callees: the same, through a different row shape.
        let cpp_calls = callee_names(&mut t, "SHOW callees OF 'shared_fn' WHERE language = 'cpp'");
        assert!(
            cpp_calls.iter().any(|c| c == "helper_a") && !cpp_calls.iter().any(|c| c == "helper_c"),
            "{backend}: SHOW callees resolved the wrong shared_fn: {cpp_calls:?}"
        );
        let rust_calls = callee_names(
            &mut t,
            "SHOW callees OF 'shared_fn' WHERE language = 'rust'",
        );
        assert!(
            rust_calls.iter().any(|c| c == "helper_c")
                && !rust_calls.iter().any(|c| c == "helper_a"),
            "{backend}: SHOW callees resolved the wrong shared_fn: {rust_calls:?}"
        );

        // A field BOTH shapes carry, where the row's value comes from the
        // resolved symbol. Every callee row reports the caller's own file, so
        // filtering rows by `path` can only keep all of them or none — and
        // routing it to the rows alone answered zero whenever the lookup had
        // picked the other language's `shared_fn`.
        let by_cpp_path = callee_names(
            &mut t,
            "SHOW callees OF 'shared_fn' WHERE path LIKE '%a_shapes.cpp'",
        );
        assert!(
            by_cpp_path.iter().any(|c| c == "helper_a"),
            "{backend}: WHERE path did not reach the lookup: {by_cpp_path:?}"
        );
        let by_rust_path = callee_names(
            &mut t,
            "SHOW callees OF 'shared_fn' WHERE path LIKE '%b_shapes.rs'",
        );
        assert!(
            by_rust_path.iter().any(|c| c == "helper_c"),
            "{backend}: WHERE path did not reach the lookup: {by_rust_path:?}"
        );
        // And through the reading verbs, whose rows are source lines.
        let rust_body = line_texts(
            &mut t,
            "SHOW body OF 'shared_fn' DEPTH 99 WHERE language = 'rust'",
        );
        assert!(
            rust_body.iter().any(|l| l.contains("helper_c"))
                && !rust_body.iter().any(|l| l.contains("helper_a")),
            "{backend}: SHOW body resolved the wrong shared_fn: {rust_body:?}"
        );

        // Both halves of one clause, at once: the lookup picks the Rust Point,
        // then the row half filters its members. Neither half alone can
        // produce this answer.
        let split = member_texts(
            &mut t,
            "SHOW members OF 'Point' WHERE language = 'rust' WHERE text LIKE '%only_rust_field%'",
        );
        assert_eq!(
            split.len(),
            1,
            "{backend}: the clause did not split between the lookup and the rows: {split:?}"
        );

        // Three outcomes, three distinguishable answers. A language nothing
        // defines is a lookup that matched nothing — a fact about the
        // workspace — and must not read as a refusal.
        let missed = refusal(&mut t, "SHOW members OF 'Point' WHERE language = 'python'");
        assert!(
            missed.contains("'Point'") && missed.contains("WHERE language = 'python'"),
            "{backend}: a scoped-away symbol did not name itself and the clause \
             that excluded it: {missed}"
        );
        assert!(
            !missed.contains("cannot be answered"),
            "{backend}: a scoped-away symbol read as a refusal: {missed}"
        );

        // A field no shape can answer is a fact about the query.
        let refused = refusal(&mut t, "SHOW members OF 'Point' WHERE size > 1");
        assert!(
            refused.contains("cannot be answered"),
            "{backend}: an unanswerable field did not refuse: {refused}"
        );
    }
}

#[test]
fn only_where_is_split_between_the_lookup_and_the_rows() {
    // The asymmetry, stated as a test because it is otherwise invisible.
    //
    // `WHERE` has two possible consumers, so a name the rows cannot carry is
    // legitimate — it is about the symbol. `ORDER BY`, `GROUP BY` and `HAVING`
    // have one: no resolver reads them. So the same field name is accepted in
    // one clause and refused in the other three, and that is not an
    // inconsistency but the difference between scoping a lookup and shaping an
    // answer.
    let mut t = two_language_workspace(true);

    assert!(
        !member_texts(&mut t, "SHOW members OF 'Point' WHERE language = 'rust'").is_empty(),
        "WHERE language must reach the lookup"
    );

    for fql in [
        "SHOW members OF 'Point' ORDER BY language",
        "SHOW members OF 'Point' GROUP BY language",
        "SHOW members OF 'Point' HAVING language = 'rust'",
        "SHOW callees OF 'shared_fn' ORDER BY language",
        "SHOW body OF 'shared_fn' GROUP BY language",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("cannot be answered"),
            "`{fql}` was not held to the row shape: {err}"
        );
    }
}

#[test]
fn a_glob_scopes_the_lookup_when_the_rows_have_no_file_of_their_own() {
    // `IN` and `EXCLUDE` are a statement about a file, and a members row and a
    // source line both report no path at all — so retaining the globs in the
    // row half dropped every row, and `SHOW members OF 'Point' IN '…'` answered
    // zero for a type that lives exactly there. The same confident absence the
    // `WHERE` split removes, arriving through the glob instead.
    for columnar in [false, true] {
        let backend = if columnar { "columnar" } else { "legacy" };
        let mut t = two_language_workspace(columnar);

        let scoped = member_texts(&mut t, "SHOW members OF 'Point' IN 'b_shapes.rs'");
        assert!(
            scoped.iter().any(|m| m.contains("only_rust_field")),
            "{backend}: IN emptied the rows instead of scoping the lookup: {scoped:?}"
        );

        let cpp = member_texts(&mut t, "SHOW members OF 'Point' IN 'a_shapes.cpp'");
        assert!(
            cpp.iter().any(|m| m.contains("int x")),
            "{backend}: IN did not scope the lookup to the C++ Point: {cpp:?}"
        );

        let lines = line_texts(&mut t, "SHOW body OF 'shared_fn' DEPTH 99 IN 'b_shapes.rs'");
        assert!(
            lines.iter().any(|l| l.contains("helper_c")),
            "{backend}: IN emptied a body it should have scoped: {lines:?}"
        );
    }

    // And on the verbs that resolve no name, a glob has neither a lookup to
    // scope nor a row path to match, so it is refused rather than accepted and
    // left wholly inert.
    let mut t = two_language_workspace(true);
    let (node, _rev) = t.file_handle("a_shapes.cpp");
    for fql in [
        format!("SHOW NODE '{node}' IN 'nowhere/**'"),
        "SHOW LINES 1-2 OF 'a_shapes.cpp' IN 'nowhere/**'".to_string(),
        "SHOW MORE IN 'nowhere/**'".to_string(),
        "SHOW COMMITS IN 'nowhere/**'".to_string(),
        "SHOW COMMITS EXCLUDE 'doc/**'".to_string(),
    ] {
        let err = refusal(&mut t, &fql);
        assert!(
            err.contains("IN / EXCLUDE cannot be answered"),
            "`{fql}` accepted a glob it does nothing with: {err}"
        );
    }

    // Sorting and grouping are the same class one clause further on: a line
    // answer comes back in source order and nothing sorts it, so an accepted
    // `ORDER BY` was read by nothing — and because `LIMIT` *is* honoured, that
    // silence paged from the wrong end rather than doing nothing at all.
    for fql in [
        "SHOW body OF 'shared_fn' DEPTH 99 ORDER BY line DESC LIMIT 2",
        "SHOW body OF 'shared_fn' DEPTH 99 GROUP BY text",
        "SHOW context OF 'shared_fn' HAVING count > 1",
        "SHOW LINES 1-2 OF 'a_shapes.cpp' ORDER BY line DESC",
        &format!("SHOW NODE '{node}' ORDER BY line DESC"),
        "SHOW MORE ORDER BY line DESC",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("nothing here to shape"),
            "`{fql}` accepted a clause nothing applies: {err}"
        );
    }

    // DEPTH is the same shape once more: three verbs read it and the parser
    // hands it to all fifteen. On `SHOW outline`, whose rows carry a literal
    // `depth` column, `DEPTH 2` reads as a depth-limited tree and returned the
    // whole one.
    for fql in [
        "SHOW outline OF 'a_shapes.cpp' DEPTH 2",
        "SHOW members OF 'Point' DEPTH 2",
        "SHOW callees OF 'shared_fn' DEPTH 2",
        "FIND symbols WHERE name = 'Point' DEPTH 2",
        "FIND usages OF 'helper_c' DEPTH 2",
        "SHOW COMMITS DEPTH 2",
        "SHOW LINES 1-2 OF 'a_shapes.cpp' DEPTH 2",
        "SHOW signature OF 'shared_fn' DEPTH 2",
        "SHOW DIFF DEPTH 2",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("DEPTH cannot be answered"),
            "`{fql}` accepted a DEPTH nothing reads: {err}"
        );
    }
    // And the three that do read it still do.
    assert!(
        !line_texts(&mut t, "SHOW body OF 'shared_fn' DEPTH 99").is_empty(),
        "DEPTH stopped working on the verb whose collapse level it is"
    );

    // A mutation reads no clause at all, and is the worst place to ignore one.
    for fql in [
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' LIMIT 5",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' OFFSET 5",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' WHERE name = 'y'",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' IN 'nowhere/**'",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' EXCLUDE 'nowhere/**'",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' ORDER BY name",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' GROUP BY name",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' HAVING count > 1",
        "CHANGE FILE 'a_shapes.cpp' WITH 'x' DEPTH 2",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("reads no clause") || err.contains("DEPTH cannot be answered"),
            "`{fql}` accepted a clause that scopes nothing on a mutation: {err}"
        );
    }
    let err = refusal(&mut t, "CHANGE FILE 'a_shapes.cpp' WITH 'x' LIMIT 5");
    assert!(
        err.contains("reads no clause"),
        "CHANGE FILE accepted a clause that scopes nothing: {err}"
    );

    // OFFSET on its own is honoured, not silently dropped for want of a LIMIT.
    let whole = line_texts(&mut t, "SHOW body OF 'shared_fn' DEPTH 99");
    let skipped = line_texts(&mut t, "SHOW body OF 'shared_fn' DEPTH 99 OFFSET 1");
    assert_eq!(
        skipped.len(),
        whole.len() - 1,
        "OFFSET without a LIMIT was read by nothing: {skipped:?} vs {whole:?}"
    );
    assert_eq!(
        skipped.first(),
        whole.get(1),
        "OFFSET returned the page it was asked to skip"
    );
}

#[test]
fn show_signature_refuses_a_field_it_has_no_rows_to_filter() {
    // `SHOW signature` renders one line rather than a row set, so a predicate
    // naming a field only a source line carries has nothing to act on. Saying
    // so beats accepting it and answering as though it had been applied — the
    // failure mode this release exists to remove, in its quietest form.
    let mut t = two_language_workspace(true);

    for fql in [
        "SHOW signature OF 'shared_fn' WHERE text LIKE '%nothing%'",
        "SHOW signature OF 'shared_fn' WHERE marker = 'x'",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("nothing here to filter"),
            "`{fql}` was accepted and ignored instead of refused: {err}"
        );
    }

    // And there are no rows to shape either.
    for fql in [
        "SHOW signature OF 'shared_fn' ORDER BY line",
        "SHOW signature OF 'shared_fn' GROUP BY name",
        "SHOW signature OF 'shared_fn' HAVING count > 1",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("nothing here to shape"),
            "`{fql}` was accepted and ignored instead of refused: {err}"
        );
    }

    // What the clause CAN do there is scope the lookup, and still does — for
    // every field a symbol row carries, including the two a source line
    // carries too. Splitting the clause here would have dropped exactly those.
    let rust = format!(
        "{:?}",
        t.try_fql("SHOW signature OF 'shared_fn' WHERE language = 'rust'")
            .expect("a lookup-scoping predicate must still be accepted")
    );
    assert!(
        rust.contains("u32"),
        "SHOW signature resolved the wrong shared_fn: {rust}"
    );

    let missed = refusal(&mut t, "SHOW signature OF 'shared_fn' WHERE line = 99999");
    assert!(
        missed.contains("WHERE line = 99999"),
        "a field both a line and a symbol row carry was swallowed rather than \
         given to the lookup: {missed}"
    );
}
#[test]
fn the_row_half_of_a_split_clause_still_filters() {
    // What the split must not break: a predicate the rows DO carry keeps
    // filtering them, and never reaches the lookup to scope it away.
    let mut t = typed_workspace();

    let body = line_count(&mut t, "SHOW body OF 'point_sum' DEPTH 99");
    assert!(body > 0, "fixture body is empty");
    let matching = line_count(
        &mut t,
        "SHOW body OF 'point_sum' DEPTH 99 WHERE text MATCHES 'return'",
    );
    assert!(
        matching > 0 && matching < body,
        "the filtering half of the clause stopped working: {matching} of {body} lines"
    );

    let baseline = member_count(&mut t, "SHOW members OF 'Point'");
    let fields = member_count(&mut t, "SHOW members OF 'Point' WHERE kind = 'field'");
    assert!(
        fields > 0 && fields <= baseline,
        "a members-row field must filter members, not empty them: {fields} of {baseline}"
    );

    // And what neither the rows nor any other shape can answer is refused.
    for fql in [
        "SHOW members OF 'Point' WHERE size > 1",
        "SHOW callees OF 'point_sum' WHERE marker = 'x'",
        "SHOW body OF 'point_sum' WHERE declaration = 'x'",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("cannot be answered"),
            "`{fql}` failed for the wrong reason: {err}"
        );
    }
}

/// How many member rows a `SHOW members` answered with — the number the
/// envelope's echoed symbol name says nothing about.
fn member_count(t: &mut common::TestSession, fql: &str) -> usize {
    match t.try_fql(fql).unwrap_or_else(|e| panic!("`{fql}`: {e}")) {
        ForgeQLResult::Show(show) => match show.content {
            forgeql_core::result::ShowContent::Members { members, .. } => members.len(),
            other => panic!("`{fql}` did not answer with members: {other:?}"),
        },
        other => panic!("`{fql}` did not answer with a show result: {other:?}"),
    }
}

/// How many source lines a reading verb answered with.
fn line_count(t: &mut common::TestSession, fql: &str) -> usize {
    match t.try_fql(fql).unwrap_or_else(|e| panic!("`{fql}`: {e}")) {
        ForgeQLResult::Show(show) => match show.content {
            forgeql_core::result::ShowContent::Lines { lines, .. } => lines.len(),
            other => panic!("`{fql}` did not answer with lines: {other:?}"),
        },
        other => panic!("`{fql}` did not answer with a show result: {other:?}"),
    }
}

// ── Every clause-carrying verb, read off the IR rather than remembered ──────

/// Every `ForgeQLIR` variant that carries a clause, read off the declaration.
///
/// Reading it rather than restating it is the point. `SHOW COMMITS` carries a
/// clause, filters rows with it, and was missing from two hand-written
/// enumerations of exactly this set — so it filtered `SymbolMatch` rows whose
/// `path`, `line` and `fql_kind` are all `None` and answered a confident zero
/// to `WHERE path LIKE '%src%'`. A list derived from the enum cannot omit a
/// verb; a list written from memory already did.
fn clause_carrying_variants() -> Vec<String> {
    let src = include_str!("../src/ir.rs");
    let body = src
        .split_once("pub enum ForgeQLIR {")
        .expect("ir.rs no longer declares `pub enum ForgeQLIR`")
        .1;

    let mut variants = Vec::new();
    let mut current: Option<String> = None;
    let mut chunk = String::new();
    let finish = |current: &mut Option<String>, chunk: &mut String, out: &mut Vec<String>| {
        if let Some(name) = current.take()
            && chunk.contains("clauses: Clauses")
        {
            out.push(name);
        }
        chunk.clear();
    };

    for line in body.lines() {
        if line == "}" {
            break;
        }
        let opens_variant = line
            .strip_prefix("    ")
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()));
        if opens_variant {
            finish(&mut current, &mut chunk, &mut variants);
            current = Some(
                line.trim_start()
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        chunk.push_str(line);
        chunk.push('\n');
    }
    finish(&mut current, &mut chunk, &mut variants);

    variants.sort_unstable();
    variants
}

/// One row per clause-carrying verb: a query naming a field that verb's rows
/// cannot answer, which must therefore be refused rather than answered.
///
/// `None` is an exemption, and it has to be written here with its reason
/// beside it. That is the difference this table is for: a verb nobody thought
/// about is a missing row and fails the test, not a silent gap.
///
/// `<NODE>` is substituted with a real handle from the fixture.
const CLAUSE_FIELD_PROBES: &[(&str, Option<&str>)] = &[
    (
        "ChangeContent",
        // `CHANGE FILE` reads no clause: the range it rewrites lives in its
        // target, not here. The probe refuses, so it edits nothing.
        Some("CHANGE FILE 'a_shapes.cpp' WITH 'x' LIMIT 5"),
    ),
    ("FindFiles", Some("FIND files WHERE usages > 1")),
    ("FindSymbols", Some("FIND symbols WHERE size > 1")),
    (
        "FindUsages",
        Some("FIND usages OF 'shared_fn' WHERE size > 1"),
    ),
    ("ShowBody", Some("SHOW body OF 'shared_fn' WHERE size > 1")),
    (
        "ShowCallees",
        Some("SHOW callees OF 'shared_fn' WHERE size > 1"),
    ),
    ("ShowCommits", Some("SHOW COMMITS WHERE path LIKE '%src%'")),
    (
        "ShowContext",
        Some("SHOW context OF 'shared_fn' WHERE size > 1"),
    ),
    ("ShowDiff", Some("SHOW DIFF WHERE size > 1")),
    (
        "ShowLines",
        Some("SHOW LINES 1-2 OF 'a_shapes.cpp' WHERE language = 'cpp'"),
    ),
    (
        "ShowMembers",
        Some("SHOW members OF 'Point' WHERE size > 1"),
    ),
    ("ShowMore", Some("SHOW MORE WHERE size > 1")),
    (
        "ShowNode",
        // METADATA returns before the CONTENT path re-synthesises `SHOW LINES`,
        // so this form reaches the check only where `exec_show_node` runs it.
        Some("SHOW NODE '<NODE>' METADATA WHERE language = 'cpp'"),
    ),
    (
        "ShowOutline",
        Some("SHOW outline OF 'a_shapes.cpp' WHERE usages > 1"),
    ),
    (
        "ShowSignature",
        Some("SHOW signature OF 'shared_fn' WHERE size > 1"),
    ),
];

#[test]
fn every_clause_carrying_verb_decides_how_its_fields_are_checked() {
    let declared = clause_carrying_variants();
    let mut probed: Vec<String> = CLAUSE_FIELD_PROBES
        .iter()
        .map(|(variant, _)| (*variant).to_string())
        .collect();
    probed.sort_unstable();

    assert_eq!(
        declared, probed,
        "a verb carrying a clause is missing from CLAUSE_FIELD_PROBES (or listed there \
         and gone from the IR). Add it with the query that must be refused, or with \
         None and the reason it cannot be probed here"
    );

    let mut t = two_language_workspace(true);
    let (node, _rev) = t.file_handle("a_shapes.cpp");

    for (variant, probe) in CLAUSE_FIELD_PROBES {
        let Some(probe) = probe else { continue };
        let fql = probe.replace("<NODE>", &node);
        let err = refusal(&mut t, &fql);
        assert!(
            err.contains("cannot be answered") || err.contains("reads no clause"),
            "{variant}: `{fql}` must be refused, not answered with nothing — got: {err}"
        );
    }
}

#[test]
fn find_usages_refuses_a_field_its_rows_cannot_carry() {
    // `FIND usages` never went through `find_symbols`, so the columnar
    // backend's index-aware Stage 0 checks were unreachable from it: an
    // unknown field answered zero sites while `FIND symbols` and `FIND globals`
    // errored on the same name. All three clauses, because each fails
    // differently — a `WHERE` matches nothing, an `ORDER BY` silently ties, a
    // `GROUP BY` fabricates one group holding every row.
    let mut t = two_language_workspace(true);

    for fql in [
        "FIND usages OF 'shared_fn' WHERE zzz_not_a_field = 'x'",
        "FIND usages OF 'shared_fn' ORDER BY zzz_not_a_field",
        "FIND usages OF 'shared_fn' GROUP BY zzz_not_a_field",
    ] {
        let err = refusal(&mut t, fql);
        assert!(
            err.contains("zzz_not_a_field"),
            "`{fql}` did not name the field it could not answer: {err}"
        );
    }

    // And the verb still answers what it can.
    let sites = query(&mut t, "FIND usages OF 'helper_c'");
    assert!(sites.total > 0, "FIND usages stopped finding real sites");
}
