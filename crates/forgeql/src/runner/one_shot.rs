//! One-shot mode runner — execute a single FQL statement and exit.

use crate::cli::CliFormat;
use crate::execute::execute_and_print;
use crate::session::{session_load, session_save, session_try_resume};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::query_logger::QueryLogger;

/// Execute one FQL statement, then exit.
///
/// `session_override` lets the `--session` CLI flag inject a session id
/// without the user re-issuing `USE`.
pub(crate) fn run_one_shot(
    mut engine: ForgeQLEngine,
    fql: &str,
    session_override: Option<&str>,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) {
    let mut session = session_load();
    if let Some(sid) = session_override {
        session.session_id = Some(sid.to_string());
    }
    session_try_resume(&mut engine, &mut session);
    execute_and_print(&mut engine, fql, &mut session, logger.as_mut(), format);
    session_save(&session);
}

/// `forgeql gc` — delete stale columnar cache version directories to reclaim
/// disk space. Thin sugar over the `VACUUM` verb so the pruning logic lives
/// once in the engine: it previews (`VACUUM …`), asks for confirmation unless
/// `yes`, then applies (`VACUUM … APPLY`).
pub(crate) fn run_gc(
    engine: &ForgeQLEngine,
    source: Option<&str>,
    keep: usize,
    all: bool,
    yes: bool,
    format: CliFormat,
) {
    use forgeql_core::storage::columnar::gc;
    use std::io::Write as _;

    // How many stale directories to list before summarizing the rest.
    const SHOW: usize = 20;

    // Preview: scan and classify, delete nothing.
    let preview = match engine.vacuum_report(source, keep, all, false) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("error: {err:#}");
            return;
        }
    };

    // `--format json` is a machine-readable passthrough for scripting.
    if matches!(format, CliFormat::Json) {
        match serde_json::to_string_pretty(&preview) {
            Ok(json) => println!("{json}"),
            Err(err) => eprintln!("error: {err}"),
        }
        return;
    }

    let deletes: Vec<&gc::VacuumEntry> = preview
        .entries
        .iter()
        .filter(|e| e.action == gc::VacuumAction::Delete)
        .collect();

    if deletes.is_empty() {
        println!(
            "Nothing to reclaim — all {} cache version dir(s) across {} source(s) are current.",
            preview.entries.len(),
            preview.source_count
        );
        return;
    }

    // List the stale directories (a bounded sample), then the authoritative total.
    println!("The following stale cache version directories will be deleted:");
    for e in deletes.iter().take(SHOW) {
        println!(
            "  {:>10}  {}",
            gc::human_bytes(e.size_bytes),
            e.path.display()
        );
    }
    if deletes.len() > SHOW {
        println!("  … and {} more", deletes.len() - SHOW);
    }
    println!(
        "\n{} {}, {} reclaimable ({} source(s) scanned).",
        preview.delete_count,
        plural_dirs(preview.delete_count),
        gc::human_bytes(preview.delete_bytes),
        preview.source_count
    );

    if !yes {
        eprint!("\nProceed with deletion? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        let confirmed = std::io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "YES");
        if !confirmed {
            eprintln!("Aborted. Nothing deleted.");
            return;
        }
    }

    // Apply the deletion, then report what was actually reclaimed.
    let applied = match engine.vacuum_report(source, keep, all, true) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("error: {err:#}");
            return;
        }
    };
    if applied.errors > 0 {
        println!(
            "Deleted {} {}, reclaimed {} ({} failed — see logs).",
            applied.delete_count,
            plural_dirs(applied.delete_count),
            gc::human_bytes(applied.delete_bytes),
            applied.errors
        );
    } else {
        println!(
            "Deleted {} {}, reclaimed {}.",
            applied.delete_count,
            plural_dirs(applied.delete_count),
            gc::human_bytes(applied.delete_bytes)
        );
    }
}

/// `"directory"` / `"directories"` for a count.
const fn plural_dirs(n: usize) -> &'static str {
    if n == 1 { "directory" } else { "directories" }
}
