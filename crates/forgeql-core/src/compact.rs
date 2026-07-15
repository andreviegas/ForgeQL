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
    CallDirection, ExportPatchResult, FileEntry, FindNodeResult, ForgeQLResult, JobListResult,
    JobStartedResult, MemberEntry, MutationResult, OutlineEntry, PendingExecResult, QueryResult,
    RunResult, SessionStats, ShowContent, ShowDiffResult, ShowResult, SourceLine, SymbolRow,
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
        ForgeQLResult::Run(r) => compact_run(r),
        ForgeQLResult::JobStarted(j) => compact_job_started(j),
        ForgeQLResult::JobStatus(s) => compact_job_status(s),
        ForgeQLResult::JobList(l) => compact_job_list(l),
        ForgeQLResult::PendingExec(p) => compact_pending_exec(p),
        ForgeQLResult::ExportPatch(e) => compact_export_patch(e),
        ForgeQLResult::ShowDiff(d) => compact_show_diff(d),
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
        ShowContent::FileList { files, total } => compact_filelist(files, *total, s),
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
    // Node-relative rendering replaces absolute line numbers with stable node
    // handles + offsets so the agent edits with `CHANGE NODE 'id(off)'`:
    //   * Per-line (SHOW LINES on a parsed file): every line carries its own
    //     innermost containing node + a 1-based offset (`node_offset`). Render a
    //     `node`,`off`,`text` table with the shared `n<segment-hex>` prefix
    //     hoisted once into the header; gap lines (no containing node) show
    //     empty handles, text only.
    //   * Frame (SHOW body): the whole region is one node (its id sits on the
    //     first line); show 1-based offsets within that single frame, id once.
    // Falls back to absolute line numbers when neither applies (SHOW context,
    // an unparsed file, or a symbol with no ordinal).
    let per_line = lines.iter().any(|line| line.node_offset.is_some());
    let frame = lines
        .iter()
        .find_map(|line| line.node_id.clone())
        .zip(s.start_line);
    if per_line {
        let prefix = lines
            .iter()
            .find_map(|line| line.node_id.as_deref())
            .and_then(|id| id.split_once('.').map(|(p, _)| p))
            .unwrap_or_default();
        row(&mut out, &[&op, &sym, &file, &span, &q(prefix)]);
        // Schema hint: `node` is the segment-relative ordinal, `off` the
        // 1-based offset of the line within that node.
        row(&mut out, &[&q("node"), &q("off"), &q("text")]);
        for line in lines {
            let (node, off) = match (line.node_id.as_deref(), line.node_offset) {
                (Some(id), Some(o)) => {
                    let ord = id.split_once('.').map_or(id, |(_, ord)| ord);
                    (format!(".{ord}"), o.to_string())
                }
                _ => (String::new(), String::new()),
            };
            row(&mut out, &[&q(&node), &q(&off), &q(&line.text)]);
        }
    } else if let Some((node_id, start)) = frame {
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
    // Schema hint. One row per node in pre-order; the first column is the
    // nesting depth, so the structural tree is visible without re-grouping.
    let schema = if has_node_id {
        "[fql_kind,name,line,node_id]"
    } else {
        "[fql_kind,name,line]"
    };
    row(&mut out, &[&q("depth"), &q(schema)]);
    for e in entries {
        let display_name = if e.fql_kind == "comment" {
            crate::result::compact_name(&e.name).into_owned()
        } else {
            e.name.clone()
        };
        let line = e.line.to_string();
        let val = if has_node_id {
            bracket(&[
                &e.fql_kind,
                &display_name,
                &line,
                e.node_id.as_deref().unwrap_or(""),
            ])
        } else {
            bracket(&[&e.fql_kind, &display_name, &line])
        };
        row(&mut out, &[&q(&e.depth.to_string()), &q(&val)]);
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

/// FIND files → 2 flat columns: path, size — plus `error_count` and/or
/// `parse_coverage` when the query asked about them.
fn compact_filelist(files: &[FileEntry], total: usize, s: &ShowResult) -> String {
    let mut out = String::with_capacity(files.len() * 40);
    // Header.
    let tot = total.to_string();
    row(&mut out, &[&q("find_files"), &tot]);
    // `error_count` / `parse_coverage` are populated only when the query asked
    // about them, so each column shows up only then — a plain `FIND files` keeps
    // its two columns and pays nothing.
    let with_errors = files.iter().any(|e| e.error_count.is_some());
    let with_coverage = files.iter().any(|e| e.parse_coverage.is_some());
    // `node_id` / `rev` are on every path row (an aggregate row addresses
    // nothing, so a GROUP BY result drops both columns).
    let with_handles = files.iter().any(|e| e.node_id.is_some());
    let with_revs = files.iter().any(|e| e.rev.is_some());
    // Schema hint.
    let mut schema: Vec<String> = vec![q("path"), q("size")];
    if with_handles {
        schema.push(q("node_id"));
    }
    if with_revs {
        schema.push(q("rev"));
    }
    if with_errors {
        schema.push(q("error_count"));
    }
    if with_coverage {
        schema.push(q("parse_coverage"));
    }
    let schema_refs: Vec<&str> = schema.iter().map(String::as_str).collect();
    row(&mut out, &schema_refs);
    // Data rows.
    for entry in files {
        let mut cells: Vec<String> = vec![q(&entry.path.to_string_lossy()), entry.size.to_string()];
        if with_handles {
            cells.push(q(entry.node_id.as_deref().unwrap_or("")));
        }
        if with_revs {
            cells.push(q(entry.rev.as_deref().unwrap_or("")));
        }
        if with_errors {
            cells.push(entry.error_count.unwrap_or(0).to_string());
        }
        if with_coverage {
            cells.push(entry.parse_coverage.unwrap_or(100).to_string());
        }
        let cell_refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        row(&mut out, &cell_refs);
    }
    // The master rev of the set this FIND armed — same row, same meaning as on
    // FIND symbols/usages, so an agent learns the gate once.
    if let Some(rev) = s
        .metadata
        .as_ref()
        .and_then(|m| m.get("found_rev"))
        .and_then(serde_json::Value::as_str)
    {
        row(&mut out, &[&q("found_rev"), &q(rev)]);
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
    row(
        &mut out,
        &[&q("lines_removed"), &r.lines_removed.to_string()],
    );
    if let Some(ref id) = r.new_node_id {
        // The handle and its rev together — a chained edit needs no re-read.
        match r.new_rev {
            Some(ref rev) => row(&mut out, &[&q("new_node_id"), &q(id), &q(rev)]),
            None => row(&mut out, &[&q("new_node_id"), &q(id)]),
        }
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
    let mut out = String::with_capacity(v.output.len() + 128);
    row(&mut out, &[&q("verify_build"), &q(&v.step), &q(verdict)]);
    out.push_str(v.output.trim_end_matches('\n'));
    if !v.success {
        out.push('\n');
        row(&mut out, &[&q("hint"), &q(FAIL_GREP_HINT)]);
        chomp(&mut out);
    }
    out
}

/// Failure-triage recipe appended to failed verify/run/job output. Appended
/// last so a tail-windowed log still shows it; the transport buffers the full
/// log for `SHOW MORE` when it exceeds the inline window.
const FAIL_GREP_HINT: &str = "grep the full log with SHOW MORE WHERE text MATCHES \
    'error\\[|error:|FAILED|warning:' or page it with SHOW MORE TAIL 60";

/// `RUN '<step>'` output — same shape and failure hint as `VERIFY build`.
fn compact_run(r: &RunResult) -> String {
    let verdict = if r.success { "PASS" } else { "FAIL" };
    let mut out = String::with_capacity(r.output.len() + 128);
    row(&mut out, &[&q("run"), &q(&r.step), &q(verdict)]);
    out.push_str(r.output.trim_end_matches('\n'));
    if !r.success {
        out.push('\n');
        row(&mut out, &[&q("hint"), &q(FAIL_GREP_HINT)]);
        chomp(&mut out);
    }
    out
}

/// `JOB START` acknowledgement with the poll recipe inline.
fn compact_job_started(j: &JobStartedResult) -> String {
    let mut out = String::with_capacity(160);
    row(&mut out, &[&q("job_started"), &q(&j.id), &q(&j.label)]);
    let hint = format!(
        "runs in background — poll JOB STATUS '{}'; JOB LIST shows all jobs",
        j.id
    );
    row(&mut out, &[&q("hint"), &q(&hint)]);
    chomp(&mut out);
    out
}

/// `JOB STATUS` — status row, then output (when finished), then a
/// state-matched next-step hint as the last line.
fn compact_job_status(s: &crate::jobs::JobSnapshot) -> String {
    let mut out = String::with_capacity(s.output.len() + 192);
    row(
        &mut out,
        &[
            &q("job_status"),
            &q(&s.id),
            &q(&s.label),
            &q(s.state.as_str()),
            &s.elapsed_ms.to_string(),
        ],
    );
    if !s.output.is_empty() {
        out.push_str(s.output.trim_end_matches('\n'));
        out.push('\n');
    }
    match s.state {
        crate::jobs::JobState::Queued | crate::jobs::JobState::Running => {
            let hint = format!(
                "still {} — re-check with JOB STATUS '{}'",
                s.state.as_str(),
                s.id
            );
            row(&mut out, &[&q("hint"), &q(&hint)]);
        }
        crate::jobs::JobState::Failed => row(&mut out, &[&q("hint"), &q(FAIL_GREP_HINT)]),
        crate::jobs::JobState::Succeeded => {}
    }
    chomp(&mut out);
    out
}

/// `JOB LIST` — one row per job plus the poll recipe for the newest job.
fn compact_job_list(l: &JobListResult) -> String {
    let mut out = String::with_capacity(64 + l.jobs.len() * 64);
    row(&mut out, &[&q("job_list"), &l.jobs.len().to_string()]);
    row(&mut out, &[&q("id"), &q("[state,label,elapsed_ms]")]);
    for job in &l.jobs {
        let detail = bracket(&[job.state.as_str(), &job.label, &job.elapsed_ms.to_string()]);
        row(&mut out, &[&q(&job.id), &detail]);
    }
    if let Some(newest) = l.jobs.last() {
        let hint = format!("JOB STATUS '{}' returns a job's output", newest.id);
        row(&mut out, &[&q("hint"), &q(&hint)]);
    }
    chomp(&mut out);
    out
}

/// Defensive rendering for a pending job the transport did not wait out.
fn compact_pending_exec(p: &PendingExecResult) -> String {
    let mut out = String::with_capacity(160);
    row(&mut out, &[&q("job_started"), &q(&p.job_id), &q(&p.step)]);
    let hint = format!(
        "runs in background — poll JOB STATUS '{}'; JOB LIST shows all jobs",
        p.job_id
    );
    row(&mut out, &[&q("hint"), &q(&hint)]);
    chomp(&mut out);
    out
}

/// EXPORT PATCH → header, one row per patch file, then the raw mbox content.
///
/// The content is emitted verbatim (not CSV-quoted) so each patch line maps
/// to one buffer line, letting `SHOW MORE` page it and letting the agent copy
/// it byte-exact. The per-file `sha256` rows let the receiver verify the
/// transfer before `git am`.
fn compact_export_patch(e: &ExportPatchResult) -> String {
    let mut out = String::with_capacity(e.content.len() + 256);
    row(
        &mut out,
        &[&q("export_patch"), &q(&e.range), &e.files.len().to_string()],
    );
    row(&mut out, &[&q("file"), &q("bytes"), &q("sha256")]);
    for f in &e.files {
        row(
            &mut out,
            &[
                &q(&f.path.display().to_string()),
                &f.bytes.to_string(),
                &q(&f.sha256),
            ],
        );
    }
    if let Some(hint) = &e.hint {
        row(&mut out, &[&q("hint"), &q(hint)]);
    }
    out.push_str(e.content.trim_end_matches('\n'));
    out
}

/// Render `SHOW DIFF` — the file map first (that is the reviewer's triage
/// question), then the hunks. Over-cap output is buffered by the SHOW MORE ring.
fn compact_show_diff(d: &ShowDiffResult) -> String {
    let mut out = String::with_capacity(d.content.len() + 256);
    row(&mut out, &[&q("show_diff"), &d.files.len().to_string()]);
    row(
        &mut out,
        &[&q("status"), &q("added"), &q("removed"), &q("file")],
    );
    for f in &d.files {
        row(
            &mut out,
            &[
                &q(&f.status),
                &f.added.to_string(),
                &f.removed.to_string(),
                &q(&f.path.display().to_string()),
            ],
        );
    }
    if let Some(hint) = &d.hint {
        row(&mut out, &[&q("hint"), &q(hint)]);
    }
    out.push_str(d.content.trim_end_matches('\n'));
    out
}

// -----------------------------------------------------------------------
// Query results
// -----------------------------------------------------------------------

fn compact_query(query: &QueryResult) -> String {
    let mut out = match query.op.as_str() {
        "find_usages" => compact_find_usages(query),
        "count_usages" => compact_count_usages(query),
        _ => compact_find_grouped_by_kind(query),
    };
    // Engine-attached guidance (e.g. a WHERE field no row type carries):
    // one CSV row so the caller sees it next to the (usually empty) results.
    if let Some(hint) = &query.hint {
        row(&mut out, &[&q("hint"), &q(hint)]);
    }
    // The master rev of the set this FIND just armed — what a bulk
    // `… NODE[S] LAST` mutation quotes in IF REV. One row, and only when a rev
    // was issued: a truncated result deliberately has none.
    if let Some(rev) = &query.found_rev {
        row(&mut out, &[&q("found_rev"), &q(rev)]);
    }
    out
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
        // The rev rides with the handle — same column group, never apart from it.
        let schema = match (has_enclosing_fn, has_node_id) {
            (true, true) => format!("[name,path,line,enclosing_fn,node_id,rev,{metric_label}]"),
            (true, false) => format!("[name,path,line,enclosing_fn,{metric_label}]"),
            (false, true) => format!("[name,path,line,node_id,rev,{metric_label}]"),
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
                        sr.rev.as_deref().unwrap_or(""),
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
                        sr.rev.as_deref().unwrap_or(""),
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

#[cfg(test)]
mod tests;
