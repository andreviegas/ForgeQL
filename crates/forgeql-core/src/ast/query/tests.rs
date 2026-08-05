//! Unit tests for read-only query execution: `LIKE` and glob pattern matching,
//! the `FIND symbols` / `FIND usages` lookups, and the file-grouping helpers
//! behind directory-depth collapsing.

use std::collections::HashMap;
use std::path::Path;

use super::*;

fn table_with_symbols(names: &[&str]) -> SymbolTable {
    let mut table = SymbolTable::default();
    for (i, &name) in names.iter().enumerate() {
        let start = i * 20;
        table.push_row_strings(
            name,
            "function_definition",
            "",
            "",
            Path::new("src/main.cpp"),
            start..start + 10,
            i + 1,
            HashMap::new(),
        );
    }
    table
}

// --- find_symbols_like -----------------------------------------------

#[test]
fn like_suffix_wildcard_matches_prefix() {
    let table = table_with_symbols(&["setPeakLevel", "setBaseLevel", "getPeakLevel"]);
    let results = find_symbols_like(&table, "set%");
    let names: Vec<&str> = results.iter().map(|r| table.name_of(r)).collect();
    assert!(names.contains(&"setPeakLevel"), "should match setPeakLevel");
    assert!(names.contains(&"setBaseLevel"), "should match setBaseLevel");
    assert!(
        !names.contains(&"getPeakLevel"),
        "should NOT match getPeakLevel"
    );
}

#[test]
fn like_exact_match_no_wildcard() {
    let table = table_with_symbols(&["showCode", "setPeakLevel"]);
    let results = find_symbols_like(&table, "showCode");
    assert_eq!(results.len(), 1);
    assert_eq!(table.name_of(results[0]), "showCode");
}

#[test]
fn like_no_match_returns_empty() {
    let table = table_with_symbols(&["setPeakLevel", "getPeakLevel"]);
    let results = find_symbols_like(&table, "show%");
    assert!(results.is_empty());
}

#[test]
fn like_case_insensitive() {
    let table = table_with_symbols(&["SetPeakLevel"]);
    let results = find_symbols_like(&table, "setpeak%");
    assert_eq!(results.len(), 1);
}

#[test]
fn like_percent_only_matches_all() {
    let table = table_with_symbols(&["foo", "bar", "baz"]);
    let results = find_symbols_like(&table, "%");
    assert_eq!(results.len(), 3);
}

#[test]
fn like_infix_wildcard_matches_substring() {
    let table = table_with_symbols(&["peak_level_", "base_level_", "repeat_count_"]);
    let results = find_symbols_like(&table, "%level%");
    let names: Vec<&str> = results.iter().map(|r| table.name_of(r)).collect();
    assert!(names.contains(&"peak_level_"));
    assert!(names.contains(&"base_level_"));
    assert!(!names.contains(&"repeat_count_"));
}

#[test]
fn like_underscore_matches_single_char() {
    let table = table_with_symbols(&["foo", "fao", "faoo"]);
    let results = find_symbols_like(&table, "f_o");
    let names: Vec<&str> = results.iter().map(|r| table.name_of(r)).collect();
    assert!(names.contains(&"foo"));
    assert!(names.contains(&"fao"));
    assert!(!names.contains(&"faoo"), "_ matches exactly one char");
}

/// `find_symbols_like` is a pure name scan.
/// Kind filtering is handled downstream via `apply_clauses` in the engine.
#[test]
fn find_symbols_like_returns_all_name_matches_regardless_of_kind() {
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "myFunc",
        "function_definition",
        "",
        "",
        Path::new("a.cpp"),
        0..6,
        1,
        HashMap::new(),
    );
    table.push_row_strings(
        "myVar",
        "declaration",
        "",
        "",
        Path::new("a.cpp"),
        10..15,
        2,
        HashMap::new(),
    );
    // Pure name scan: both rows match 'my%' regardless of node_kind.
    let results = find_symbols_like(&table, "my%");
    assert_eq!(results.len(), 2);
    let names: Vec<&str> = results.iter().map(|r| table.name_of(r)).collect();
    assert!(names.contains(&"myFunc"));
    assert!(names.contains(&"myVar"));
}

// --- path_glob header fallback ---------------------------------------

/// `find_symbols_like` is a pure name scan; path/glob filtering is
/// applied downstream via `apply_clauses` in the engine.
#[test]
fn find_symbols_like_returns_all_rows_on_wildcard() {
    let mut table = SymbolTable::default();

    // Function row with definition in a .cpp file.
    table.push_row_strings(
        "shouldTurnLedOn",
        "function_definition",
        "",
        "",
        Path::new("src/led_controller.cpp"),
        100..115,
        5,
        HashMap::new(),
    );

    // Header declaration shows up as a usage site.
    table.add_usage(
        "shouldTurnLedOn".into(),
        Path::new("include/led_controller.hpp"),
        50..65,
        2,
    );
    table.add_usage(
        "shouldTurnLedOn".into(),
        Path::new("src/led_controller.cpp"),
        100..115,
        5,
    );

    let all = find_symbols_like(&table, "%");
    assert_eq!(all.len(), 1);
}

#[test]
fn find_symbols_like_matches_by_name_not_path() {
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "processPacket",
        "function_definition",
        "",
        "",
        Path::new("src/net.cpp"),
        0..13,
        1,
        HashMap::new(),
    );
    // Pure name pattern '%' matches all rows regardless of path.
    let results = find_symbols_like(&table, "%");
    assert_eq!(results.len(), 1);
}

// --- glob_matches ----------------------------------------------------

#[test]
fn glob_double_star_matches_nested_paths() {
    use std::path::Path;
    assert!(glob_matches(Path::new("tests/unit/foo.cpp"), "tests/**"));
    // Leading `**` floats — matches `tests/` at any depth.
    assert!(glob_matches(
        Path::new("/data/worktrees/pisco/tests/foo.cpp"),
        "**/tests/**"
    ));
    assert!(!glob_matches(Path::new("src/foo.cpp"), "tests/**"));
    // Anchored pattern must NOT match a deeper path.
    assert!(!glob_matches(Path::new("extra/tests/foo.cpp"), "tests/**"));
}

#[test]
fn glob_star_matches_within_segment() {
    use std::path::Path;
    assert!(glob_matches(Path::new("src/foo.cpp"), "src/*.cpp"));
    assert!(!glob_matches(Path::new("src/sub/foo.cpp"), "src/*.cpp"));
}

#[test]
fn glob_question_matches_one_char() {
    use std::path::Path;
    assert!(glob_matches(Path::new("src/a.cpp"), "src/?.cpp"));
    assert!(!glob_matches(Path::new("src/ab.cpp"), "src/?.cpp"));
}

// --- find_usages -----------------------------------------------------

#[test]
fn find_usages_returns_correct_sites() {
    let mut table = SymbolTable::default();
    table.add_usage(
        "showCode".to_string(),
        Path::new("src/signal_emitter.cpp"),
        10..20,
        7,
    );
    let usages = find_usages(&table, "showCode");
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0].byte_range, 10..20);
}

#[test]
fn find_usages_missing_symbol_returns_empty_slice() {
    let table = SymbolTable::default();
    let usages = find_usages(&table, "doesNotExist");
    assert!(usages.is_empty());
}

// --- group_files_by_depth --------------------------------------------

fn file_entry(path: &str, size: u64) -> serde_json::Value {
    serde_json::json!({ "path": path, "size": size })
}

#[test]
fn group_files_by_depth_empty_input() {
    let result = group_files_by_depth(&[], 0);
    assert!(result.is_empty());
}

#[test]
fn group_files_by_depth_zero_collapses_all_subdirs() {
    let files = vec![
        file_entry("src/main.cpp", 100),
        file_entry("src/util/helper.cpp", 200),
        file_entry("src/util/math.cpp", 150),
        file_entry("src/net/socket.cpp", 300),
    ];
    let result = group_files_by_depth(&files, 0);

    let individual: Vec<_> = result.iter().filter(|v| v.get("kind").is_none()).collect();
    let summary_count = result
        .iter()
        .filter(|v| v.get("kind").is_some_and(|k| k == "directory_summary"))
        .count();

    assert_eq!(individual.len(), 1, "only main.cpp is at depth 0");
    assert_eq!(individual[0]["path"], "src/main.cpp");
    assert_eq!(summary_count, 1, "all deep files collapse into one dir");
    let summary = result
        .iter()
        .find(|v| v.get("kind").is_some_and(|k| k == "directory_summary"))
        .expect("summary must exist");
    assert_eq!(summary["path"], "src/");
    assert_eq!(summary["file_count"], 3);
    assert_eq!(summary["total_size"], 650);
}

#[test]
fn group_files_by_depth_one_shows_immediate_children() {
    let files = vec![
        file_entry("src/main.cpp", 100),
        file_entry("src/util/helper.cpp", 200),
        file_entry("src/util/deep/algo.cpp", 500),
    ];
    let result = group_files_by_depth(&files, 1);

    let individual_count = result.iter().filter(|v| v.get("kind").is_none()).count();
    let summary_count = result
        .iter()
        .filter(|v| v.get("kind").is_some_and(|k| k == "directory_summary"))
        .count();

    assert_eq!(individual_count, 2);
    assert_eq!(summary_count, 1);
    let summary = result
        .iter()
        .find(|v| v.get("kind").is_some_and(|k| k == "directory_summary"))
        .expect("summary must exist");
    assert_eq!(summary["file_count"], 1);
}

#[test]
fn group_files_by_depth_all_shallow_no_collapsing() {
    let files = vec![
        file_entry("src/a.cpp", 10),
        file_entry("src/b.cpp", 20),
        file_entry("src/c.cpp", 30),
    ];
    let result = group_files_by_depth(&files, 0);
    let summary_count = result
        .iter()
        .filter(|v| v.get("kind").is_some_and(|k| k == "directory_summary"))
        .count();
    assert_eq!(result.len(), 3);
    assert_eq!(summary_count, 0);
}

#[test]
fn common_prefix_depth_single_path() {
    assert_eq!(common_prefix_depth(&["src/main.cpp"]), 1);
}

#[test]
fn common_prefix_depth_shared_prefix() {
    assert_eq!(
        common_prefix_depth(&["src/a.cpp", "src/b.cpp", "src/sub/c.cpp"]),
        1
    );
}

#[test]
fn common_prefix_depth_no_common() {
    assert_eq!(common_prefix_depth(&["include/a.h", "src/b.cpp"]), 0);
}

#[test]
fn common_prefix_depth_empty() {
    assert_eq!(common_prefix_depth(&[]), 0);
}

// --- like_match ------------------------------------------------------

#[test]
fn like_match_basic_patterns() {
    assert!(like_match("setPeakLevel", "set%"));
    assert!(like_match("setPeakLevel", "%Peak%"));
    assert!(like_match("setPeakLevel", "%Level"));
    assert!(!like_match("setPeakLevel", "get%"));
    assert!(like_match("a", "_"));
    assert!(!like_match("ab", "_"));
    assert!(like_match("setPeakLevel", "%"));
}

#[test]
fn like_match_case_insensitive() {
    assert!(like_match("SetPeakLevel", "set%"));
    assert!(like_match("setPeakLevel", "SET%"));
}
