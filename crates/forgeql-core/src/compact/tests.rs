use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::result::*;

// -- SHOW outline --------------------------------------------------

#[test]
fn outline_renders_pre_order_tree_with_depth() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_outline".into(),
        symbol: None,
        file: Some(PathBuf::from("include/types.hpp")),
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Outline {
            entries: vec![
                OutlineEntry {
                    name: "int16_t".into(),
                    fql_kind: "type_alias".into(),
                    path: PathBuf::from("include/types.hpp"),
                    line: 17,
                    node_id: None,
                    rev: None,
                    depth: 0,
                },
                OutlineEntry {
                    name: "int32_t".into(),
                    fql_kind: "type_alias".into(),
                    path: PathBuf::from("include/types.hpp"),
                    line: 18,
                    node_id: None,
                    rev: None,
                    depth: 0,
                },
                OutlineEntry {
                    name: "Pid".into(),
                    fql_kind: "class_specifier".into(),
                    path: PathBuf::from("include/types.hpp"),
                    line: 22,
                    node_id: None,
                    rev: None,
                    depth: 1,
                },
            ],
        },
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""show_outline","include/types.hpp""#);
    assert_eq!(lines[1], r#""depth","[fql_kind,name,line]""#);
    assert_eq!(lines[2], r#""0","[type_alias,int16_t,17]""#);
    assert_eq!(lines[3], r#""0","[type_alias,int32_t,18]""#);
    assert_eq!(lines[4], r#""1","[class_specifier,Pid,22]""#);
    assert_eq!(lines.len(), 5);
}

#[test]
fn outline_comment_renders_as_snippet() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_outline".into(),
        symbol: None,
        file: Some(PathBuf::from("src/adc.cpp")),
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Outline {
            entries: vec![
                OutlineEntry {
                    name: "// ADC conversion".into(),
                    fql_kind: "comment".into(),
                    path: PathBuf::from("src/adc.cpp"),
                    line: 1,
                    node_id: None,
                    rev: None,
                    depth: 0,
                },
                OutlineEntry {
                    name: "convertByte2Volts".into(),
                    fql_kind: "function_definition".into(),
                    path: PathBuf::from("src/adc.cpp"),
                    line: 5,
                    node_id: None,
                    rev: None,
                    depth: 0,
                },
            ],
        },
    });
    let csv = to_compact(&result);
    assert!(
        csv.contains("// ADC conversion"),
        "comment should render as a first-line snippet, got: {csv}"
    );
    assert!(
        !csv.contains("len:"),
        "comment should not render the opaque len:N placeholder, got: {csv}"
    );
}

// -- SHOW members --------------------------------------------------

#[test]
fn members_groups_by_kind() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_members".into(),
        symbol: Some("MotorControl".into()),
        file: Some(PathBuf::from("include/motor_control.hpp")),
        start_line: Some(20),
        end_line: Some(55),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Members {
            members: vec![
                MemberEntry {
                    fql_kind: "field".into(),
                    text: "uint16_t rpm_setpoint;".into(),
                    line: 28,
                    node_id: None,
                    rev: None,
                },
                MemberEntry {
                    fql_kind: "method".into(),
                    text: "void setRPM(uint16_t);".into(),
                    line: 35,
                    node_id: None,
                    rev: None,
                },
                MemberEntry {
                    fql_kind: "field".into(),
                    text: "bool is_locked;".into(),
                    line: 51,
                    node_id: None,
                    rev: None,
                },
            ],
            byte_start: 0,
        },
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        r#""show_members","MotorControl","include/motor_control.hpp""#
    );
    assert_eq!(lines[1], r#""type","[declaration,line]""#);
    assert_eq!(
        lines[2],
        r#""field","[uint16_t rpm_setpoint;,28],[bool is_locked;,51]""#
    );
    assert_eq!(lines[3], r#""method","[void setRPM(uint16_t);,35]""#);
}

// -- SHOW body / lines ---------------------------------------------

#[test]
fn body_lines_two_columns() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_body".into(),
        symbol: Some("convert".into()),
        file: Some(PathBuf::from("src/adc.cpp")),
        start_line: Some(42),
        end_line: Some(44),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Lines {
            lines: vec![
                SourceLine {
                    line: 42,
                    text: "float convert(uint8_t raw) {".into(),
                    marker: None,
                    node_id: None,
                    node_offset: None,
                },
                SourceLine {
                    line: 43,
                    text: "    return raw * 3.3f / 255.0f;".into(),
                    marker: None,
                    node_id: None,
                    node_offset: None,
                },
                SourceLine {
                    line: 44,
                    text: "}".into(),
                    marker: None,
                    node_id: None,
                    node_offset: None,
                },
            ],
            byte_start: Some(1024),
            depth: Some(1),
        },
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""show_body","convert","src/adc.cpp","42-44""#);
    assert_eq!(lines[1], r#""line","text""#);
    assert_eq!(lines[2], r#"42,"float convert(uint8_t raw) {""#);
    assert_eq!(lines[3], r#"43,"    return raw * 3.3f / 255.0f;""#);
    assert_eq!(lines[4], r#"44,"}""#);
    assert_eq!(lines.len(), 5);
}

// -- SHOW signature ------------------------------------------------

#[test]
fn signature_flat_row() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_signature".into(),
        symbol: Some("setPeakLevel".into()),
        file: Some(PathBuf::from("src/signal.cpp")),
        start_line: Some(125),
        end_line: Some(125),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Signature {
            signature: "void setPeakLevel(int level)".into(),
            line: 125,
            byte_start: 0,
        },
    });
    let csv = to_compact(&result);
    assert_eq!(
        csv,
        r#""show_signature","setPeakLevel","src/signal.cpp",125,"void setPeakLevel(int level)""#
    );
}

// -- SHOW callees --------------------------------------------------

#[test]
fn callgraph_groups_by_file() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "show_callees".into(),
        symbol: Some("setPWMDuty".into()),
        file: None,
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::CallGraph {
            direction: CallDirection::Callees,
            entries: vec![
                crate::result::CallGraphEntry {
                    name: "writePWM".into(),
                    path: Some(PathBuf::from("src/pwm_driver.cpp")),
                    line: Some(189),
                    byte_start: None,
                },
                crate::result::CallGraphEntry {
                    name: "updateTimer".into(),
                    path: Some(PathBuf::from("src/timer.cpp")),
                    line: Some(405),
                    byte_start: None,
                },
            ],
        },
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""show_callees","setPWMDuty""#);
    assert_eq!(lines[1], r#""file","[name,line]""#);
    assert_eq!(lines[2], r#""src/pwm_driver.cpp","[writePWM,189]""#);
    assert_eq!(lines[3], r#""src/timer.cpp","[updateTimer,405]""#);
}

// -- FIND files ----------------------------------------------------

#[test]
fn filelist_two_columns() {
    let result = ForgeQLResult::Show(ShowResult {
        op: "find_files".into(),
        symbol: None,
        file: None,
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::FileList {
            files: vec![
                FileEntry {
                    path: PathBuf::from("src/motor_control.cpp"),
                    depth: Some(1),
                    extension: "cpp".into(),
                    size: 12847,
                    count: None,
                    error_count: None,
                    parse_coverage: None,
                    node_id: None,
                    rev: None,
                },
                FileEntry {
                    path: PathBuf::from("include/motor_control.hpp"),
                    depth: Some(1),
                    extension: "hpp".into(),
                    size: 3421,
                    count: None,
                    error_count: None,
                    parse_coverage: None,
                    node_id: None,
                    rev: None,
                },
            ],
            total: 142,
        },
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""find_files",142"#);
    assert_eq!(lines[1], r#""path","size""#);
    assert_eq!(lines[2], r#""src/motor_control.cpp",12847"#);
    assert_eq!(lines[3], r#""include/motor_control.hpp",3421"#);
}

// -- FIND symbols --------------------------------------------------

#[test]
fn find_symbols_groups_by_kind() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".into(),
        total: 3,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
        results: vec![
            SymbolMatch {
                name: "encenderMotor".into(),
                node_kind: None,
                fql_kind: Some("function".into()),
                language: None,
                path: Some(PathBuf::from("src/motor_control.cpp")),
                line: None,
                usages_count: Some(7),
                fields: HashMap::new(),
                count: None,
                node_id: None,
                rev: None,
            },
            SymbolMatch {
                name: "apagarMotor".into(),
                node_kind: None,
                fql_kind: Some("function".into()),
                language: None,
                path: Some(PathBuf::from("src/motor_control.cpp")),
                line: None,
                usages_count: Some(5),
                fields: HashMap::new(),
                count: None,
                node_id: None,
                rev: None,
            },
            SymbolMatch {
                name: "MotorControl".into(),
                node_kind: None,
                fql_kind: Some("class".into()),
                language: None,
                path: Some(PathBuf::from("include/motor_control.hpp")),
                line: None,
                usages_count: Some(2),
                fields: HashMap::new(),
                count: None,
                node_id: None,
                rev: None,
            },
        ],
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""find_symbols",3"#);
    assert_eq!(lines[1], r#""fql_kind","[name,path,line,usages]""#);
    assert_eq!(
        lines[2],
        r#""function","[encenderMotor,src/motor_control.cpp,0,7],[apagarMotor,src/motor_control.cpp,0,5]""#
    );
    assert_eq!(
        lines[3],
        r#""class","[MotorControl,include/motor_control.hpp,0,2]""#
    );
}

#[test]
fn find_symbols_cf_rows_include_enclosing_fn() {
    let mut fields = HashMap::new();
    fields.insert("mixed_logic".to_string(), "true".to_string());
    fields.insert("enclosing_fn".to_string(), "traverse_trees".to_string());
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".into(),
        total: 1,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
        results: vec![SymbolMatch {
            name: "(a&&(b||c))".into(),
            node_kind: Some("if_statement".into()),
            fql_kind: Some("if".into()),
            language: None,
            path: Some(PathBuf::from("tree-walk.c")),
            line: Some(899),
            usages_count: Some(0),
            fields,
            count: None,
            node_id: None,
            rev: None,
        }],
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    // enclosing_fn present → schema extends to 5 columns.
    assert_eq!(
        lines[1],
        r#""fql_kind","[name,path,line,enclosing_fn,usages]""#
    );
    // Data row contains function name and line number.
    assert!(lines[2].contains("traverse_trees"));
    assert!(lines[2].contains("899"));
}

// -- FIND usages ---------------------------------------------------

#[test]
fn find_usages_groups_by_file() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_usages".into(),
        total: 3,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
        results: vec![
            SymbolMatch {
                name: "encenderMotor".into(),
                node_kind: Some("identifier".into()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("src/motor_control.cpp")),
                line: Some(45),
                usages_count: None,
                fields: HashMap::new(),
                count: None,
                node_id: None,
                rev: None,
            },
            SymbolMatch {
                name: "encenderMotor".into(),
                node_kind: Some("identifier".into()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("src/motor_control.cpp")),
                line: Some(89),
                usages_count: None,
                fields: HashMap::new(),
                count: None,
                node_id: None,
                rev: None,
            },
            SymbolMatch {
                name: "encenderMotor".into(),
                node_kind: Some("identifier".into()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("include/motor_control.hpp")),
                line: Some(34),
                usages_count: None,
                fields: HashMap::new(),
                count: None,
                node_id: None,
                rev: None,
            },
        ],
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""find_usages","encenderMotor",3"#);
    assert_eq!(lines[1], r#""file","[lines]""#);
    assert_eq!(lines[2], r#""src/motor_control.cpp","45,89""#);
    assert_eq!(lines[3], r#""include/motor_control.hpp","34""#);
}

// -- COUNT usages --------------------------------------------------

#[test]
fn count_usages_flat_rows() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "count_usages".into(),
        total: 2,
        metric_hint: None,
        group_by_field: None,
        found_rev: None,
        hint: None,
        results: vec![
            SymbolMatch {
                name: "src/signal.cpp".into(),
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
            },
            SymbolMatch {
                name: "src/main.cpp".into(),
                node_kind: None,
                fql_kind: None,
                language: None,
                path: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: Some(1),
                node_id: None,
                rev: None,
            },
        ],
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], r#""count_usages",2"#);
    assert_eq!(lines[1], r#""file","count""#);
    assert_eq!(lines[2], r#""src/signal.cpp",4"#);
    assert_eq!(lines[3], r#""src/main.cpp",1"#);
}

// -- Non-query/show falls back to JSON -----------------------------

// -- metric_hint overrides last column -----------------------------

#[test]
fn find_symbols_with_metric_hint_shows_field_value() {
    let result = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".into(),
        total: 2,
        metric_hint: Some("member_count".into()),
        group_by_field: None,
        found_rev: None,
        hint: None,
        results: vec![
            SymbolMatch {
                name: "Serial_Protocol".into(),
                node_kind: None,
                fql_kind: Some("class".into()),
                language: None,
                path: Some(PathBuf::from("src/Serial_Protocol.h")),
                line: Some(24),
                usages_count: Some(8),
                fields: HashMap::from([("member_count".into(), "17".into())]),
                count: None,
                node_id: None,
                rev: None,
            },
            SymbolMatch {
                name: "MpptState".into(),
                node_kind: None,
                fql_kind: Some("struct".into()),
                language: None,
                path: Some(PathBuf::from("src/SolarCharger.h")),
                line: Some(57),
                usages_count: Some(4),
                fields: HashMap::from([("member_count".into(), "12".into())]),
                count: None,
                node_id: None,
                rev: None,
            },
        ],
    });
    let csv = to_compact(&result);
    let lines: Vec<&str> = csv.lines().collect();
    // Schema hint must show the metric name, not "usages".
    assert_eq!(lines[1], r#""fql_kind","[name,path,line,member_count]""#);
    // Values must come from fields["member_count"], not usages_count.
    assert!(
        lines[2].contains(",17]"),
        "expected member_count=17 in output, got: {}",
        lines[2]
    );
    assert!(
        lines[3].contains(",12]"),
        "expected member_count=12 in output, got: {}",
        lines[3]
    );
}

#[test]
fn mutation_falls_back_to_json() {
    let result = ForgeQLResult::Mutation(MutationResult {
        op: "rename_symbol".into(),
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
    let output = to_compact(&result);
    assert!(output.contains("rename_symbol"));
    assert!(output.contains("applied"));
}

#[test]
fn compact_mutation_surfaces_lines_removed() {
    // The destructive-edit signal must appear in the DEFAULT (compact CSV)
    // output, not only in JSON: a signature-only CHANGE NODE that deletes a
    // function body reports a large lines_removed next to a small lines_written.
    let result = ForgeQLResult::Mutation(MutationResult {
        op: "change_content".into(),
        applied: true,
        files_changed: vec![],
        edit_count: 1,
        lines_written: 6,
        lines_removed: 26,
        diff: None,
        suggestions: vec![],
        new_node_id: None,
        new_rev: None,
        structural_errors: Vec::new(),
    });
    let output = to_compact(&result);
    assert!(output.contains("lines_removed"), "missing label: {output}");
    assert!(output.contains("26"), "missing value: {output}");
}
// -- Low-level CSV helpers -----------------------------------------

#[test]
fn q_empty_string() {
    assert_eq!(q(""), r#""""#);
}

#[test]
fn q_plain_string() {
    assert_eq!(q("hello"), r#""hello""#);
}

#[test]
fn q_embedded_double_quote() {
    // Input: say "hi"  →  escaped: say ""hi""  →  wrapped: "say ""hi"""
    assert_eq!(q("say \"hi\""), "\"say \"\"hi\"\"\"");
}

#[test]
fn q_only_double_quotes() {
    // Input: ""  →  escaped: """"  →  wrapped: """""" (6 quotes total)
    assert_eq!(q("\"\""), "\"\"\"\"\"\"");
}

#[test]
fn bracket_empty() {
    assert_eq!(bracket(&[]), "[]");
}

#[test]
fn bracket_single() {
    assert_eq!(bracket(&["a"]), "[a]");
}

#[test]
fn bracket_multiple() {
    assert_eq!(bracket(&["a", "b", "c"]), "[a,b,c]");
}

#[test]
fn row_basic_two_fields() {
    let mut out = String::new();
    row(&mut out, &["alpha", "beta"]);
    assert_eq!(out, "alpha,beta\n");
}

#[test]
fn row_single_field() {
    let mut out = String::new();
    row(&mut out, &["only"]);
    assert_eq!(out, "only\n");
}

#[test]
fn row_appends_to_existing_string() {
    let mut out = "first\n".to_string();
    row(&mut out, &["second"]);
    assert_eq!(out, "first\nsecond\n");
}

#[test]
fn chomp_removes_trailing_newline() {
    let mut s = "hello\n".to_string();
    chomp(&mut s);
    assert_eq!(s, "hello");
}

#[test]
fn chomp_no_newline_unchanged() {
    let mut s = "hello".to_string();
    chomp(&mut s);
    assert_eq!(s, "hello");
}

#[test]
fn chomp_empty_string_unchanged() {
    let mut s = String::new();
    chomp(&mut s);
    assert_eq!(s, "");
}

fn lines_result(op: &str, start: usize, len: usize) -> ShowResult {
    ShowResult {
        op: op.to_string(),
        symbol: Some("foo".to_string()),
        file: None,
        content: ShowContent::Lines {
            lines: Vec::new(),
            byte_start: None,
            depth: None,
        },
        start_line: Some(start),
        end_line: Some(start + len.saturating_sub(1)),
        total_lines: None,
        hint: None,
        metadata: None,
    }
}

#[test]
fn compact_lines_node_framed_drops_absolute_lines() {
    // SHOW body emits the node's id on its first line; the renderer then
    // shows 1-based node-relative offsets instead of absolute line numbers.
    let lines = vec![
        SourceLine {
            line: 10,
            text: "fn foo() {".to_string(),
            marker: None,
            node_id: Some("nabc123def456.0007".to_string()),
            node_offset: None,
        },
        SourceLine {
            line: 11,
            text: "    bar();".to_string(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
        SourceLine {
            line: 12,
            text: "}".to_string(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
    ];
    let s = lines_result("show_body", 10, lines.len());
    let out = compact_lines(&s, &lines);
    assert!(
        out.contains("nabc123def456.0007"),
        "header carries node_id: {out}"
    );
    assert!(
        out.contains("\"off\",\"text\""),
        "schema is off/text: {out}"
    );
    assert!(
        out.contains("1,\"fn foo() {\""),
        "offsets are 1-based: {out}"
    );
    assert!(out.contains("2,\"    bar();\""));
    assert!(out.contains("3,\"}\""));
    assert!(
        !out.contains("10,\"fn foo() {\""),
        "absolute lines dropped: {out}"
    );
}

#[test]
fn compact_lines_without_node_id_keeps_absolute_lines() {
    let lines = vec![
        SourceLine {
            line: 10,
            text: "a".to_string(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
        SourceLine {
            line: 11,
            text: "b".to_string(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
    ];
    let s = lines_result("show_lines", 10, lines.len());
    let out = compact_lines(&s, &lines);
    assert!(
        out.contains("\"line\",\"text\""),
        "schema stays line/text: {out}"
    );
    assert!(out.contains("10,\"a\""));
    assert!(out.contains("11,\"b\""));
}

#[test]
fn chomp_only_newline_becomes_empty() {
    let mut s = "\n".to_string();
    chomp(&mut s);
    assert_eq!(s, "");
}

#[test]
fn compact_lines_per_line_node_offsets_replace_absolute() {
    // SHOW LINES on a parsed file: each line carries its own innermost
    // containing node + a 1-based offset, so absolute line numbers give way
    // to a hoisted `n<hex>` prefix (header) plus per-row `node`/`off`.
    let lines = vec![
        SourceLine {
            line: 40,
            text: "    let x = 1;".to_string(),
            marker: None,
            node_id: Some("nabc123def456.0264".to_string()),
            node_offset: Some(1),
        },
        SourceLine {
            line: 41,
            text: "    if x > 0 {".to_string(),
            marker: None,
            node_id: Some("nabc123def456.0265".to_string()),
            node_offset: Some(1),
        },
        SourceLine {
            line: 42,
            text: "        log();".to_string(),
            marker: None,
            node_id: Some("nabc123def456.0265".to_string()),
            node_offset: Some(2),
        },
        // Gap line: no containing node (e.g. a top-level blank).
        SourceLine {
            line: 43,
            text: String::new(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
    ];
    let s = lines_result("show_lines", 40, lines.len());
    let out = compact_lines(&s, &lines);
    assert!(out.contains("nabc123def456"), "prefix hoisted: {out}");
    assert!(
        out.contains("\"node\",\"off\",\"text\""),
        "schema is node/off/text: {out}"
    );
    assert!(out.contains("\".0264\",\"1\""), "ordinal + offset: {out}");
    assert!(out.contains("\".0265\",\"1\""));
    assert!(out.contains("\".0265\",\"2\""));
    assert!(!out.contains("40,\""), "absolute lines dropped: {out}");
    assert!(
        out.contains("\"\",\"\",\"\""),
        "gap line blank handle: {out}"
    );
}
