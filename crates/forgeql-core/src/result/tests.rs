use std::collections::HashMap;

use super::*;

#[test]
fn query_result_json_contains_projected_fields() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".to_string(),
        results: vec![SymbolMatch {
            name: "setPeakLevel".to_string(),
            node_kind: Some("Function".to_string()),
            fql_kind: None,
            language: None,
            path: Some(PathBuf::from("src/signal_controller.cpp")),
            line: None,
            usages_count: Some(3),
            fields: HashMap::from([(
                "signature".to_string(),
                "void setPeakLevel(int level)".to_string(),
            )]),
            count: None,
            node_id: None,
            rev: None,
        }],
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    });

    let json_string = result.to_json();
    let v: serde_json::Value = serde_json::from_str(&json_string).unwrap();
    assert_eq!(v["op"], "find_symbols");
    assert_eq!(v["total"], 1);
    assert_eq!(v["results"][0]["name"], "setPeakLevel");
    assert_eq!(v["results"][0]["usages"], 3);
    // Raw fields must not leak.
    assert!(
        v["results"][0].get("signature").is_none(),
        "enrichment field must not leak"
    );
    assert!(
        v["results"][0].get("fields").is_none(),
        "fields HashMap must not leak"
    );
}

#[test]
fn json_find_result_includes_usages() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".to_string(),
        results: vec![SymbolMatch {
            name: "setPeakLevel".to_string(),
            node_kind: Some("Function".to_string()),
            fql_kind: None,
            language: None,
            path: Some(PathBuf::from("src/signal.cpp")),
            line: None,
            usages_count: Some(7),
            fields: HashMap::new(),
            count: None,
            node_id: None,
            rev: None,
        }],
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    });
    let json = result.to_json();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["op"], "find_symbols");
    assert_eq!(v["total"], 1);
    assert_eq!(v["results"][0]["name"], "setPeakLevel");
    assert_eq!(v["results"][0]["usages"], 7);
    // Must NOT contain raw fields HashMap.
    assert!(
        v["results"][0].get("fields").is_none(),
        "fields must not leak"
    );
}

#[test]
fn json_find_result_excludes_raw_fields() {
    // Ensure the fields HashMap is never serialized.
    let mut fields = HashMap::new();
    fields.insert("value".to_string(), "69".to_string());
    fields.insert("num_format".to_string(), "dec".to_string());
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".to_string(),
        results: vec![SymbolMatch {
            name: "SOME_CONST".to_string(),
            node_kind: Some("number_literal".to_string()),
            fql_kind: Some("number".to_string()),
            language: None,
            path: Some(PathBuf::from("src/defs.h")),
            line: Some(10),
            usages_count: Some(3),
            fields,
            count: None,
            node_id: None,
            rev: None,
        }],
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    });
    let json = result.to_json();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    // Projected fields should be present.
    assert_eq!(v["results"][0]["name"], "SOME_CONST");
    assert_eq!(v["results"][0]["kind"], "number");
    // Raw HashMap keys must NOT appear.
    assert!(
        v["results"][0].get("value").is_none(),
        "value must not leak"
    );
    assert!(
        v["results"][0].get("num_format").is_none(),
        "num_format must not leak"
    );
    assert!(
        v["results"][0].get("fields").is_none(),
        "fields must not leak"
    );
}

#[test]
fn json_count_group_by_includes_count() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "count_usages".to_string(),
        results: vec![SymbolMatch {
            name: "src/signal.cpp".to_string(),
            node_kind: None,
            fql_kind: None,
            language: None,
            path: None,
            line: None,
            usages_count: None,
            fields: HashMap::new(),
            count: Some(4),
            node_id: None,
            rev: None,
        }],
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    });
    let json = result.to_json();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["results"][0]["count"], 4);
}

#[test]
fn json_non_query_result_uses_serde() {
    let result = ForgeQLResult::Mutation(MutationResult {
        op: "rename_symbol".to_string(),
        applied: true,
        files_changed: vec![],
        edit_count: 0,
        lines_written: 0,
        lines_removed: 0,
        diff: None,
        suggestions: vec![],
        new_node_id: None,
        new_rev: None,
        structural_errors: Vec::new(),
    });
    let output = result.to_json();
    // Must fall back to full serde JSON, not crash or return empty.
    assert!(output.contains("rename_symbol"), "fallback JSON: {output}");
    assert!(output.contains("applied"), "fallback JSON: {output}");
}
#[test]
fn show_result_round_trips_through_json() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_body".to_string(),
        symbol: Some("convertByte2Volts".to_string()),
        file: Some(PathBuf::from("src/adc.cpp")),
        start_line: Some(42),
        end_line: Some(44),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Lines {
            lines: vec![
                SourceLine {
                    rev: None,
                    line: 42,
                    text: "float convertByte2Volts(uint8_t raw) {".to_string(),
                    marker: None,
                    node_id: None,
                    node_offset: None,
                },
                SourceLine {
                    rev: None,
                    line: 43,
                    text: "    return raw * 3.3f / 255.0f;".to_string(),
                    marker: None,
                    node_id: None,
                    node_offset: None,
                },
            ],
            byte_start: Some(1024),
            depth: Some(1),
        },
    });

    let json_string = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

    match deserialized {
        ForgeQLResult::Show(show_result) => {
            assert_eq!(show_result.op, "show_body");
            assert_eq!(show_result.symbol.as_deref(), Some("convertByte2Volts"),);
            // Phase 4: start_line and end_line must round-trip.
            assert_eq!(
                show_result.start_line,
                Some(42),
                "start_line must round-trip"
            );
            assert_eq!(show_result.end_line, Some(44), "end_line must round-trip");
        }
        other => panic!("expected Show variant, got: {other:?}"),
    }
}

#[test]
fn mutation_result_round_trips_through_json() {
    let result = ForgeQLResult::Mutation(MutationResult {
        op: "rename_symbol".to_string(),
        applied: true,
        files_changed: vec![
            PathBuf::from("src/signal_controller.cpp"),
            PathBuf::from("include/signal_controller.hpp"),
        ],
        edit_count: 5,
        lines_written: 0,
        lines_removed: 0,
        diff: None,
        suggestions: vec![SuggestionEntry {
            path: PathBuf::from("src/signal_controller.cpp"),
            byte_offset: 2048,
            snippet: r#"[[deprecated("Use setMaxIntensity()")]]"#.to_string(),
            reason: "deprecated_attribute".to_string(),
        }],
        new_node_id: None,
        new_rev: None,
        structural_errors: Vec::new(),
    });

    let json_string = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

    match deserialized {
        ForgeQLResult::Mutation(mutation_result) => {
            assert!(mutation_result.applied);
            assert_eq!(mutation_result.files_changed.len(), 2);
            assert_eq!(mutation_result.suggestions.len(), 1);
        }
        other => panic!("expected Mutation variant, got: {other:?}"),
    }
}

#[test]
fn source_op_result_round_trips_through_json() {
    let result = ForgeQLResult::SourceOp(SourceOpResult {
        op: "use_source".to_string(),
        source_name: Some("pisco-code".to_string()),
        session_id: Some("my-session".to_string()), // alias-style: equals the AS 'alias'
        branches: vec!["main".to_string(), "develop".to_string()],
        symbols_indexed: Some(668),
        resumed: false,
        base_commit: None,
        message: None,
    });

    let json_string = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

    match deserialized {
        ForgeQLResult::SourceOp(source_result) => {
            assert_eq!(source_result.op, "use_source");
            assert_eq!(source_result.symbols_indexed, Some(668));
            assert!(!source_result.resumed);
        }
        other => panic!("expected SourceOp variant, got: {other:?}"),
    }
}

#[test]
fn begin_transaction_result_round_trips_through_json() {
    let result = ForgeQLResult::BeginTransaction(BeginTransactionResult {
        name: "rename-signal-api".to_string(),
        checkpoint_oid: "abc123def456".to_string(),
    });

    let json_string = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

    match deserialized {
        ForgeQLResult::BeginTransaction(bt) => {
            assert_eq!(bt.name, "rename-signal-api");
            assert_eq!(bt.checkpoint_oid, "abc123def456");
        }
        other => panic!("expected BeginTransaction variant, got: {other:?}"),
    }
}

#[test]
fn commit_result_round_trips_through_json() {
    let result = ForgeQLResult::Commit(CommitResult {
        message: "Rename signal controller API".to_string(),
        commit_hash: "abc123def456".to_string(),
    });

    let json_string = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

    match deserialized {
        ForgeQLResult::Commit(c) => {
            assert_eq!(c.message, "Rename signal controller API");
            assert_eq!(c.commit_hash, "abc123def456");
        }
        other => panic!("expected Commit variant, got: {other:?}"),
    }
}

#[test]
fn plan_result_round_trips_through_json() {
    let result = ForgeQLResult::Plan(PlanResult {
        op: "dry_run".to_string(),
        diff: "--- a/src/signal.cpp\n+++ b/src/signal.cpp\n".to_string(),
        file_edits: vec![FileEditSummary {
            path: PathBuf::from("src/signal.cpp"),
            edit_count: 3,
        }],
        suggestions: vec![],
    });

    let json_string = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

    match deserialized {
        ForgeQLResult::Plan(plan_result) => {
            assert_eq!(plan_result.op, "dry_run");
            assert_eq!(plan_result.file_edits.len(), 1);
            assert_eq!(plan_result.file_edits[0].edit_count, 3);
        }
        other => panic!("expected Plan variant, got: {other:?}"),
    }
}

#[test]
fn display_query_result_empty() {
    let result = QueryResult {
        op: "find_symbols".to_string(),
        results: vec![],
        total: 0,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    };
    let output = format!("{result}");
    assert!(output.contains("No results"));
}

#[test]
fn display_query_result_with_items() {
    let result = QueryResult {
        op: "find_symbols".to_string(),
        results: vec![SymbolMatch {
            name: "setPeakLevel".to_string(),
            node_kind: None,
            fql_kind: Some("function".to_string()),
            language: None,
            path: Some(PathBuf::from("src/signal.cpp")),
            line: Some(42),
            usages_count: Some(3),
            fields: HashMap::new(),
            count: None,
            node_id: None,
            rev: None,
        }],
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    };
    let output = format!("{result}");
    assert!(output.contains("setPeakLevel"));
    assert!(output.contains("function"));
    assert!(output.contains("src/signal.cpp:42"));
    assert!(output.contains("usages: 3"));
}

#[test]
fn display_query_result_shows_enclosing_fn() {
    let mut fields = HashMap::new();
    fields.insert("enclosing_fn".to_string(), "traverse_trees".to_string());
    let result = QueryResult {
        op: "find_symbols".to_string(),
        results: vec![SymbolMatch {
            name: "(a&&(b||c))".to_string(),
            node_kind: None,
            fql_kind: Some("if".to_string()),
            language: None,
            path: Some(PathBuf::from("tree-walk.c")),
            line: Some(899),
            usages_count: Some(0),
            fields,
            count: None,
            node_id: None,
            rev: None,
        }],
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    };
    let output = format!("{result}");
    assert!(output.contains("via traverse_trees"));
    assert!(output.contains("tree-walk.c:899"));
}

#[test]
fn display_query_result_shows_truncation_notice() {
    let result = QueryResult {
        op: "find_symbols".to_string(),
        results: vec![SymbolMatch {
            name: "foo".to_string(),
            node_kind: None,
            fql_kind: None,
            language: None,
            path: None,
            line: None,
            usages_count: None,
            fields: HashMap::new(),
            count: None,
            node_id: None,
            rev: None,
        }],
        total: 100,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    };
    let output = format!("{result}");
    assert!(output.contains("1 of 100 shown"));
}

#[test]
fn display_mutation_result_applied() {
    let result = MutationResult {
        op: "rename_symbol".to_string(),
        applied: true,
        files_changed: vec![PathBuf::from("src/main.cpp")],
        edit_count: 4,
        lines_written: 0,
        lines_removed: 0,
        diff: None,
        suggestions: vec![],
        new_node_id: None,
        new_rev: None,
        structural_errors: Vec::new(),
    };
    let output = format!("{result}");
    assert!(output.contains("Applied"));
    assert!(output.contains("4 edit(s)"));
    assert!(output.contains("1 file(s)"));
}

#[test]
fn display_plan_result() {
    let result = PlanResult {
        op: "dry_run".to_string(),
        diff: "--- a/test.cpp\n+++ b/test.cpp\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        file_edits: vec![FileEditSummary {
            path: PathBuf::from("test.cpp"),
            edit_count: 1,
        }],
        suggestions: vec![],
    };
    let output = format!("{result}");
    assert!(output.contains("1 edit(s)"));
    assert!(output.contains("test.cpp"));
    assert!(output.contains("-old"));
    assert!(output.contains("+new"));
}

// ------------------------------------------------------------------
// source_lines_count
// ------------------------------------------------------------------

fn make_lines_result(n: usize) -> ForgeQLResult {
    ForgeQLResult::Show(ShowResult {
        op: "show_lines".to_string(),
        symbol: None,
        file: Some(PathBuf::from("src/foo.cpp")),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Lines {
            lines: (1..=n)
                .map(|i| SourceLine {
                    rev: None,
                    line: i,
                    text: format!("line {i}"),
                    marker: None,
                    node_id: None,
                    node_offset: None,
                })
                .collect(),
            byte_start: None,
            depth: None,
        },
        start_line: Some(1),
        end_line: Some(n),
    })
}

#[test]
fn source_lines_count_zero_for_empty_lines_vec() {
    assert_eq!(make_lines_result(0).source_lines_count(), 0);
}

#[test]
fn source_lines_count_matches_lines_vec_length() {
    assert_eq!(make_lines_result(1).source_lines_count(), 1);
    assert_eq!(make_lines_result(42).source_lines_count(), 42);
    assert_eq!(make_lines_result(70).source_lines_count(), 70);
}

#[test]
fn output_capped_is_true_only_above_the_cap() {
    // A read whose source-line count exceeds the cap would be windowed for
    // SHOW MORE, so the coach must observe it as capped at execute time —
    // before, and regardless of, whichever transport attaches the footer.
    assert!(!make_lines_result(40).output_capped(40));
    assert!(make_lines_result(41).output_capped(40));
    assert!(!make_lines_result(39).output_capped(40));
    // A non-line result is never a capped source read.
    let q = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".to_string(),
        results: vec![],
        total: 0,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    });
    assert!(!q.output_capped(0));
}

#[test]
fn source_lines_count_increases_with_more_lines() {
    // Simulates SHOW BODY DEPTH 1 (10 lines) vs DEPTH 2 (13 lines).
    assert!(
        make_lines_result(13).source_lines_count() > make_lines_result(10).source_lines_count()
    );
}

#[test]
fn source_lines_count_zero_for_query_result() {
    let r = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".to_string(),
        results: vec![],
        total: 0,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
    });
    assert_eq!(r.source_lines_count(), 0);
}

#[test]
fn source_lines_count_zero_for_show_members() {
    let r = ForgeQLResult::Show(ShowResult {
        op: "show_members".to_string(),
        symbol: Some("MyClass".to_string()),
        file: None,
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Members {
            members: vec![MemberEntry {
                fql_kind: "field".to_string(),
                text: "int x;".to_string(),
                line: 1,
                node_id: None,
                rev: None,
            }],
            byte_start: 0,
        },
        start_line: None,
        end_line: None,
    });
    assert_eq!(r.source_lines_count(), 0);
}

#[test]
fn source_lines_count_zero_for_show_outline() {
    let r = ForgeQLResult::Show(ShowResult {
        op: "show_outline".to_string(),
        symbol: None,
        file: Some(PathBuf::from("src/foo.cpp")),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Outline { entries: vec![] },
        start_line: None,
        end_line: None,
    });
    assert_eq!(r.source_lines_count(), 0);
}

#[test]
fn source_lines_count_zero_for_source_op_result() {
    let r = ForgeQLResult::SourceOp(SourceOpResult {
        op: "use_source".to_string(),
        source_name: None,
        session_id: Some("sid".to_string()),
        branches: vec![],
        symbols_indexed: None,
        resumed: false,
        base_commit: None,
        message: None,
    });
    assert_eq!(r.source_lines_count(), 0);
}
// -- compact_name edge cases -----------------------------------------

#[test]
fn compact_name_short_returned_as_is() {
    let name = "short_sym";
    let result = compact_name(name);
    assert_eq!(result.as_ref(), name);
}

#[test]
fn compact_name_exactly_120_chars_returned_as_is() {
    let name = "a".repeat(120);
    let result = compact_name(&name);
    assert_eq!(
        result.as_ref(),
        name.as_str(),
        "exactly 120 chars must not be truncated"
    );
}

#[test]
fn compact_name_121_chars_truncated_with_ellipsis() {
    let name = "b".repeat(121);
    let result = compact_name(&name);
    // First 120 bytes + "…" (U+2026, 3 bytes in UTF-8)
    let expected = format!("{}…", "b".repeat(120));
    assert_eq!(result.as_ref(), expected.as_str());
}

/// The three tests above truncate pure ASCII, where a character index and a
/// byte index are the same number — so they pass whether the cut is made in
/// characters or in bytes, and cannot see the difference. This one can: the
/// em dash straddles byte 120, so a byte slice there is not on a character
/// boundary and panics. `ForgeQL` indexes prose as well as code, and this
/// exact name — a line of its own documentation — took the process down.
#[test]
fn compact_name_cuts_on_a_character_not_a_byte() {
    let name = format!("{}—tail", "x".repeat(118));
    assert!(
        !name.is_char_boundary(120),
        "the em dash must straddle byte 120 for this test to mean anything"
    );
    let result = compact_name(&name);
    assert_eq!(
        result.chars().count(),
        121,
        "120 characters plus the ellipsis"
    );
    assert!(result.ends_with(char::from_u32(0x2026).expect("ellipsis")));
}

#[test]
fn compact_name_with_newline_returns_first_line_snippet() {
    let name = "line1\nline2";
    let result = compact_name(name);
    assert_eq!(result.as_ref(), "line1…");
}

#[test]
fn surface_block_alias_builds_block_handle_for_members() {
    // A row carrying block_ord/block_off surfaces as block_id(offset), reusing
    // the member's own segment prefix and swapping ordinal + offset.
    let fields = HashMap::from([
        ("block_ord".to_string(), "0007".to_string()),
        ("block_off".to_string(), "2".to_string()),
    ]);
    let row = SymbolMatch {
        node_id: Some("nabc123def456.0011".to_string()),
        fields,
        ..Default::default()
    };
    assert_eq!(
        surface_block_alias(&row).as_deref(),
        Some("nabc123def456.0007(2)"),
    );
}

#[test]
fn surface_block_alias_passes_through_non_members() {
    // A row without block fields keeps its own node id.
    let row = SymbolMatch {
        node_id: Some("nabc123def456.0011".to_string()),
        ..Default::default()
    };
    assert_eq!(
        surface_block_alias(&row).as_deref(),
        Some("nabc123def456.0011"),
    );
}

// -- ShowResult Display variants -------------------------------------

#[test]
fn display_show_result_lines_variant() {
    let result = ShowResult {
        op: "show_body".to_string(),
        symbol: Some("myFunc".to_string()),
        file: Some(PathBuf::from("src/lib.cpp")),
        content: ShowContent::Lines {
            lines: vec![SourceLine {
                rev: None,
                line: 10,
                text: "void myFunc() {}".to_string(),
                marker: None,
                node_id: None,
                node_offset: None,
            }],
            byte_start: None,
            depth: None,
        },
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
    };
    let output = format!("{result}");
    assert!(
        output.contains("--- myFunc ---"),
        "symbol header must appear"
    );
    assert!(output.contains("src/lib.cpp"), "file must appear");
    assert!(
        output.contains("void myFunc()"),
        "source line text must appear"
    );
    assert!(output.contains("10"), "line number must appear");
}

#[test]
fn display_show_result_signature_variant() {
    let result = ShowResult {
        op: "show_signature".to_string(),
        symbol: Some("myFunc".to_string()),
        file: None,
        content: ShowContent::Signature {
            signature: "void myFunc(int x)".to_string(),
            line: 42,
            byte_start: 0,
        },
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
    };
    let output = format!("{result}");
    assert!(output.contains("42"), "signature line number must appear");
    assert!(
        output.contains("void myFunc(int x)"),
        "signature text must appear"
    );
}

#[test]
fn display_show_result_outline_variant() {
    let result = ShowResult {
        op: "show_outline".to_string(),
        symbol: None,
        file: Some(PathBuf::from("src/api.h")),
        content: ShowContent::Outline {
            entries: vec![OutlineEntry {
                name: "ApiHandler".to_string(),
                fql_kind: "class".to_string(),
                path: PathBuf::from("src/api.h"),
                line: 5,
                node_id: None,
                rev: None,
                depth: 0,
            }],
        },
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
    };
    let output = format!("{result}");
    assert!(output.contains("ApiHandler"), "class name must appear");
    assert!(output.contains("class"), "fql_kind must appear");
    assert!(output.contains('5'), "line number must appear");
}

#[test]
fn display_show_result_members_variant() {
    let result = ShowResult {
        op: "show_members".to_string(),
        symbol: Some("Foo".to_string()),
        file: None,
        content: ShowContent::Members {
            members: vec![MemberEntry {
                fql_kind: "field".to_string(),
                text: "int count;".to_string(),
                line: 7,
                node_id: None,
                rev: None,
            }],
            byte_start: 0,
        },
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
    };
    let output = format!("{result}");
    assert!(output.contains("int count;"), "member text must appear");
    assert!(output.contains("field"), "member kind must appear");
}

#[test]
fn display_show_result_callgraph_variant() {
    let result = ShowResult {
        op: "show_callees".to_string(),
        symbol: Some("process".to_string()),
        file: None,
        content: ShowContent::CallGraph {
            direction: CallDirection::Callees,
            entries: vec![CallGraphEntry {
                name: "write_buf".to_string(),
                path: Some(PathBuf::from("src/io.cpp")),
                line: Some(88),
                byte_start: None,
            }],
        },
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
    };
    let output = format!("{result}");
    assert!(output.contains("write_buf"), "callee name must appear");
    assert!(output.contains("src/io.cpp"), "callee path must appear");
    assert!(output.contains("88"), "callee line must appear");
}

#[test]
fn display_show_result_filelist_variant() {
    let result = ShowResult {
        op: "find_files".to_string(),
        symbol: None,
        file: None,
        content: ShowContent::FileList {
            files: vec![FileEntry {
                path: PathBuf::from("src/main.cpp"),
                depth: None,
                extension: "cpp".to_string(),
                size: 1024,
                count: None,
                error_count: None,
                parse_coverage: None,
                node_id: None,
                rev: None,
            }],
            total: 1,
        },
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
    };
    let output = format!("{result}");
    assert!(output.contains("src/main.cpp"), "file path must appear");
    assert!(output.contains("(1 files)"), "total count must appear");
}

// -- RollbackResult Display ------------------------------------------

#[test]
fn display_rollback_result_contains_name_and_oid() {
    use crate::result::RollbackResult;
    let result = RollbackResult {
        name: "my-checkpoint".to_string(),
        reset_to_oid: "abc123def456".to_string(),
    };
    let output = format!("{result}");
    assert!(
        output.contains("my-checkpoint"),
        "checkpoint name must appear"
    );
    assert!(output.contains("abc123def456"), "OID must appear");
    assert!(output.contains("Rolled back"), "action label must appear");
}

/// The notice is one row of an answer, not a listing. A `FOUND` sweep can name
/// every file a formatter just rewrote, so the paths are capped and the rest
/// counted — and the sentence agrees with how many there were.
#[test]
fn the_reindex_notice_caps_the_paths_it_names() {
    let many: Vec<std::path::PathBuf> = (0..9)
        .map(|i| std::path::PathBuf::from(format!("src/f{i}.rs")))
        .collect();
    let notice = ForgeQLResult::reindexed_outside_forgeql_notice(&many);
    assert!(notice.contains("src/f4.rs"), "{notice}");
    assert!(!notice.contains("src/f5.rs"), "{notice}");
    assert!(notice.contains("and 4 more"), "{notice}");
    assert!(notice.contains("they changed"), "{notice}");

    let one = [std::path::PathBuf::from("src/only.rs")];
    let notice = ForgeQLResult::reindexed_outside_forgeql_notice(&one);
    assert!(notice.contains("src/only.rs"), "{notice}");
    assert!(notice.contains("the file changed"), "{notice}");
    assert!(!notice.contains(" more"), "{notice}");

    // A refusal carries no lines and no revs, so its notice must not claim
    // they are current — only that the re-index happened before the refusal.
    let refused = ForgeQLResult::reindexed_before_refusal_notice(&one);
    assert!(refused.contains("outside ForgeQL"), "{refused}");
    assert!(refused.contains("src/only.rs"), "{refused}");
    assert!(
        !refused.contains("in this answer are current"),
        "a refusal has no lines or revs to vouch for: {refused}"
    );
}

/// A row answering a stamp-only field's DEFAULT renders that value, not the
/// column that follows it.
///
/// `has_todo` is written onto a row only when it holds, so a row answering
/// `has_todo = 'false'` carries no such field. The metric column used to read
/// that absence as "no value" and fall through to `usages`, which put a usage
/// count under a `has_todo` header — `0` and `3` for two rows whose real
/// answer was `false` both times.
///
/// The two rows here differ ONLY in usages, so a fix that still fell through
/// would print two different values where the answer is the same; and asserting
/// against `usages` rather than a literal is what keeps this from passing on a
/// row whose usage count happened to equal the value.
#[test]
fn a_stamp_only_default_renders_its_value_and_not_the_usage_count() {
    let make = |usages: usize| SymbolMatch {
        name: "f".to_string(),
        node_kind: None,
        fql_kind: Some("function".to_string()),
        language: Some("rust".to_string()),
        path: Some(PathBuf::from("src/lib.rs")),
        line: Some(1),
        usages_count: Some(usages),
        fields: HashMap::new(),
        count: None,
        node_id: None,
        rev: None,
    };
    let ctx = QueryContext {
        metric_hint: Some("has_todo"),
        group_by_field: None,
    };
    for usages in [0_usize, 3] {
        let row = SymbolRow::from_match_with_ctx(&make(usages), &ctx);
        assert_eq!(
            row.metric_str(),
            "false",
            "a row answering the default must render it, not its {usages} usages",
        );
        assert_ne!(
            row.metric_str(),
            usages.to_string(),
            "the metric column fell through to `usages` under a `has_todo` header",
        );
    }
}

/// Build a symbol row of the given kind, carrying `scope` when one is given.
fn row_with_scope(kind: &str, scope: Option<&str>, usages: usize) -> SymbolMatch {
    let mut fields = HashMap::new();
    if let Some(scope) = scope {
        let _ = fields.insert("scope".to_string(), scope.to_string());
    }
    SymbolMatch {
        name: "session_id".to_string(),
        node_kind: None,
        fql_kind: Some(kind.to_string()),
        language: Some("rust".to_string()),
        path: Some(PathBuf::from("src/lib.rs")),
        line: Some(1),
        usages_count: Some(usages),
        fields,
        count: None,
        node_id: None,
        rev: None,
    }
}

#[test]
fn only_a_local_scope_variable_loses_its_usage_count() {
    // The boundary is `fql_kind = 'variable'` AND `scope = 'local'`, and each
    // half of that conjunction is load-bearing. Keyed on scope alone, the rule
    // would take any row an enricher marks local — the scope enricher writes
    // `local` on declaration nodes inside a function body, not only on
    // variables. Keyed on kind alone, it would take file-scope variables,
    // whose names DO identify them across the workspace and whose counts are
    // real. A variable row with no `scope` at all keeps its count on purpose:
    // the enricher writes the field only on nodes its language declares a
    // declaration kind, so an unexamined row is not a row known to be local.
    let cases = [
        (("variable", Some("local")), None, "a local variable"),
        (("variable", Some("file")), Some(7), "a file-scope variable"),
        (("variable", None), Some(7), "a variable carrying no scope"),
        (("function", Some("local")), Some(7), "a function"),
        (("field", Some("local")), Some(7), "a struct field"),
    ];
    for ((kind, scope), expected, what) in cases {
        let mut row = row_with_scope(kind, scope, 7);
        row.drop_meaningless_usage_count();
        assert_eq!(row.usages_count, expected, "{what} was judged wrongly");
    }
}

#[test]
fn a_row_with_no_usage_count_renders_an_empty_metric_column() {
    // The column's header names `usages`; a row that carries none must leave
    // the cell empty rather than print `0`, which reads as "referenced
    // nowhere" — a fact the row does not hold. This is the same fall-through
    // that once rendered a stamp-only default's row as the usage count under
    // its own field's header, one step further down the chain.
    let ctx = QueryContext {
        metric_hint: None,
        group_by_field: None,
    };
    let mut local = row_with_scope("variable", Some("local"), 370);
    local.drop_meaningless_usage_count();
    let rendered = SymbolRow::from_match_with_ctx(&local, &ctx);
    assert_eq!(
        rendered.metric_str(),
        "",
        "a row with no usage count rendered a number under the `usages` header",
    );

    // The control: a row that HAS a count still renders it.
    let counted = row_with_scope("function", Some("local"), 370);
    let rendered = SymbolRow::from_match_with_ctx(&counted, &ctx);
    assert_eq!(rendered.metric_str(), "370");
}

/// A row the declaration does NOT speak for renders no value rather than a
/// number belonging to another field.
///
/// `has_todo` is declared for the languages whose configs name a comment kind.
/// A `cmake` function is examined by no comment-scanning enricher, so nothing
/// answers the field for it — stamping `false` there would claim a row the
/// corpus arithmetic deliberately excludes, and falling through to `usages`
/// would relabel a usage count. Neither is right; the honest column is empty.
#[test]
fn a_row_outside_the_declared_languages_renders_no_metric_value() {
    let row = SymbolRow::from_match_with_ctx(
        &SymbolMatch {
            name: "f".to_string(),
            node_kind: None,
            fql_kind: Some("function".to_string()),
            language: Some("cmake".to_string()),
            path: Some(PathBuf::from("CMakeLists.txt")),
            line: Some(1),
            usages_count: Some(7),
            fields: HashMap::new(),
            count: None,
            node_id: None,
            rev: None,
        },
        &QueryContext {
            metric_hint: Some("has_todo"),
            group_by_field: None,
        },
    );
    assert_eq!(
        row.metric_str(),
        "",
        "nothing answers has_todo for a cmake row; it must not borrow `usages`",
    );
}
