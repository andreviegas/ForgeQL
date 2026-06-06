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
    CallDirection, FileEntry, FindNodeResult, ForgeQLResult, MemberEntry, MutationResult,
    OutlineEntry, QueryResult, SessionStats, ShowContent, ShowResult, SourceLine, SymbolRow,
    VerifyBuildResult,
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
        ForgeQLResult::FindNode(r) => compact_find_node(r),
        ForgeQLResult::Mutation(m) => compact_mutation(m),
        ForgeQLResult::VerifyBuild(v) => compact_verify(v),
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
        ShowContent::Stats { sessions } => compact_stats(sessions),
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
    // Node-framed rendering: when the shown lines belong to a single addressable
    // node (SHOW body emits the node's id on its first line), drop absolute line
    // numbers in favour of a 1-based node-relative `off`set, so the agent edits
    // with `CHANGE NODE 'id(off)'` / `'id(a-b)'`. The id is stated once in the
    // header. Falls back to absolute line numbers when no node frame is present
    // (SHOW LINES / SHOW context, or an unparsed symbol with no ordinal).
    let frame = lines
        .iter()
        .find_map(|line| line.node_id.clone())
        .zip(s.start_line);
    if let Some((node_id, start)) = frame {
        row(&mut out, &[&op, &sym, &file, &span, &q(&node_id)]);
        // Schema hint: `off` is the 1-based line offset within the node.
        row(&mut out, &[&q("off"), &q("text")]);
        for line in lines {
            let off = (line.line.saturating_sub(start) + 1).to_string();
            row(&mut out, &[&off, &q(&line.text)]);
        }
    } else {
        row(&mut out, &[&op, &sym, &file, &span]);
        // Schema hint.
        row(&mut out, &[&q("line"), &q("text")]);
        // Data rows.
        for line in lines {
            let lnum = line.line.to_string();
            let text = q(&line.text);
            row(&mut out, &[&lnum, &text]);
        }
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
    // DEPTH 0 metadata: enrichment fields (lines, param_count, etc.).
    if let Some(ref meta) = s.metadata {
        let pairs: Vec<String> = meta
            .iter()
            .map(|(k, v)| {
                let val = v.as_str().unwrap_or("");
                format!("{k}={val}")
            })
            .collect();
        if !pairs.is_empty() {
            row(&mut out, &[&q("metadata"), &q(&pairs.join(","))]);
        }
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
    // Include node_id only when at least one entry carries it (post-reindex).
    let has_node_id = entries.iter().any(|e| e.node_id.is_some());
    // Schema hint.
    let schema = if has_node_id {
        "[name,line,node_id]"
    } else {
        "[name,line]"
    };
    row(&mut out, &[&q("fql_kind"), &q(schema)]);
    // Group by fql_kind.
    let groups = group_outline(entries);
    for (kind, items) in &groups {
        let brackets: Vec<String> = items
            .iter()
            .map(|(name, line, node_id)| {
                let display_name = if kind == "comment" {
                    format!("len:{}", name.len())
                } else {
                    (*name).to_string()
                };
                if has_node_id {
                    bracket(&[&display_name, &line.to_string(), node_id.unwrap_or("")])
                } else {
                    bracket(&[&display_name, &line.to_string()])
                }
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

/// SHOW STATS → one section per session with memory and symbol breakdown.
#[allow(clippy::cast_precision_loss)]
fn compact_stats(sessions: &[SessionStats]) -> String {
    if sessions.is_empty() {
        return "\"show_stats\",\"no sessions loaded\"\n".to_string();
    }
    let mut out = String::with_capacity(sessions.len() * 512);
    row(
        &mut out,
        &[&q("show_stats"), &q(&sessions.len().to_string())],
    );
    for s in sessions {
        row(&mut out, &[&q("session"), &q(&s.session_id)]);
        row(&mut out, &[&q("source"), &q(&s.source)]);
        row(&mut out, &[&q("branch"), &q(&s.branch)]);
        row(&mut out, &[&q("rows"), &s.rows.to_string()]);
        row(
            &mut out,
            &[&q("distinct_names"), &s.distinct_names.to_string()],
        );
        row(
            &mut out,
            &[&q("distinct_paths"), &s.distinct_paths.to_string()],
        );
        row(
            &mut out,
            &[&q("usage_symbols"), &s.usage_symbols.to_string()],
        );
        row(&mut out, &[&q("usage_sites"), &s.usage_sites.to_string()]);
        row(
            &mut out,
            &[&q("trigram_distinct"), &s.trigram_distinct.to_string()],
        );
        row(
            &mut out,
            &[
                &q("mem_total_mb"),
                &format!("{:.1}", s.mem_total_bytes as f64 / 1_048_576.0),
            ],
        );
        row(
            &mut out,
            &[
                &q("mem_rows_mb"),
                &format!("{:.1}", s.mem_rows_bytes as f64 / 1_048_576.0),
            ],
        );
        row(
            &mut out,
            &[
                &q("mem_usages_mb"),
                &format!("{:.1}", s.mem_usages_bytes as f64 / 1_048_576.0),
            ],
        );
        row(
            &mut out,
            &[
                &q("mem_indexes_mb"),
                &format!("{:.1}", s.mem_indexes_bytes as f64 / 1_048_576.0),
            ],
        );
        row(
            &mut out,
            &[
                &q("mem_trigram_mb"),
                &format!("{:.1}", s.mem_trigram_bytes as f64 / 1_048_576.0),
            ],
        );
        row(
            &mut out,
            &[
                &q("mem_strings_mb"),
                &format!("{:.1}", s.mem_strings_bytes as f64 / 1_048_576.0),
            ],
        );
        // by_language — sorted for deterministic output
        let mut langs: Vec<(&String, &usize)> = s.by_language.iter().collect();
        langs.sort_by_key(|(k, _)| k.as_str());
        for (lang, count) in &langs {
            row(&mut out, &[&format!("lang:{lang}"), &count.to_string()]);
        }
        // by_fql_kind — sorted
        let mut kinds: Vec<(&String, &usize)> = s.by_fql_kind.iter().collect();
        kinds.sort_by_key(|(k, _)| k.as_str());
        for (kind, count) in &kinds {
            row(&mut out, &[&format!("kind:{kind}"), &count.to_string()]);
        }
    }
    chomp(&mut out);
    out
}

// -----------------------------------------------------------------------
// FIND NODE result
// -----------------------------------------------------------------------

fn compact_find_node(r: &FindNodeResult) -> String {
    let mut out = String::new();
    row(&mut out, &[&q("find_node"), &q(&r.node_id)]);
    row(
        &mut out,
        &[&q("fql_kind"), &q("[name,path,line,end_line,rev]")],
    );
    let data = bracket(&[
        &q(&r.name),
        &q(&r.path.to_string_lossy()),
        &r.line.to_string(),
        &r.end_line.to_string(),
        &q(&r.rev),
    ]);
    row(&mut out, &[&q(&r.fql_kind), &data]);
    // nav row
    let mut nav: Vec<String> = Vec::new();
    if let Some(p) = &r.parent_node_id {
        nav.push(format!("parent={p}"));
    } else {
        nav.push("parent=null".to_string());
    }
    if let Some(fc) = &r.first_child_node_id {
        nav.push(format!("first_child={fc}"));
    }
    if let Some(ns) = &r.next_sibling_node_id {
        nav.push(format!("next_sibling={ns}"));
    }
    if let Some(ps) = &r.prev_sibling_node_id {
        nav.push(format!("prev_sibling={ps}"));
    }
    row(
        &mut out,
        &[&q("node_nav"), &q(&r.node_id), &q(&nav.join(","))],
    );
    out
}
fn compact_mutation(r: &MutationResult) -> String {
    let mut out = String::new();
    row(&mut out, &[&q("mutation"), &q(&r.op)]);
    row(&mut out, &[&q("applied"), &q(&r.applied.to_string())]);
    let file_strs: Vec<String> = r
        .files_changed
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let file_refs: Vec<&str> = file_strs.iter().map(String::as_str).collect();
    row(&mut out, &[&q("files_changed"), &bracket(&file_refs)]);
    row(&mut out, &[&q("edit_count"), &r.edit_count.to_string()]);
    row(
        &mut out,
        &[&q("lines_written"), &r.lines_written.to_string()],
    );
    if let Some(ref id) = r.new_node_id {
        row(&mut out, &[&q("new_node_id"), &q(id)]);
    }
    if let Some(ref d) = r.diff {
        row(&mut out, &[&q("diff"), &q(d)]);
    }
    chomp(&mut out);
    out
}
// -----------------------------------------------------------------------
// VERIFY results
// -----------------------------------------------------------------------

/// VERIFY build → a one-line header followed by the raw command output.
///
/// The output is emitted verbatim (not CSV-quoted) so each log line maps to one
/// buffer line, letting `SHOW MORE TAIL n` / `SHOW MORE WHERE text MATCHES …`
/// window and grep the build log without re-running it.
fn compact_verify(v: &VerifyBuildResult) -> String {
    let verdict = if v.success { "PASS" } else { "FAIL" };
    let mut out = String::with_capacity(v.output.len() + 64);
    row(&mut out, &[&q("verify_build"), &q(&v.step), &q(verdict)]);
    out.push_str(v.output.trim_end_matches('\n'));
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

/// FIND symbols / defines / enums / includes → grouped by `fql_kind`.
fn compact_find_grouped_by_kind(query: &QueryResult) -> String {
    let mut out = String::with_capacity(query.results.len() * 50);
    let rows = query.projected_rows();
    // Header.
    let tot = query.total.to_string();
    row(&mut out, &[&q(&query.op), &tot]);
    // Schema hint — GROUP BY always displays the aggregated count (per-symbol
    // fields are meaningless for a group representative).  Without GROUP BY,
    // use the enrichment field name from metric_hint, else "usages".
    let is_grouped = rows.iter().any(|sr| sr.count.is_some());
    let metric_label = if is_grouped {
        "count"
    } else {
        query.metric_hint.as_deref().unwrap_or("usages")
    };
    // Include enclosing_fn in the schema when at least one result carries it.
    let has_enclosing_fn = rows.iter().any(|sr| sr.enclosing_fn.is_some());
    // Include node_id in the schema when at least one result carries it (post-reindex).
    let has_node_id = rows.iter().any(|sr| sr.node_id.is_some());

    // When GROUP BY uses a custom field (not fql_kind/file), show the group
    // key value as the row label instead of fql_kind.
    if let Some(ref group_field) = query.group_by_field {
        let schema = format!("[{metric_label}]");
        row(&mut out, &[&q(group_field), &q(&schema)]);
        // Group by the custom field value.
        let groups = group_rows_by_field(&rows);
        for (key, count) in &groups {
            row(&mut out, &[&q(key), &count.to_string()]);
        }
    } else {
        let schema = match (has_enclosing_fn, has_node_id) {
            (true, true) => format!("[name,path,line,enclosing_fn,node_id,{metric_label}]"),
            (true, false) => format!("[name,path,line,enclosing_fn,{metric_label}]"),
            (false, true) => format!("[name,path,line,node_id,{metric_label}]"),
            (false, false) => format!("[name,path,line,{metric_label}]"),
        };
        row(&mut out, &[&q("fql_kind"), &q(&schema)]);
        // Group by fql_kind.
        let groups = group_rows_by_kind(&rows);
        for (kind, items) in &groups {
            let brackets: Vec<String> = items
                .iter()
                .map(|sr| match (has_enclosing_fn, has_node_id) {
                    (true, true) => bracket(&[
                        &sr.name,
                        &sr.path,
                        &sr.line.to_string(),
                        sr.enclosing_fn.as_deref().unwrap_or(""),
                        sr.node_id.as_deref().unwrap_or(""),
                        &sr.metric_str(),
                    ]),
                    (true, false) => bracket(&[
                        &sr.name,
                        &sr.path,
                        &sr.line.to_string(),
                        sr.enclosing_fn.as_deref().unwrap_or(""),
                        &sr.metric_str(),
                    ]),
                    (false, true) => bracket(&[
                        &sr.name,
                        &sr.path,
                        &sr.line.to_string(),
                        sr.node_id.as_deref().unwrap_or(""),
                        &sr.metric_str(),
                    ]),
                    (false, false) => {
                        bracket(&[&sr.name, &sr.path, &sr.line.to_string(), &sr.metric_str()])
                    }
                })
                .collect();
            let val = q(&brackets.join(","));
            row(&mut out, &[&q(kind), &val]);
        }
    }
    chomp(&mut out);
    out
}

// -----------------------------------------------------------------------
// Grouping helpers (preserve insertion order)
// -----------------------------------------------------------------------

/// Group projected rows by their custom group key, returning `(key, count)` pairs.
///
/// Used when GROUP BY targets a non-standard field like `guard_kind`.
fn group_rows_by_field(rows: &[SymbolRow]) -> Vec<(String, usize)> {
    let mut groups: Vec<(String, usize)> = Vec::new();
    for sr in rows {
        let key = sr.group_key.as_deref().unwrap_or("(empty)").to_string();
        let count = sr.count.unwrap_or(1);
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &key) {
            g.1 += count;
        } else {
            groups.push((key, count));
        }
    }
    groups
}

/// Group projected rows by `kind` (`fql_kind`).
///
/// Returns `(kind, Vec<&SymbolRow>)`.
fn group_rows_by_kind<'a>(rows: &'a [SymbolRow]) -> Vec<(String, Vec<&'a SymbolRow>)> {
    let mut groups: Vec<(String, Vec<&'a SymbolRow>)> = Vec::new();
    for sr in rows {
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &sr.kind) {
            g.1.push(sr);
        } else {
            let kind = sr.kind.clone();
            groups.push((kind, vec![sr]));
        }
    }
    groups
}

/// `(name, 1-based-line, node_id)` tuple stored per kind group in `group_outline`.
type OutlineItem<'a> = (&'a str, usize, Option<&'a str>);

/// Group outline entries by kind → Vec<(kind, Vec<OutlineItem>)>.
fn group_outline(entries: &[OutlineEntry]) -> Vec<(String, Vec<OutlineItem<'_>>)> {
    let mut groups: Vec<(String, Vec<OutlineItem<'_>>)> = Vec::new();
    for e in entries {
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &e.fql_kind) {
            g.1.push((&e.name, e.line, e.node_id.as_deref()));
        } else {
            groups.push((
                e.fql_kind.clone(),
                vec![(&e.name, e.line, e.node_id.as_deref())],
            ));
        }
    }
    groups
}

/// Group member entries by kind → Vec<(kind, Vec<(text, line)>)>.
fn group_members(members: &[MemberEntry]) -> Vec<(String, Vec<(&str, usize)>)> {
    let mut groups: Vec<(String, Vec<(&str, usize)>)> = Vec::new();
    for m in members {
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &m.fql_kind) {
            g.1.push((&m.text, m.line));
        } else {
            groups.push((m.fql_kind.clone(), vec![(&m.text, m.line)]));
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

/// Group usage rows by file → Vec<(file, Vec<line>)>.
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
            metadata: None,
            content: ShowContent::Outline {
                entries: vec![
                    OutlineEntry {
                        name: "int16_t".into(),
                        fql_kind: "type_alias".into(),
                        path: PathBuf::from("include/types.hpp"),
                        line: 17,
                        node_id: None,
                    },
                    OutlineEntry {
                        name: "int32_t".into(),
                        fql_kind: "type_alias".into(),
                        path: PathBuf::from("include/types.hpp"),
                        line: 18,
                        node_id: None,
                    },
                    OutlineEntry {
                        name: "Pid".into(),
                        fql_kind: "class_specifier".into(),
                        path: PathBuf::from("include/types.hpp"),
                        line: 22,
                        node_id: None,
                    },
                ],
            },
        });
        let csv = to_compact(&result);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], r#""show_outline","include/types.hpp""#);
        assert_eq!(lines[1], r#""fql_kind","[name,line]""#);
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
            metadata: None,
            content: ShowContent::Outline {
                entries: vec![
                    OutlineEntry {
                        name: "// ADC conversion".into(),
                        fql_kind: "comment".into(),
                        path: PathBuf::from("src/adc.cpp"),
                        line: 1,
                        node_id: None,
                    },
                    OutlineEntry {
                        name: "convertByte2Volts".into(),
                        fql_kind: "function_definition".into(),
                        path: PathBuf::from("src/adc.cpp"),
                        line: 5,
                        node_id: None,
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
            metadata: None,
            content: ShowContent::Members {
                members: vec![
                    MemberEntry {
                        fql_kind: "field".into(),
                        text: "uint16_t rpm_setpoint;".into(),
                        line: 28,
                    },
                    MemberEntry {
                        fql_kind: "method".into(),
                        text: "void setRPM(uint16_t);".into(),
                        line: 35,
                    },
                    MemberEntry {
                        fql_kind: "field".into(),
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
            metadata: None,
            content: ShowContent::Lines {
                lines: vec![
                    SourceLine {
                        line: 42,
                        text: "float convert(uint8_t raw) {".into(),
                        marker: None,
                        node_id: None,
                    },
                    SourceLine {
                        line: 43,
                        text: "    return raw * 3.3f / 255.0f;".into(),
                        marker: None,
                        node_id: None,
                    },
                    SourceLine {
                        line: 44,
                        text: "}".into(),
                        marker: None,
                        node_id: None,
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
            group_by_field: None,
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
            diff: None,
            suggestions: vec![],
            new_node_id: None,
        });
        let output = to_compact(&result);
        assert!(output.contains("rename_symbol"));
        assert!(output.contains("applied"));
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
            },
            SourceLine {
                line: 11,
                text: "    bar();".to_string(),
                marker: None,
                node_id: None,
            },
            SourceLine {
                line: 12,
                text: "}".to_string(),
                marker: None,
                node_id: None,
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
            },
            SourceLine {
                line: 11,
                text: "b".to_string(),
                marker: None,
                node_id: None,
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
}
