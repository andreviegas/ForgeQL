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
/// that match any of `exclude_globs` (e.g. `"tests/**"`).
///
/// Further filtering (WHERE predicates, ORDER BY, LIMIT, etc.) is handled
/// downstream by `apply_clauses` in the engine.
#[must_use]
pub fn find_usages_filtered<'a>(
    table: &'a SymbolTable,
    name: &str,
    exclude_globs: &[String],
) -> Vec<&'a UsageSite> {
    let sites = table.find_usages(name);
    sites
        .iter()
        .filter(|s| {
            let path = table.strings.paths.get(s.path_id);
            !exclude_globs.iter().any(|exc| glob_matches(path, exc))
        })
        .collect()
}

/// `FIND files IN 'glob' [EXCLUDE 'glob']` — enumerate workspace files.
///
/// Walks the workspace (respecting `.gitignore` / `.forgeql-ignore`) and
/// returns every regular file whose path matches `glob`.  When `exclude` is
/// supplied any path matching it is omitted.
#[must_use]
pub fn find_files(workspace: &Workspace, glob: &str, exclude: &[String]) -> Vec<serde_json::Value> {
    workspace
        .files()
        .filter(|p| !crate::result::FileEntry::is_runtime_artifact(p))
        .filter(|p| relative_glob_matches(p, glob, workspace.root()))
        .filter(|p| {
            !exclude
                .iter()
                .any(|ex| relative_glob_matches(p, ex, workspace.root()))
        })
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

/// `FIND files` — the directory rows.
///
/// Directories are addressable nodes too, so they are listed alongside files
/// and marked by a **trailing slash on `path`** (`src/`): no new column, and
/// `WHERE path LIKE '%/'` selects them with operators that already exist.
/// `size` is the number of direct children, which is the only size a directory
/// has. The glob is matched against the slash-less path, so `IN 'src/**'`
/// behaves the same for a directory as for a file under it.
#[must_use]
pub fn find_dirs(workspace: &Workspace, glob: &str, exclude: &[String]) -> Vec<serde_json::Value> {
    workspace
        .dirs()
        .into_iter()
        .filter(|p| relative_glob_matches(p, glob, workspace.root()))
        .filter(|p| {
            !exclude
                .iter()
                .any(|ex| relative_glob_matches(p, ex, workspace.root()))
        })
        .map(|p| {
            let children = std::fs::read_dir(&p).map(Iterator::count).unwrap_or(0);
            let path_str = workspace.relative(&p).display().to_string();
            serde_json::json!({
                "path":      format!("{path_str}/"),
                "size":      children,
                "extension": "",
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
            // "size" mirrors "total_size" so ORDER BY size works uniformly
            // across individual files and directory summary entries.
            "size":       total_size,
            // "count" is the uniform name a directory row carries everywhere;
            // "file_count" is kept for readers of the historical shape.
            "count":      count,
            "file_count": count,
            "total_size": total_size,
            "kind":       "directory_summary",
        }));
    }
    result
}

/// Find the number of common leading path segments across all paths.
pub(crate) fn common_prefix_depth(paths: &[&str]) -> usize {
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
mod tests;
