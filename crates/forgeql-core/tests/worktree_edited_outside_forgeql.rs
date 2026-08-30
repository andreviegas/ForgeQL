//! A file rewritten outside ForgeQL — by a formatter, a build step, an editor —
//! between two ForgeQL commands, on the indexed backend.
//!
//! The index is content-addressed per file, and a command that names a node, a
//! file or a symbol checks the file it is about to read or write against the
//! indexed content first, re-indexing that one file when the two differ. That
//! gate had one hole: a file the session had already mutated carries a dirty
//! segment, and a dirty segment was taken as fresh without looking at the disk
//! again — so a rewrite landing AFTER the session's own edit (which is exactly
//! when a gate's auto-format runs) was never noticed until the next mutation.
//! A read into the re-flowed region then walked stale rows whose end, derived
//! from the file's current newlines, precedes their stored start, and the
//! subtraction underflowed: a panic under overflow checks. Without them the
//! row's clamped line range is empty, so those lines carry no handle at all —
//! the observable defect was the panic. On the stdio MCP server the panic was
//! caught and reported as an error; on the HTTP server nothing caught it and
//! the connection died with the request.
//!
//! Every case here drives the gate through a real `execute()`: the file is
//! indexed, edited through ForgeQL so it carries a dirty segment, rewritten on
//! disk behind ForgeQL's back so every line shifts and the file shrinks, and
//! then read or edited through handles and names taken BEFORE the rewrite.
//!
//! Run with: `cargo test -p forgeql-core --test worktree_edited_outside_forgeql`
//!
//! BOUNDARY: these cases drive the columnar (indexed) backend, the only one
//! that stores a per-file content id. The in-memory backend — what a source
//! with no `.forgeql.yaml` falls back to — stores none, so it has nothing to
//! compare a file against; it answers from its table until ForgeQL next writes
//! or re-indexes the file, and the ordinal handles it prints on SHOW rows do
//! not resolve through `SHOW NODE`/`FIND NODE` anyway.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    unused_results
)]

use std::fs;
use std::path::PathBuf;

use forgeql_core::result::{ForgeQLResult, ShowContent, ShowResult};
use tempfile::tempdir;

mod common;

use common::TestSession;

const FILE: &str = "shift.cpp";

/// Twelve lines of header comment above three functions and a struct, so
/// removing the header shifts every construct up by twelve lines.
const HEADER: &str = "\
//1
//2
//3
//4
//5
//6
//7
//8
//9
//10
//11
//12
";

const BODY: &str = "\
namespace shifted {

int alpha(int carga)
{
    int base = carga + 1;
    return base * 2;
}

int beta(int carga)
{
    int base = carga + 2;
    return base * 3;
}

int gamma(int carga)
{
    int base = carga + 3;
    int extra = base + 1;
    return extra * 4;
}

struct Motor {
    int spin();
};

}  // namespace shifted
";

/// What the session itself writes into `alpha` — one line changed, same shape.
const ALPHA_EDITED: &str = "\
int alpha(int carga)
{
    int base = carga + 1;
    return base * 20;
}";

/// The rewrite the formatter makes to `gamma`: one line fewer.
const GAMMA_LONG: &str = "    int extra = base + 1;\n    return extra * 4;";
const GAMMA_SHORT: &str = "    return base * 4;";

/// 1-based number of the first line holding `needle`.
fn line_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|l| l.contains(needle))
        .map_or_else(|| panic!("no line holds {needle:?} in:\n{text}"), |i| i + 1)
}

fn lines_of(show: &ShowResult) -> &[forgeql_core::result::SourceLine] {
    match &show.content {
        ShowContent::Lines { lines, .. } => lines,
        other => panic!("expected line content, got {other:?}"),
    }
}

/// The hint as an agent actually receives it: read back out of the rendered
/// CSV, never off the struct. A verb whose renderer never emits the field
/// delivers no notice however well the field was set, so reading the struct
/// here would pin a promise nothing keeps.
fn hint_of(show: &ShowResult) -> String {
    let rendered = forgeql_core::compact::to_compact(&ForgeQLResult::Show(show.clone()));
    rendered
        .lines()
        .find_map(|line| line.strip_prefix("\"hint\","))
        .map_or_else(String::new, |rest| {
            rest.trim_matches('"').replace("\"\"", "\"")
        })
}

fn reindex_hint(text: impl AsRef<str>) -> bool {
    let text = text.as_ref();
    text.contains("outside ForgeQL") && text.contains(FILE)
}

/// `(node_id, rev)` of a function, from a `FIND symbols` row.
fn symbol(s: &mut TestSession, name: &str) -> (String, String) {
    let r = s.exec(&format!(
        "FIND symbols WHERE name = '{name}' WHERE fql_kind = 'function'"
    ));
    let m = common::as_query(&r)
        .results
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no symbol {name}"))
        .clone();
    (m.node_id.expect("node_id"), m.rev.expect("rev"))
}

/// The current start line of a handle, via `FIND NODE`.
fn resolved_line(s: &mut TestSession, handle: &str) -> usize {
    match s.exec(&format!("FIND NODE '{handle}'")) {
        ForgeQLResult::FindNode(node) => node.line,
        other => panic!("expected FindNode, got {other:?}"),
    }
}

struct Fixture {
    s: TestSession,
    file: PathBuf,
    file_hex: String,
    alpha: (String, String),
    beta: (String, String),
    gamma: (String, String),
    /// The file as it is on disk after the rewrite outside ForgeQL.
    after: String,
}

impl Fixture {
    fn line(&self, needle: &str) -> usize {
        line_of(&self.after, needle)
    }

    fn on_disk(&self) -> String {
        fs::read_to_string(&self.file).expect("read file")
    }
}

/// Index the file, edit `alpha` THROUGH ForgeQL so the file carries a dirty
/// segment, then rewrite the file on disk behind ForgeQL's back: the header is
/// gone (every line shifts up by twelve) and `gamma` is one line shorter.
fn mutated_then_rewritten() -> Fixture {
    let mut fx = indexed(true);
    let after = fx
        .on_disk()
        .replace(HEADER, "")
        .replace(GAMMA_LONG, GAMMA_SHORT);
    assert!(
        after.contains("return base * 20;") && !after.contains("//11"),
        "fixture: the rewrite must keep the session's edit and drop the header"
    );
    fs::write(&fx.file, &after).expect("rewrite outside ForgeQL");
    fx.after = after;
    fx
}

/// Same rewrite, but the session never edited the file first: the committed
/// segment is the only one, which is the case the gate already covered.
fn rewritten_without_a_prior_edit() -> Fixture {
    let mut fx = indexed(false);
    let after = fx
        .on_disk()
        .replace(HEADER, "")
        .replace(GAMMA_LONG, GAMMA_SHORT);
    fs::write(&fx.file, &after).expect("rewrite outside ForgeQL");
    fx.after = after;
    fx
}

fn indexed(edit_alpha_first: bool) -> Fixture {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join(FILE);
    fs::write(&file, format!("{HEADER}{BODY}")).expect("write fixture");
    let mut s = common::columnar_session_in(dir);

    let alpha = symbol(&mut s, "alpha");
    let beta = symbol(&mut s, "beta");
    let gamma = symbol(&mut s, "gamma");
    assert_eq!(line_of(&format!("{HEADER}{BODY}"), "int gamma("), 27);

    let alpha = if edit_alpha_first {
        let r = s.exec(&format!(
            "CHANGE NODE '{}' IF REV '{}' WITH <<CPP\n{ALPHA_EDITED}\nCPP",
            alpha.0, alpha.1
        ));
        assert!(
            common::as_mutation(&r).applied,
            "the session's own edit applies"
        );
        let on_disk = fs::read_to_string(&file).expect("read");
        assert!(on_disk.contains("return base * 20;"));
        // The handle survives its own edit; the rev moved with the bytes.
        let rev = s.node_rev(&alpha.0);
        assert_ne!(rev, alpha.1);
        (alpha.0, rev)
    } else {
        alpha
    };

    let after = fs::read_to_string(&file).expect("read");
    Fixture {
        s,
        file,
        file_hex: common::path_handle(FILE),
        alpha,
        beta,
        gamma,
        after,
    }
}

// ---------------------------------------------------------------------------
// Reads: every shape that resolves a span answers the file as it is now, and
// the first one to notice the rewrite says so.
// ---------------------------------------------------------------------------

#[test]
fn a_whole_file_read_into_the_reflowed_region_answers_the_current_file() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec(&format!("SHOW NODE '{}'", fx.file_hex));
    let show = common::as_show(&r);
    let lines = lines_of(show);
    let expected: Vec<&str> = fx.after.lines().collect();
    assert_eq!(lines.len(), expected.len(), "one row per current line");
    for (row, text) in lines.iter().zip(&expected) {
        assert_eq!(row.text, *text, "line {} is the current text", row.line);
    }
    assert!(
        reindex_hint(hint_of(show)),
        "the read that found the rewrite says the file was re-indexed: {:?}",
        show.hint
    );
    // The handle stamped on alpha's first line names a node that starts on
    // that line NOW — not the namespace the stale offsets used to resolve to.
    let alpha_line = fx.line("int alpha(");
    let stamped = lines
        .iter()
        .find(|l| l.line == alpha_line)
        .and_then(|l| l.node_id.clone())
        .expect("alpha's first line carries a handle");
    assert_eq!(resolved_line(&mut fx.s, &stamped), alpha_line);
}

#[test]
fn a_node_offset_read_answers_the_nodes_current_lines() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    let show = common::as_show(&r);
    let lines = lines_of(show);
    let alpha_line = fx.line("int alpha(");
    let got: Vec<(usize, &str)> = lines.iter().map(|l| (l.line, l.text.as_str())).collect();
    assert_eq!(
        got,
        vec![(alpha_line, "int alpha(int carga)"), (alpha_line + 1, "{")],
        "offset 1-2 is the node's first two lines where the node is NOW"
    );
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn a_line_filtered_node_read_answers_current_line_numbers() {
    let mut fx = mutated_then_rewritten();
    let alpha_line = fx.line("int alpha(");
    let r = fx.s.exec(&format!(
        "SHOW NODE '{}' WHERE line >= {}",
        fx.alpha.0,
        alpha_line + 3
    ));
    let show = common::as_show(&r);
    let got: Vec<(usize, &str)> = lines_of(show)
        .iter()
        .map(|l| (l.line, l.text.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (alpha_line + 3, "    return base * 20;"),
            (alpha_line + 4, "}")
        ]
    );
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn a_text_filtered_node_read_answers_current_text() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec(&format!(
        "SHOW NODE '{}' WHERE text LIKE '%return%'",
        fx.gamma.0
    ));
    let show = common::as_show(&r);
    let got: Vec<(usize, &str)> = lines_of(show)
        .iter()
        .map(|l| (l.line, l.text.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![(fx.line("return base * 4;"), "    return base * 4;")],
        "gamma's return is the rewritten one, at its current line"
    );
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

/// A read routed elsewhere by `USING 'legacy'` resolves its symbol in THAT
/// backend, so the gate resolves it there too: gating the file the DEFAULT
/// engine names would check, hash and re-index a file the command never reads.
/// This fixture builds no legacy table, so the verb refuses — and that is what
/// makes the case discriminating. Resolving on the default engine would find
/// `gamma`'s file, see it rewritten, re-index it, and the notice would ride
/// back out on the verb's own refusal. It must not.
#[test]
fn a_legacy_routed_read_is_not_gated_on_the_default_engines_file() {
    let mut fx = mutated_then_rewritten();
    let err =
        fx.s.try_fql("SHOW body OF 'gamma' USING 'legacy'")
            .expect_err("this fixture builds no legacy table, so the verb refuses");
    let msg = format!("{err:#}");
    assert!(
        !reindex_hint(&msg),
        "the gate resolved on the default engine and re-indexed a file this read \
         never touches — the notice rode back on the verb's own refusal: {msg}"
    );
}
#[test]
fn a_body_read_by_symbol_answers_the_current_body() {
    let mut fx = mutated_then_rewritten();
    let r =
        fx.s.exec("SHOW body OF 'gamma' DEPTH 99 WHERE text LIKE '%return%'");
    let show = common::as_show(&r);
    let got: Vec<(usize, &str)> = lines_of(show)
        .iter()
        .map(|l| (l.line, l.text.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![(fx.line("return base * 4;"), "    return base * 4;")]
    );
    assert_eq!(show.start_line, Some(fx.line("int gamma(")));
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn a_context_read_by_symbol_centres_on_the_current_line() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec("SHOW context OF 'gamma'");
    let show = common::as_show(&r);
    let centre = lines_of(show)
        .iter()
        .find(|l| l.marker.as_deref() == Some(">>>"))
        .expect("a centre line");
    assert_eq!(centre.line, fx.line("int gamma("));
    assert_eq!(centre.text, "int gamma(int carga)");
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn a_signature_read_by_symbol_reports_the_current_line() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec("SHOW signature OF 'beta'");
    let show = common::as_show(&r);
    match &show.content {
        ShowContent::Signature { line, .. } => assert_eq!(*line, fx.line("int beta(")),
        other => panic!("expected a signature, got {other:?}"),
    }
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn a_callees_read_by_symbol_is_gated_too() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec("SHOW callees OF 'gamma'");
    let show = common::as_show(&r);
    assert!(
        matches!(show.content, ShowContent::CallGraph { .. }),
        "got {:?}",
        show.content
    );
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn a_members_read_by_type_reports_current_lines() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec("SHOW members OF 'Motor'");
    let show = common::as_show(&r);
    match &show.content {
        ShowContent::Members { members, .. } => {
            let spin = members
                .iter()
                .find(|m| m.text.contains("spin"))
                .expect("spin");
            assert_eq!(spin.line, fx.line("int spin();"));
        }
        other => panic!("expected members, got {other:?}"),
    }
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn an_outline_and_a_line_range_read_by_file_report_current_lines() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec(&format!("SHOW outline OF '{FILE}'"));
    let show = common::as_show(&r);
    let gamma_line = fx.line("int gamma(");
    match &show.content {
        ShowContent::Outline { entries } => {
            let gamma = entries
                .iter()
                .find(|e| e.node_id.as_deref() == Some(fx.gamma.0.as_str()))
                .expect("gamma's handle from before the rewrite is in the outline");
            assert_eq!((gamma.name.as_str(), gamma.line), ("gamma", gamma_line));
        }
        other => panic!("expected an outline, got {other:?}"),
    }
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);

    // The file is fresh now: a second read answers without the notice, and
    // the handle stamped on gamma's first line starts on that line.
    let r = fx.s.exec(&format!(
        "SHOW LINES {gamma_line}-{} OF '{FILE}'",
        gamma_line + 4
    ));
    let show = common::as_show(&r);
    let first = &lines_of(show)[0];
    assert_eq!(
        (first.line, first.text.as_str()),
        (gamma_line, "int gamma(int carga)")
    );
    assert!(
        !reindex_hint(hint_of(show)),
        "no second notice: {:?}",
        show.hint
    );
    let stamped = first
        .node_id
        .clone()
        .expect("gamma's first line carries a handle");
    assert_eq!(resolved_line(&mut fx.s, &stamped), gamma_line);
}

#[test]
fn find_node_reports_the_current_span_and_rev() {
    let mut fx = mutated_then_rewritten();
    let r = fx.s.exec(&format!("FIND NODE '{}'", fx.gamma.0));
    let ForgeQLResult::FindNode(node) = r else {
        panic!("expected FindNode, got {r:?}");
    };
    let gamma_line = fx.line("int gamma(");
    assert_eq!((node.line, node.end_line), (gamma_line, gamma_line + 4));
    assert_ne!(
        node.rev, fx.gamma.1,
        "gamma's bytes changed, so its rev moved"
    );
    let rendered = forgeql_core::compact::to_compact(&ForgeQLResult::FindNode(node));
    assert!(
        rendered
            .lines()
            .any(|line| line.starts_with("\"hint\",") && reindex_hint(line)),
        "the notice must reach the rendered answer, not just the struct: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn control_a_rewrite_before_any_edit_of_the_session_is_seen_too() {
    let mut fx = rewritten_without_a_prior_edit();
    let r = fx.s.exec(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    let show = common::as_show(&r);
    let alpha_line = fx.line("int alpha(");
    let got: Vec<(usize, &str)> = lines_of(show)
        .iter()
        .map(|l| (l.line, l.text.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![(alpha_line, "int alpha(int carga)"), (alpha_line + 1, "{")]
    );
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn control_a_file_nobody_rewrote_is_read_without_a_notice() {
    let mut fx = indexed(true);
    let r = fx.s.exec(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    let show = common::as_show(&r);
    let alpha_line = fx.line("int alpha(");
    assert_eq!(lines_of(show)[0].line, alpha_line);
    assert!(!reindex_hint(hint_of(show)), "no notice: {:?}", show.hint);
    // And a whole-file read, the shape that used to panic, is plain too.
    let r = fx.s.exec(&format!("SHOW NODE '{}'", fx.file_hex));
    assert!(!reindex_hint(hint_of(common::as_show(&r))));
}

// ---------------------------------------------------------------------------
// Edits: a rev read before the rewrite is refused when the bytes moved, and an
// edit whose rev still matches lands where the node is now — never on the old
// line numbers.
// ---------------------------------------------------------------------------

#[test]
fn a_stale_rev_is_refused_with_rev_mismatch_not_applied_to_shifted_lines() {
    let mut fx = mutated_then_rewritten();
    let err = fx
        .s
        .try_fql(&format!(
            "CHANGE NODE '{}' IF REV '{}' WITH <<CPP\nint gamma(int carga)\n{{\n    return carga;\n}}\nCPP",
            fx.gamma.0, fx.gamma.1
        ))
        .expect_err("the rev predates the rewrite, so the edit must be refused");
    let text = format!("{err:#}");
    assert!(text.contains("rev_mismatch"), "refusal: {text}");
    // The refusal is a JSON payload a self-healing caller parses, and the
    // notice rides it as a field of its own, not as text glued to the JSON.
    let payload: serde_json::Value =
        serde_json::from_str(&text).expect("a structured refusal stays parseable");
    assert_eq!(payload["error"], "rev_mismatch");
    let reindexed = payload["reindexed"].as_str().expect("a `reindexed` field");
    assert!(
        reindex_hint(reindexed),
        "the field says which file the gate re-indexed: {reindexed}"
    );
    assert_eq!(fx.on_disk(), fx.after, "a refused edit writes nothing");

    // The refusal hands back what the agent needs: the current rev edits.
    let rev = fx.s.node_rev(&fx.gamma.0);
    assert_ne!(rev, fx.gamma.1);
    let r = fx.s.exec(&format!(
        "CHANGE NODE '{}' IF REV '{rev}' WITH <<CPP\nint gamma(int carga)\n{{\n    return carga * 40;\n}}\nCPP",
        fx.gamma.0
    ));
    assert!(common::as_mutation(&r).applied);
    let now = fx.on_disk();
    assert_eq!(line_of(&now, "int gamma("), fx.line("int gamma("));
    assert!(now.contains("return carga * 40;"));
    assert!(now.contains("return base * 20;"), "alpha untouched");
    assert!(!now.contains("//11"), "the header stays gone");
}

#[test]
fn an_unchanged_node_whose_rev_still_matches_is_edited_where_it_is_now() {
    let mut fx = mutated_then_rewritten();
    // beta's bytes did not change in the rewrite, only its position: the rev
    // taken before the rewrite is still the node's rev, and the edit must land
    // on beta's current lines — twelve above where the index last saw it.
    let r = fx.s.exec(&format!(
        "CHANGE NODE '{}' IF REV '{}' WITH <<CPP\nint beta(int carga)\n{{\n    return carga * 30;\n}}\nCPP",
        fx.beta.0, fx.beta.1
    ));
    assert!(common::as_mutation(&r).applied);
    let now = fx.on_disk();
    let beta_line = fx.line("int beta(");
    assert_eq!(line_of(&now, "int beta("), beta_line);
    assert_eq!(line_of(&now, "return carga * 30;"), beta_line + 2);
    assert!(!now.contains("carga + 2;"), "the old beta body is gone");
    assert!(now.contains("return base * 20;"), "alpha untouched");
    assert!(now.contains("return base * 4;"), "gamma untouched");
    assert!(!now.contains("//11"), "the header stays gone");
}

#[test]
fn a_found_sweep_armed_before_the_rewrite_lands_on_the_current_lines() {
    let mut fx = indexed(true);
    let r =
        fx.s.exec("FIND symbols WHERE name = 'beta' WHERE fql_kind = 'function'");
    let master = common::as_query(&r)
        .found_rev
        .clone()
        .expect("a complete FIND arms the set");
    let after = fx
        .on_disk()
        .replace(HEADER, "")
        .replace(GAMMA_LONG, GAMMA_SHORT);
    fs::write(&fx.file, &after).expect("rewrite outside ForgeQL");
    fx.after = after;

    let r = fx.s.exec(&format!(
        "CHANGE NODES FOUND IF REV '{master}' MATCHING 'carga + 2' WITH 'carga + 22'"
    ));
    assert!(common::as_mutation(&r).applied);
    let now = fx.on_disk();
    assert_eq!(line_of(&now, "carga + 22;"), fx.line("int beta(") + 2);
    assert!(
        now.contains("carga + 1;") && now.contains("carga + 3;"),
        "only beta swept"
    );
    assert!(!now.contains("//11"), "the header stays gone");
}

// ---------------------------------------------------------------------------
// The reach of the gate, stated as tests: it can only check what the stale
// index can still name.
// ---------------------------------------------------------------------------

#[test]
fn a_symbol_the_rewrite_introduced_is_not_seen_until_the_file_is_read_by_handle_or_path() {
    let mut fx = mutated_then_rewritten();
    // The rewrite also renames gamma. The stale index cannot resolve the new
    // name, so a read by symbol names no file, re-indexes nothing and answers
    // that no symbol matches — the documented edge.
    let renamed = fx
        .after
        .replace("int gamma(int carga)", "int delta(int carga)");
    fs::write(&fx.file, &renamed).expect("rename gamma outside ForgeQL");
    fx.after = renamed;
    let err =
        fx.s.try_fql("SHOW body OF 'delta'")
            .expect_err("a name the stale index does not hold is not found");
    assert!(
        format!("{err:#}").contains("delta"),
        "the refusal names the symbol: {err:#}"
    );

    // A read by path re-indexes the file; the new name resolves from then on.
    let r = fx.s.exec(&format!("SHOW outline OF '{FILE}'"));
    assert!(reindex_hint(hint_of(common::as_show(&r))));
    let r = fx.s.exec("SHOW body OF 'delta' DEPTH 0");
    assert_eq!(common::as_show(&r).start_line, Some(fx.line("int delta(")));
}

#[test]
fn a_re_index_that_fails_refuses_the_command_instead_of_answering_from_old_rows() {
    let mut fx = mutated_then_rewritten();
    // Seal the staging directory the re-index writes its segment into, so the
    // re-index the gate runs cannot land — the notice "lines and revs here are
    // current" must never be printed over rows the gate could not refresh.
    let staging = fx.s.workspace().join(".forgeql-staging");
    let writable = fs::metadata(&staging)
        .expect("staging dir exists")
        .permissions();
    let mut sealed = writable.clone();
    sealed.set_readonly(true);
    fs::set_permissions(&staging, sealed).expect("seal staging dir");
    if fs::File::create(staging.join(".probe")).is_ok() {
        // Permissions are not enforced for this user (root): nothing to observe.
        let _ = fs::remove_file(staging.join(".probe"));
        fs::set_permissions(&staging, writable).expect("unseal staging dir");
        return;
    }
    let outcome = fx.s.try_fql(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    fs::set_permissions(&staging, writable).expect("unseal staging dir");
    let err = outcome.expect_err("a file that could not be re-indexed refuses the read");
    let text = format!("{err:#}");
    assert!(
        text.contains(FILE) && text.contains("could not be re-indexed"),
        "the refusal names the file and the failure: {text}"
    );
    // With the directory writable again the next read re-indexes and answers.
    let r = fx.s.exec(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    let show = common::as_show(&r);
    assert_eq!(lines_of(show)[0].line, fx.line("int alpha("));
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[test]
fn an_outline_read_by_handle_reports_current_lines() {
    let mut fx = mutated_then_rewritten();
    // `SHOW outline OF '<node_id>'` is the subtree form: it names a handle,
    // not a path, and is gated like every other read of that handle's file.
    let r = fx.s.exec(&format!("SHOW outline OF '{}'", fx.gamma.0));
    let show = common::as_show(&r);
    let gamma_line = fx.line("int gamma(");
    match &show.content {
        ShowContent::Outline { entries } => {
            assert!(!entries.is_empty(), "gamma's subtree has rows");
            for entry in entries {
                assert!(
                    (gamma_line..=gamma_line + 4).contains(&entry.line),
                    "{} at line {} lies outside gamma's current span",
                    entry.name,
                    entry.line
                );
            }
        }
        other => panic!("expected an outline, got {other:?}"),
    }
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}

#[cfg(unix)]
#[test]
fn a_file_that_could_not_be_read_is_not_stamped_as_verified() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = mutated_then_rewritten();
    // Make the file unreadable while its size and mtime stay as they are: the
    // gate can stat it and cannot hash it, so it has nothing to compare and
    // must record nothing.
    let readable = fs::metadata(&fx.file).expect("stat").permissions();
    let mut sealed = readable.clone();
    sealed.set_mode(0o000);
    fs::set_permissions(&fx.file, sealed).expect("seal file");
    if fs::read(&fx.file).is_ok() {
        // Permissions are not enforced for this user (root): nothing to observe.
        fs::set_permissions(&fx.file, readable).expect("unseal file");
        return;
    }
    // This read fails on the unreadable bytes — an I/O error, not a stale
    // answer; what matters is what the gate remembered about the file.
    let _ = fx.s.try_fql(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    fs::set_permissions(&fx.file, readable).expect("unseal file");
    // Readable again with the same stat: the next read must hash, find the
    // rewrite, re-index and answer the current lines — not trust a stamp taken
    // while the bytes could not be seen.
    let r = fx.s.exec(&format!("SHOW NODE '{}(1-2)'", fx.alpha.0));
    let show = common::as_show(&r);
    assert_eq!(lines_of(show)[0].line, fx.line("int alpha("));
    assert!(reindex_hint(hint_of(show)), "hint: {:?}", show.hint);
}
