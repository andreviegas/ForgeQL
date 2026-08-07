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
    Tier, lookup,
};
use forgeql_core::filter::{CORE_WHERE_FIELDS, SORTABLE_SYMBOL_FIELDS};
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
                Tier::Scan | Tier::Refused | Tier::Unserved => assert!(
                    serving.then.is_none(),
                    "{}: {:?} has nothing to fall through to",
                    row.field,
                    serving.tier
                ),
                Tier::StoredColumn
                | Tier::KeyBitmap
                | Tier::ValueUniverse
                | Tier::NumericIndex
                | Tier::ZoneMap => assert!(
                    serving.then.is_some(),
                    "{}: {:?} stands aside for fields it cannot key, so the row \
                     must name what runs instead",
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
    for &field in SORTABLE_SYMBOL_FIELDS {
        assert!(
            declared.contains(field),
            "{field} passes ORDER BY validation but the table does not name it"
        );
    }
}

#[test]
fn no_row_is_invented() {
    // The reverse of the above: a table row must correspond to something the
    // engine really has, or the table drifts in the other direction and
    // starts documenting fields that do not exist.
    let zone_map_columns: BTreeSet<&str> = ZONEMAP_NUMERIC_FIELDS.iter().map(|&(c, _)| c).collect();
    for row in rows_except_catch_all() {
        let known = CORE_WHERE_FIELDS.contains(&row.field)
            || SORTABLE_SYMBOL_FIELDS.contains(&row.field)
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

#[test]
fn only_a_tier_that_reads_every_row_may_call_a_value_absent() {
    // The Q1 defect, as a rule. A tier assembled from keys sees only what was
    // keyed, so an empty result from it is "no candidates", never "no rows" —
    // unless a per-segment proof has separately established the absence. A
    // tier that compares each row against that row's own stored value is a
    // different thing and may conclude.
    for row in FIELD_TIERS {
        for s in row.serving {
            let legal = matches!(
                (s.tier, s.exactness),
                (Tier::Refused | Tier::Unserved, Exactness::NotApplicable)
                    | (
                        Tier::Scan | Tier::StoredColumn | Tier::NameFst | Tier::KindBitmap,
                        Exactness::Exact,
                    )
                    | (
                        Tier::KeyBitmap
                            | Tier::ValueUniverse
                            | Tier::NumericIndex
                            | Tier::ZoneMap
                            | Tier::Trigram
                            | Tier::NamePrefix
                            | Tier::NameFst,
                        Exactness::Superset,
                    )
                    | (Tier::KeyBitmap, Exactness::SupersetProvenAbsent(_))
            );
            assert!(
                legal,
                "{}: {:?} declared {:?} — a tier built from keys cannot be exact, \
                 and one that reads every row does not need a proof",
                row.field, s.tier, s.exactness
            );
            if let Exactness::SupersetProvenAbsent(proof) = s.exactness {
                assert!(
                    !proof.is_empty(),
                    "{}: a key-derived tier may only conclude an absence through a \
                     named proof",
                    row.field
                );
            }
        }
    }
}

#[test]
fn a_refused_or_unserved_field_promises_no_serving() {
    for row in FIELD_TIERS {
        let dead = row
            .serving
            .iter()
            .any(|s| matches!(s.tier, Tier::Refused | Tier::Unserved));
        if dead {
            assert!(
                row.serving
                    .iter()
                    .all(|s| matches!(s.tier, Tier::Refused | Tier::Unserved)),
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
        "a_pattern_that_accepts_the_empty_string_answers_completely",
    ),
    (
        Gap::DirtySession,
        "query_set_valued_fields::an_uncommitted_row_is_found_by_a_set_field_query \
     and query_core_field_tier's dirty case",
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
fn every_declared_gap_names_a_fallback() {
    for row in FIELD_TIERS {
        for gap in row.gaps {
            assert!(
                !gap.fallback().is_empty(),
                "{}: {gap:?} names no fallback",
                row.field
            );
        }
    }
}

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
    for (gap, note) in GAP_COVERAGE {
        assert!(!note.is_empty(), "{gap:?} has an empty coverage note");
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

/// One file whose conditional chain carries `branches` distinct
/// `guard_branch` values — the cheapest way to put a real field over a real
/// budget, since the value is the branch's position.
fn guarded_workspace(branches: usize) -> common::TestSession {
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
fn a_pattern_that_accepts_the_empty_string_answers_completely() {
    // A row whose value is empty is keyed nowhere, so a pattern that would
    // match it cannot be answered from the keys at all. The tier has to stand
    // aside rather than return what the keys happen to hold.
    let mut t = guarded_workspace(4);
    let scan = query(&mut t, "FIND symbols WHERE fql_kind = 'function' LIMIT 500");
    let patterned = query(
        &mut t,
        "FIND symbols WHERE fql_kind = 'function' WHERE guard_branch MATCHES '.*' LIMIT 500",
    );
    assert_eq!(
        patterned.results.len(),
        scan.results.len(),
        "a pattern accepting the empty string must not narrow to the keyed rows"
    );
}

#[test]
fn refused_fields_error_rather_than_answering_nothing() {
    let mut t = guarded_workspace(2);
    for row in FIELD_TIERS {
        if !row.serving.iter().any(|s| s.tier == Tier::Refused) {
            continue;
        }
        for name in std::iter::once(row.field).chain(row.aliases.iter().copied()) {
            let fql = format!("FIND symbols WHERE {name} = 'x'");
            let outcome = t.try_fql(&fql);
            assert!(
                outcome.is_err(),
                "{name} is declared Refused but `{fql}` answered instead of erroring"
            );
        }
    }
}

#[test]
fn unserved_fields_really_do_match_nothing() {
    // This test documents a defect rather than an invariant: each of these
    // names passes validation, resolves on no symbol row, and so answers a
    // confident zero. It exists so the family cannot grow quietly, and so
    // that the day one of them is fixed — by refusing it, or by resolving it
    // — this fails and the table has to be brought along.
    let mut t = guarded_workspace(3);
    let populated = query(&mut t, "FIND symbols LIMIT 10");
    assert!(!populated.results.is_empty(), "fixture indexed nothing");

    for row in FIELD_TIERS {
        if !row.serving.iter().any(|s| s.tier == Tier::Unserved) {
            continue;
        }
        for name in std::iter::once(row.field).chain(row.aliases.iter().copied()) {
            // `!= 'zzz'` matches every row that can resolve the field at all,
            // so an empty answer is proof the row cannot resolve it.
            let q = query(
                &mut t,
                &format!("FIND symbols WHERE {name} != 'zzz' LIMIT 10"),
            );
            assert!(
                q.results.is_empty(),
                "{name} now resolves on symbol rows — good, but the table still \
                 declares it Unserved"
            );
        }
    }
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
