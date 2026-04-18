//! Interactive REPL runner.

use anyhow::{Context, Result};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::cli::CliFormat;
use crate::execute::execute_and_print;
use crate::session::{session_config_dir, session_load, session_save, session_try_resume};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::query_logger::QueryLogger;

/// Start the interactive REPL.
///
/// Loads history and last session from disk, prints a banner, then
/// loops reading lines until the user types `exit`, `quit`, `\q`, or
/// sends EOF / Ctrl-C.
pub(crate) fn run_repl(
    mut engine: ForgeQLEngine,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) -> Result<()> {
    let mut editor = DefaultEditor::new().context("failed to initialise line editor")?;

    // Load readline history from config dir.
    let history_path = session_config_dir().map(|d| d.join("history.txt"));
    if let Some(ref path) = history_path {
        let _ = editor.load_history(path);
    }

    // Try to resume a saved session.
    let mut session = session_load();
    session_try_resume(&mut engine, &mut session);

    println!(
        "ForgeQL v{} — type 'help' or 'exit'",
        env!("CARGO_PKG_VERSION")
    );
    println!();

    loop {
        let prompt = session.session_id.as_ref().map_or_else(
            || "forgeql> ".to_string(),
            |sid| format!("forgeql [{sid}]> "),
        );

        match editor.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(trimmed);

                match trimmed {
                    "exit" | "quit" | "\\q" => break,
                    "help" | "\\h" => {
                        print_repl_help();
                        continue;
                    }
                    _ => {}
                }

                execute_and_print(&mut engine, trimmed, &mut session, logger.as_mut(), format);
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("readline error: {err}");
                break;
            }
        }
    }

    // Persist history and session.
    if let Some(ref path) = history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.save_history(path);
    }
    session_save(&session);

    Ok(())
}

/// Print a short command-reference cheat-sheet.
pub(crate) fn print_repl_help() {
    println!("  FIND symbols WHERE name LIKE 'set%'");
    println!("  FIND usages OF 'showCode'");
    println!("  FIND defines");
    println!("  FIND enums");
    println!("  SHOW body OF 'myFunction'");
    println!("  SHOW outline OF 'src/main.cpp'");
    println!("  RENAME symbol 'old' TO 'new'");
    println!("  CREATE SOURCE 'name' FROM 'url'");
    println!("  USE source.branch");
    println!("  exit / quit / \\q");
    println!();
}
