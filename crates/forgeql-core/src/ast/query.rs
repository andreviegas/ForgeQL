/// Read-only query execution against the `SymbolTable`.
///
/// `SELECT` and `FIND` `ForgeQL` statements compile to query calls here.
/// No files are written.
use crate::ast::index::{IndexRow, SymbolTable, UsageSite};
use crate::workspace::Workspace;

// -----------------------------------------------------------------------
// Query functions
// -----------------------------------------------------------------------

/// Find all indexed rows whose name matches a LIKE pattern (`%` = wildcard).
///
/// This is a pure name scan — no kind, path, or numeric filtering is applied
/// here.  The engine applies all additional filtering via `apply_clauses` on
/// the resulting `SymbolMatch` rows.
#[must_use]
pub fn find_symbols_like<'a>(table: &'a SymbolTable, pattern: &str) -> Vec<&'a IndexRow> {
    table
        .rows
        .iter()
        .filter(|r| like_match(table.name_of(r), pattern))
        .collect()
}

/// Find all usage sites of a specific symbol name.
#[must_use]
pub fn find_usages<'a>(table: &'a SymbolTable, name: &str) -> &'a [UsageSite] {
    table.find_usages(name)
}

/// Find all usage sites of a specific symbol name, optionally excluding files
/// that match `exclude_glob` (e.g. `"tests/**"`).
///
/// Further filtering (WHERE predicates, ORDER BY, LIMIT, etc.) is handled
/// downstream by `apply_clauses` in the engine.
#[must_use]
pub fn find_usages_filtered<'a>(
    table: &'a SymbolTable,
    name: &str,
    exclude_glob: Option<&str>,
) -> Vec<&'a UsageSite> {
    let sites = table.find_usages(name);
    exclude_glob.map_or_else(
        || sites.iter().collect(),
        |exc| {
            sites
                .iter()
                .filter(|s| !glob_matches(table.strings.paths.get(s.path_id), exc))
                .collect()
        },
    )
}

/// `FIND files IN 'glob' [EXCLUDE 'glob']` — enumerate workspace files.
///
/// Walks the workspace (respecting `.gitignore` / `.forgeql-ignore`) and
/// returns every regular file whose path matches `glob`.  When `exclude` is
/// supplied any path matching it is omitted.
#[must_use]
pub fn find_files(
    workspace: &Workspace,
    glob: &str,
    exclude: Option<&str>,
) -> Vec<serde_json::Value> {
    workspace
        .files()
        .filter(|p| relative_glob_matches(p, glob, workspace.root()))
        .filter(|p| exclude.is_none_or(|ex| !relative_glob_matches(p, ex, workspace.root())))
        .map(|p| {
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let path_str = workspace.relative(&p).display().to_string();
            serde_json::json!({
                "path":      path_str,
                "size":      size,
                "extension": ext,
            })
        })
        .collect()
}

/// Group flat file results by directory depth, collapsing sub-directories
/// deeper than `max_depth` into summary entries with file counts.
#[must_use]
pub fn group_files_by_depth(
    files: &[serde_json::Value],
    max_depth: usize,
) -> Vec<serde_json::Value> {
    use std::collections::BTreeMap;

    if files.is_empty() {
        return Vec::new();
    }

    let paths: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).collect();

    let prefix_depth = common_prefix_depth(&paths);

    let mut individual = Vec::new();
    let mut dir_counts: BTreeMap<String, (usize, u64)> = BTreeMap::new();

    for file in files {
        let path = file["path"].as_str().unwrap_or("");
        let size = file["size"].as_u64().unwrap_or(0);
        let segments: Vec<&str> = path.split('/').collect();
        let relative_depth = segments.len().saturating_sub(prefix_depth + 1);

        if relative_depth <= max_depth {
            individual.push(file.clone());
        } else {
            let dir_end = prefix_depth + max_depth;
            let dir = if dir_end < segments.len() {
                segments[..dir_end].join("/")
            } else {
                segments[..segments.len() - 1].join("/")
            };
            let entry = dir_counts.entry(dir).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += size;
        }
    }

    let mut result = individual;
    for (dir, (count, total_size)) in &dir_counts {
        result.push(serde_json::json!({
            "path":       format!("{dir}/"),
            "file_count": count,
            "total_size": total_size,
            "kind":       "directory_summary",
        }));
    }
    result
}

/// Find the number of common leading path segments across all paths.
fn common_prefix_depth(paths: &[&str]) -> usize {
    if paths.is_empty() {
        return 0;
    }
    let first_segs: Vec<&str> = paths[0].split('/').collect();
    let mut common = first_segs.len().saturating_sub(1);
    for path in &paths[1..] {
        let segs: Vec<&str> = path.split('/').collect();
        let file_segs = segs.len().saturating_sub(1);
        common = common.min(file_segs);
        for i in 0..common {
            if segs[i] != first_segs[i] {
                common = i;
                break;
            }
        }
    }
    common
}
/// Normalize a glob pattern so that bare directory paths match recursively.
///
/// If `pattern` looks like a plain directory path (no `*`, `?` wildcards,
/// and either ends with `/` or contains no `.` in its last segment), append
/// `/**` so `IN 'src'` and `IN 'crates/'` behave like `IN 'src/**'`.
fn normalize_glob(pattern: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = pattern.trim_end_matches('/');
    // Already contains wildcard characters — return as-is.
    if trimmed.contains('*') || trimmed.contains('?') {
        return std::borrow::Cow::Borrowed(pattern);
    }
    // If the pattern ends with `/`, it's clearly a directory.
    // If the last segment has no `.`, treat it as a directory too
    // (e.g. `src`, `crates/forgeql-core`).
    let last_seg = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if pattern.ends_with('/') || !last_seg.contains('.') {
        std::borrow::Cow::Owned(format!("{trimmed}/**"))
    } else {
        std::borrow::Cow::Borrowed(pattern)
    }
}

// -----------------------------------------------------------------------
// Glob path matching
// -----------------------------------------------------------------------

/// Match a file path against a glob pattern.
///
/// Supports `*`, `**`, and `?` wildcards.  Tries every suffix of `path`'s
/// segments so that relative patterns work against absolute worktree paths.
///
/// Bare directory paths are auto-normalized: `src` and `crates/` become
/// `src/**` and `crates/**` respectively.
#[must_use]
pub fn glob_matches(path: &std::path::Path, pattern: &str) -> bool {
    let pattern = normalize_glob(pattern);
    let path_str = path.to_string_lossy();
    let path_segs: Vec<&str> = path_str.split('/').filter(|s| !s.is_empty()).collect();
    let pattern_segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    // Only float (try every start position) when the pattern begins with `**`.
    // Otherwise anchor at the start — `kernel/**` must NOT match `tests/kernel/…`.
    if pattern_segs.first() == Some(&"**") {
        (0..=path_segs.len()).any(|start| match_segs(&path_segs[start..], &pattern_segs))
    } else {
        match_segs(&path_segs, &pattern_segs)
    }
}

/// Like [`glob_matches`] but strips `root` from `path` first, so that an
/// absolute worktree path can be matched against a relative pattern.
#[must_use]
pub fn relative_glob_matches(
    path: &std::path::Path,
    pattern: &str,
    root: &std::path::Path,
) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    glob_matches(relative, pattern)
}

fn match_segs(path: &[&str], pat: &[&str]) -> bool {
    match (path, pat) {
        ([], []) => true,
        ([], [p]) if *p == "**" => true,
        ([], _) | (_, []) => false,
        (_, [p, rest @ ..]) if *p == "**" => (0..=path.len()).any(|i| match_segs(&path[i..], rest)),
        ([ps, path_rest @ ..], [p, pattern_rest @ ..]) => {
            seg_glob(ps, p) && match_segs(path_rest, pattern_rest)
        }
    }
}

#[allow(clippy::many_single_char_names)]
fn seg_glob(seg: &str, pat: &str) -> bool {
    let s: Vec<char> = seg.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let (n, m) = (s.len(), p.len());
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[0][0] = true;
    for j in 1..=m {
        if p[j - 1] == '*' {
            dp[0][j] = dp[0][j - 1];
        }
    }
    for i in 1..=n {
        for j in 1..=m {
            dp[i][j] = match p[j - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && s[i - 1] == c,
            };
        }
    }
    dp[n][m]
}

// -----------------------------------------------------------------------
// LIKE pattern matching
// -----------------------------------------------------------------------

/// Case-insensitive SQL LIKE match.
///
/// - `%` matches any sequence of characters (including empty).
/// - `_` matches exactly one character.
#[must_use]
#[allow(clippy::indexing_slicing)]
pub fn like_match(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = name.to_ascii_lowercase().chars().collect();
    let pat: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let (n, p) = (name.len(), pat.len());

    let mut dp = vec![vec![false; p + 1]; n + 1];
    dp[0][0] = true;

    for j in 1..=p {
        if pat[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        }
    }

    for i in 1..=n {
        for j in 1..=p {
            dp[i][j] = match pat[j - 1] {
                '%' => dp[i - 1][j] || dp[i][j - 1],
                '_' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && name[i - 1] == c,
            };
        }
    }

    dp[n][p]
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
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
}
