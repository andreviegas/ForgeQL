/// Token-efficient compact output for `ForgeQL` results.
///
/// Produces a minimal CSV representation that deduplicates repeated fields
/// by grouping rows that share a key (e.g. `node_kind` for FIND symbols,
/// `path` for FIND usages).  The output is valid 2-column CSV with a
/// command header row, a schema-hint row, and grouped data rows.
///
/// # Format overview
///
/// ```text
/// "command","meta1","meta2"      ← command + context
/// "group_key","[field1,field2]"  ← schema hint (what values mean)
/// "kind_a","[v1,v2],[v3,v4]"    ← grouped data rows
/// ```
///
/// Result types that are already small (mutations, transactions, source ops)
/// fall back to `to_json()`.
use crate::result::{
    CallDirection, FileEntry, ForgeQLResult, MemberEntry, OutlineEntry, QueryResult, ShowContent,
    ShowResult, SourceLine, compact_name,
};

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// Produce a compact CSV representation of a `ForgeQLResult`.
///
/// Falls back to `to_json()` for result types that are already small
/// (mutations, transactions, source ops, etc.).
#[must_use]
pub fn to_compact(result: &ForgeQLResult) -> String {
    match result {
        ForgeQLResult::Query(q) => compact_query(q),
        ForgeQLResult::Show(s) => compact_show(s),
        // These are already small — keep JSON.
        _ => result.to_json(),
    }
}

// -----------------------------------------------------------------------
// CSV helpers
// -----------------------------------------------------------------------

/// Quote a string for CSV: wrap in `"` and escape internal `"` by doubling.
fn q(s: &str) -> String {
    let escaped = s.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Format a single `[v1,v2,…]` bracket group.
fn bracket(values: &[&str]) -> String {
    format!("[{}]", values.join(","))
}

/// Write a CSV row from pre-formatted fields (some may already be quoted).
fn row(out: &mut String, fields: &[&str]) {
    let line = fields.join(",");
    out.push_str(&line);
    out.push('\n');
}

// -----------------------------------------------------------------------
// SHOW results
// -----------------------------------------------------------------------

fn compact_show(s: &ShowResult) -> String {
    match &s.content {
        ShowContent::Lines { lines, .. } => compact_lines(s, lines),
        ShowContent::Signature {
            signature, line, ..
        } => compact_signature(s, *line, signature),
        ShowContent::Outline { entries } => compact_outline(s, entries),
        ShowContent::Members { members, .. } => compact_members(s, members),
        ShowContent::CallGraph {
            direction, entries, ..
        } => compact_callgraph(s, direction, entries),
        ShowContent::FileList { files, total } => compact_filelist(files, *total),
    }
}

/// SHOW body / SHOW lines / SHOW context → 2 columns: line, text.
fn compact_lines(s: &ShowResult, lines: &[SourceLine]) -> String {
    let mut out = String::with_capacity(lines.len() * 60);
    // Header: "show_body","symbol","file","start-end"
    let op = q(&s.op);
    let sym = s.symbol.as_deref().map_or_else(String::new, q);
    let file = s
        .file
        .as_ref()
        .map_or_else(String::new, |p| q(&p.to_string_lossy()));
    let span = match (s.start_line, s.end_line) {
        (Some(start), Some(end)) => q(&format!("{start}-{end}")),
        (Some(start), None) => q(&start.to_string()),
        _ => String::new(),
    };
    row(&mut out, &[&op, &sym, &file, &span]);
    // Schema hint.
    row(&mut out, &[&q("line"), &q("text")]);
    // Data rows.
    for line in lines {
        let lnum = line.line.to_string();
        let text = q(&line.text);
        row(&mut out, &[&lnum, &text]);
    }
    // Truncation hint (when implicit line cap fired).
    if let Some(ref hint) = s.hint {
        if let Some(total) = s.total_lines {
            row(
                &mut out,
                &[
                    &q("truncated"),
                    &q(&format!("{} of {total} lines", lines.len())),
                ],
            );
        }
        row(&mut out, &[&q("hint"), &q(hint)]);
    }
    chomp(&mut out);
    out
}

/// SHOW signature → single flat row.
fn compact_signature(s: &ShowResult, line: usize, signature: &str) -> String {
    let mut out = String::new();
    let op = q(&s.op);
    let sym = s.symbol.as_deref().map_or_else(String::new, q);
    let file = s
        .file
        .as_ref()
        .map_or_else(String::new, |p| q(&p.to_string_lossy()));
    let lnum = line.to_string();
    let sig = q(signature);
    row(&mut out, &[&op, &sym, &file, &lnum, &sig]);
    chomp(&mut out);
    out
}

/// SHOW outline → grouped by kind, 2 columns.
///
/// Comments are compressed to `len:N` (byte length of the comment text).
fn compact_outline(s: &ShowResult, entries: &[OutlineEntry]) -> String {
    let mut out = String::with_capacity(entries.len() * 40);
    // Header.
    let op = q(&s.op);
    let file = entries
        .first()
        .map_or(String::new(), |e| q(&e.path.to_string_lossy()));
    row(&mut out, &[&op, &file]);
    // Schema hint.
    row(&mut out, &[&q("kind"), &q("[name,line]")]);
    // Group by kind.
    let groups = group_outline(entries);
    for (kind, items) in &groups {
        let brackets: Vec<String> = items
            .iter()
            .map(|(name, line)| {
                let display_name = if kind == "comment" {
                    format!("len:{}", name.len())
                } else {
                    (*name).to_string()
                };
                bracket(&[&display_name, &line.to_string()])
            })
            .collect();
        let val = q(&brackets.join(","));
        row(&mut out, &[&q(kind), &val]);
    }
    chomp(&mut out);
    out
}

/// SHOW members → grouped by kind, 2 columns.
fn compact_members(s: &ShowResult, members: &[MemberEntry]) -> String {
    let mut out = String::with_capacity(members.len() * 50);
    // Header.
    let op = q(&s.op);
    let sym = s.symbol.as_deref().map_or_else(String::new, q);
    let file = s
        .file
        .as_ref()
        .map_or_else(String::new, |p| q(&p.to_string_lossy()));
    row(&mut out, &[&op, &sym, &file]);
    // Schema hint.
    row(&mut out, &[&q("type"), &q("[declaration,line]")]);
    // Group by kind.
    let groups = group_members(members);
    for (kind, items) in &groups {
        let brackets: Vec<String> = items
            .iter()
            .map(|(text, line)| bracket(&[text, &line.to_string()]))
            .collect();
        let val = q(&brackets.join(","));
        row(&mut out, &[&q(kind), &val]);
    }
    chomp(&mut out);
    out
}

/// SHOW callers / SHOW callees → grouped by file, 2 columns.
fn compact_callgraph(
    s: &ShowResult,
    direction: &CallDirection,
    entries: &[crate::result::CallGraphEntry],
) -> String {
    let mut out = String::with_capacity(entries.len() * 50);
    // Header.
    let op = match direction {
        CallDirection::Callers => q("show_callers"),
        CallDirection::Callees => q("show_callees"),
    };
    let sym = s.symbol.as_deref().map_or_else(String::new, q);
    row(&mut out, &[&op, &sym]);
    // Schema hint.
    row(&mut out, &[&q("file"), &q("[name,line]")]);
    // Group by file path.
    let groups = group_callgraph(entries);
    for (file, items) in &groups {
        let brackets: Vec<String> = items
            .iter()
            .map(|(name, line)| bracket(&[name, &line.to_string()]))
            .collect();
        let val = q(&brackets.join(","));
        row(&mut out, &[&q(file), &val]);
    }
    chomp(&mut out);
    out
}

/// FIND files → 2 flat columns: path, size.
fn compact_filelist(files: &[FileEntry], total: usize) -> String {
    let mut out = String::with_capacity(files.len() * 40);
    // Header.
    let tot = total.to_string();
    row(&mut out, &[&q("find_files"), &tot]);
    // Schema hint.
    row(&mut out, &[&q("path"), &q("size")]);
    // Data rows.
    for entry in files {
        let path = q(&entry.path.to_string_lossy());
        let size = entry.size.to_string();
        row(&mut out, &[&path, &size]);
    }
    chomp(&mut out);
    out
}

// -----------------------------------------------------------------------
// Query results
// -----------------------------------------------------------------------

fn compact_query(query: &QueryResult) -> String {
    match query.op.as_str() {
        "find_usages" => compact_find_usages(query),
        "count_usages" => compact_count_usages(query),
        _ => compact_find_grouped_by_kind(query),
    }
}

/// FIND usages → grouped by file, values are comma-separated line numbers.
fn compact_find_usages(query: &QueryResult) -> String {
    let mut out = String::with_capacity(query.results.len() * 30);
    // Header.
    let symbol = query.results.first().map_or("", |r| r.name.as_str());
    let tot = query.total.to_string();
    row(&mut out, &[&q("find_usages"), &q(symbol), &tot]);
    // Schema hint.
    row(&mut out, &[&q("file"), &q("[lines]")]);
    // Group by path.
    let groups = group_usages_by_file(query);
    for (file, lines) in &groups {
        let lines_str: Vec<String> = lines.iter().map(ToString::to_string).collect();
        let val = q(&lines_str.join(","));
        row(&mut out, &[&q(file), &val]);
    }
    chomp(&mut out);
    out
}

/// COUNT usages GROUP BY file → 2 flat columns: file, count.
fn compact_count_usages(query: &QueryResult) -> String {
    let mut out = String::with_capacity(query.results.len() * 40);
    // Header.
    let tot = query.total.to_string();
    row(&mut out, &[&q("count_usages"), &tot]);
    // Schema hint.
    row(&mut out, &[&q("file"), &q("count")]);
    // Data rows.
    for r in &query.results {
        let count = r
            .count
            .or(r.usages_count)
            .map_or(String::new(), |n| n.to_string());
        row(&mut out, &[&q(&r.name), &count]);
    }
    chomp(&mut out);
    out
}

/// FIND symbols / defines / enums / includes → grouped by `node_kind`.
fn compact_find_grouped_by_kind(query: &QueryResult) -> String {
    let mut out = String::with_capacity(query.results.len() * 50);
    // Header.
    let tot = query.total.to_string();
    row(&mut out, &[&q(&query.op), &tot]);
    // Schema hint — use metric name when a numeric WHERE/ORDER BY was used.
    let metric_label = query.metric_hint.as_deref().unwrap_or("usages");
    let schema = format!("[name,path,line,{metric_label}]");
    row(&mut out, &[&q("kind"), &q(&schema)]);
    // Group by node_kind.
    let groups = group_symbols_by_kind(query);
    for (kind, items) in &groups {
        let brackets: Vec<String> = items
            .iter()
            .map(|(name, path, line, val)| {
                bracket(&[name, path, &line.to_string(), &val.to_string()])
            })
            .collect();
        let val = q(&brackets.join(","));
        row(&mut out, &[&q(kind), &val]);
    }
    chomp(&mut out);
    out
}

// -----------------------------------------------------------------------
// Grouping helpers (preserve insertion order)
// -----------------------------------------------------------------------

/// Group outline entries by kind → Vec<(kind, Vec<(name, line)>)>.
fn group_outline(entries: &[OutlineEntry]) -> Vec<(String, Vec<(&str, usize)>)> {
    let mut groups: Vec<(String, Vec<(&str, usize)>)> = Vec::new();
    for e in entries {
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &e.kind) {
            g.1.push((&e.name, e.line));
        } else {
            groups.push((e.kind.clone(), vec![(&e.name, e.line)]));
        }
    }
    groups
}

/// Group member entries by kind → Vec<(kind, Vec<(text, line)>)>.
fn group_members(members: &[MemberEntry]) -> Vec<(String, Vec<(&str, usize)>)> {
    let mut groups: Vec<(String, Vec<(&str, usize)>)> = Vec::new();
    for m in members {
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &m.kind) {
            g.1.push((&m.text, m.line));
        } else {
            groups.push((m.kind.clone(), vec![(&m.text, m.line)]));
        }
    }
    groups
}

/// Group call graph entries by file → Vec<(file, Vec<(name, line)>)>.
fn group_callgraph(entries: &[crate::result::CallGraphEntry]) -> Vec<(String, Vec<(&str, usize)>)> {
    let mut groups: Vec<(String, Vec<(&str, usize)>)> = Vec::new();
    for e in entries {
        let file = e.path.as_ref().map_or("", |p| p.to_str().unwrap_or(""));
        let line = e.line.unwrap_or(0);
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == file) {
            g.1.push((&e.name, line));
        } else {
            groups.push((file.to_string(), vec![(&e.name, line)]));
        }
    }
    groups
}

/// Group usages by file path → Vec<(file, Vec<line>)>.
fn group_usages_by_file(query: &QueryResult) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for r in &query.results {
        let file = r
            .path
            .as_ref()
            .map_or(String::new(), |p| p.to_string_lossy().into_owned());
        let line = r.line.unwrap_or(0);
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &file) {
            g.1.push(line);
        } else {
            groups.push((file, vec![line]));
        }
    }
    groups
}

/// Group symbols by `node_kind`.
///
/// The last element of each tuple is the "metric value": when a
/// `metric_hint` is set on the query, the value comes from the row's
/// enrichment `fields`; otherwise it falls back to `usages_count`.
#[allow(clippy::type_complexity)]
fn group_symbols_by_kind(
    query: &QueryResult,
) -> Vec<(String, Vec<(String, String, usize, usize)>)> {
    let hint = query.metric_hint.as_deref();
    let mut groups: Vec<(String, Vec<(String, String, usize, usize)>)> = Vec::new();
    for r in &query.results {
        let kind = r.node_kind.as_deref().unwrap_or("");
        let path = r
            .path
            .as_ref()
            .map_or(String::new(), |p| p.to_string_lossy().into_owned());
        let line = r.line.unwrap_or(0);
        let metric = hint.map_or_else(
            || r.usages_count.or(r.count).unwrap_or(0),
            |field| {
                r.fields
                    .get(field)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0)
            },
        );
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == kind) {
            g.1.push((compact_name(&r.name).into_owned(), path, line, metric));
        } else {
            groups.push((
                kind.to_string(),
                vec![(compact_name(&r.name).into_owned(), path, line, metric)],
            ));
        }
    }
    groups
}

/// Remove trailing newline if present.
fn chomp(s: &mut String) {
    if s.ends_with('\n') {
        let _ = s.pop();
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::result::*;

    // -- SHOW outline --------------------------------------------------

    #[test]
    fn outline_groups_by_kind() {
        let result = ForgeQLResult::Show(ShowResult {
            op: "show_outline".into(),
            symbol: None,
            file: Some(PathBuf::from("include/types.hpp")),
            start_line: None,
            end_line: None,
            total_lines: None,
            hint: None,
            content: ShowContent::Outline {
                entries: vec![
                    OutlineEntry {
                        name: "int16_t".into(),
                        kind: "type_alias".into(),
                        path: PathBuf::from("include/types.hpp"),
                        line: 17,
                    },
                    OutlineEntry {
                        name: "int32_t".into(),
                        kind: "type_alias".into(),
                        path: PathBuf::from("include/types.hpp"),
                        line: 18,
                    },
                    OutlineEntry {
                        name: "Pid".into(),
                        kind: "class_specifier".into(),
                        path: PathBuf::from("include/types.hpp"),
                        line: 22,
                    },
                ],
            },
        });
        let csv = to_compact(&result);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], r#""show_outline","include/types.hpp""#);
        assert_eq!(lines[1], r#""kind","[name,line]""#);
        assert_eq!(lines[2], r#""type_alias","[int16_t,17],[int32_t,18]""#);
        assert_eq!(lines[3], r#""class_specifier","[Pid,22]""#);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn outline_comment_compressed_to_len() {
        let result = ForgeQLResult::Show(ShowResult {
            op: "show_outline".into(),
            symbol: None,
            file: Some(PathBuf::from("src/adc.cpp")),
            start_line: None,
            end_line: None,
            total_lines: None,
            hint: None,
            content: ShowContent::Outline {
                entries: vec![
                    OutlineEntry {
                        name: "// ADC conversion".into(),
                        kind: "comment".into(),
                        path: PathBuf::from("src/adc.cpp"),
                        line: 1,
                    },
                    OutlineEntry {
                        name: "convertByte2Volts".into(),
                        kind: "function_definition".into(),
                        path: PathBuf::from("src/adc.cpp"),
                        line: 5,
                    },
                ],
            },
        });
        let csv = to_compact(&result);
        assert!(
            csv.contains("len:17"),
            "comment should be compressed to len:N, got: {csv}"
        );
        assert!(
            !csv.contains("ADC conversion"),
            "comment text should not appear in compact output"
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
            content: ShowContent::Members {
                members: vec![
                    MemberEntry {
                        kind: "field".into(),
                        text: "uint16_t rpm_setpoint;".into(),
                        line: 28,
                    },
                    MemberEntry {
                        kind: "method".into(),
                        text: "void setRPM(uint16_t);".into(),
                        line: 35,
                    },
                    MemberEntry {
                        kind: "field".into(),
                        text: "bool is_locked;".into(),
                        line: 51,
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
            content: ShowContent::Lines {
                lines: vec![
                    SourceLine {
                        line: 42,
                        text: "float convert(uint8_t raw) {".into(),
                        marker: None,
                    },
                    SourceLine {
                        line: 43,
                        text: "    return raw * 3.3f / 255.0f;".into(),
                        marker: None,
                    },
                    SourceLine {
                        line: 44,
                        text: "}".into(),
                        marker: None,
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
            content: ShowContent::FileList {
                files: vec![
                    FileEntry {
                        path: PathBuf::from("src/motor_control.cpp"),
                        depth: Some(1),
                        extension: "cpp".into(),
                        size: 12847,
                        count: None,
                    },
                    FileEntry {
                        path: PathBuf::from("include/motor_control.hpp"),
                        depth: Some(1),
                        extension: "hpp".into(),
                        size: 3421,
                        count: None,
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
            results: vec![
                SymbolMatch {
                    name: "encenderMotor".into(),
                    node_kind: Some("function_definition".into()),
                    fql_kind: None,
                    language: None,
                    path: Some(PathBuf::from("src/motor_control.cpp")),
                    line: None,
                    usages_count: Some(7),
                    fields: HashMap::new(),
                    count: None,
                },
                SymbolMatch {
                    name: "apagarMotor".into(),
                    node_kind: Some("function_definition".into()),
                    fql_kind: None,
                    language: None,
                    path: Some(PathBuf::from("src/motor_control.cpp")),
                    line: None,
                    usages_count: Some(5),
                    fields: HashMap::new(),
                    count: None,
                },
                SymbolMatch {
                    name: "MotorControl".into(),
                    node_kind: Some("class_specifier".into()),
                    fql_kind: None,
                    language: None,
                    path: Some(PathBuf::from("include/motor_control.hpp")),
                    line: None,
                    usages_count: Some(2),
                    fields: HashMap::new(),
                    count: None,
                },
            ],
        });
        let csv = to_compact(&result);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], r#""find_symbols",3"#);
        assert_eq!(lines[1], r#""kind","[name,path,line,usages]""#);
        assert_eq!(
            lines[2],
            r#""function_definition","[encenderMotor,src/motor_control.cpp,0,7],[apagarMotor,src/motor_control.cpp,0,5]""#
        );
        assert_eq!(
            lines[3],
            r#""class_specifier","[MotorControl,include/motor_control.hpp,0,2]""#
        );
    }

    // -- FIND usages ---------------------------------------------------

    #[test]
    fn find_usages_groups_by_file() {
        let result = ForgeQLResult::Query(QueryResult {
            op: "find_usages".into(),
            total: 3,
            metric_hint: None,
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
            results: vec![
                SymbolMatch {
                    name: "Serial_Protocol".into(),
                    node_kind: Some("class_specifier".into()),
                    fql_kind: None,
                    language: None,
                    path: Some(PathBuf::from("src/Serial_Protocol.h")),
                    line: Some(24),
                    usages_count: Some(8),
                    fields: HashMap::from([("member_count".into(), "17".into())]),
                    count: None,
                },
                SymbolMatch {
                    name: "MpptState".into(),
                    node_kind: Some("struct_specifier".into()),
                    fql_kind: None,
                    language: None,
                    path: Some(PathBuf::from("src/SolarCharger.h")),
                    line: Some(57),
                    usages_count: Some(4),
                    fields: HashMap::from([("member_count".into(), "12".into())]),
                    count: None,
                },
            ],
        });
        let csv = to_compact(&result);
        let lines: Vec<&str> = csv.lines().collect();
        // Schema hint must show the metric name, not "usages".
        assert_eq!(lines[1], r#""kind","[name,path,line,member_count]""#);
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
            diff: None,
            suggestions: vec![],
        });
        let output = to_compact(&result);
        assert!(output.contains("rename_symbol"));
        assert!(output.contains("applied"));
    }
}
