//! Lightweight, flag-gated debug logging.
//!
//! Activated by the `--debug <file>` server flag (see `forgeql::cli`). When a
//! sink is installed, the [`debug_log!`] macro appends one line per call to the
//! target file; with no sink installed the macro is a cheap no-op, so
//! instrumentation can be left in place permanently in hot paths.
//!
//! Output goes to a file, never to the query response, so enabling it cannot
//! affect normal results — it is a pure side channel for diagnosing internals
//! such as ordinal reassignment during reindex.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Process-wide debug sink. Empty until [`init`] installs a file handle.
static SINK: OnceLock<Mutex<File>> = OnceLock::new();

/// Install the debug sink, truncating `path` so each server launch starts with
/// a clean log.
///
/// Idempotent: if a sink is already installed the first one wins and the freshly
/// opened handle is dropped, so re-entrant startup paths cannot clobber state.
///
/// # Errors
/// Returns an error if `path` cannot be created or opened for writing.
pub fn init(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let _ = SINK.set(Mutex::new(file));
    Ok(())
}

/// Returns `true` when a debug sink is installed (i.e. `--debug` was passed).
#[must_use]
pub fn is_enabled() -> bool {
    SINK.get().is_some()
}

/// Append one preformatted line to the debug sink. No-op when disabled.
///
/// Prefer the [`debug_log!`] macro, which skips argument formatting entirely
/// when the sink is absent.
pub fn write_line(args: std::fmt::Arguments<'_>) {
    if let Some(lock) = SINK.get()
        && let Ok(mut file) = lock.lock()
    {
        let _ = writeln!(file, "{args}");
        let _ = file.flush();
    }
}

/// Append a line to the `--debug` log, formatted like `println!`.
///
/// Expands to an [`is_enabled`] check first, so the formatting cost is only
/// paid when debugging is active. Safe to leave in hot paths permanently.
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug_log::is_enabled() {
            $crate::debug_log::write_line(::std::format_args!($($arg)*));
        }
    };
}
