//! CSV query logger.
//!
//! Appends one CSV row per executed FQL statement to
//! `{data_dir}/log/{source}.csv`.
//!
//! The file is created (with a header row) automatically on first write.
//! All subsequent writes are append-only so the log survives process
//! restarts without duplication.
//!
//! CSV columns:
//! `timestamp`, `elapsed_ms`, `source_lines`, `tokens_sent`, `tokens_received`, `command_preview`

use std::io::Write;
use std::path::PathBuf;

use crate::result::ForgeQLResult;

/// Maximum characters kept in the CSV log command preview column.
const LOG_PREVIEW_MAX_CHARS: usize = 160;

/// Approximate number of UTF-8 characters per LLM token (used for
/// rough token-count estimates in the query log).
const CHARS_PER_TOKEN: usize = 4;

/// CSV query logger that records every FQL statement execution to disk.
pub struct QueryLogger {
    data_dir: PathBuf,
    /// Sanitized source name — used as the CSV filename stem.
    source: String,
}

impl QueryLogger {
    /// Create a new logger that writes to `{data_dir}/log/`.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            source: "unknown".to_string(),
        }
    }

    /// Update the source name once a `USE source.branch` succeeds.
    pub fn set_source(&mut self, source: &str) {
        self.source = source
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
    }

    /// Return the path to the log CSV file.
    #[must_use]
    pub fn log_path(&self) -> PathBuf {
        self.data_dir
            .join("log")
            .join(format!("{}.csv", self.source))
    }

    /// Append one CSV row for the completed FQL statement.
    ///
    /// `fql`           — the raw statement text.
    /// `result`        — the typed result, used to count disclosed source lines.
    /// `result_output` — the serialized output string, used to estimate token usage.
    /// `elapsed_ms`    — wall-clock milliseconds to execute the command.
    pub fn log(&self, fql: &str, result: &ForgeQLResult, result_output: &str, elapsed_ms: u64) {
        let log_dir = self.data_dir.join("log");
        if std::fs::create_dir_all(&log_dir).is_err() {
            return;
        }
        let log_path = log_dir.join(format!("{}.csv", self.source));

        let needs_header = !log_path.exists();
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        else {
            return;
        };
        if needs_header {
            let _ = writeln!(
                file,
                "timestamp,elapsed_ms,source_lines,tokens_sent,tokens_received,command_preview"
            );
        }

        let flat: String = fql
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let preview: String = flat
            .chars()
            .take(LOG_PREVIEW_MAX_CHARS)
            .collect::<String>()
            .replace('"', "\"\"");

        let source_lines = result.source_lines_count();
        let tokens_sent = fql.len().div_ceil(CHARS_PER_TOKEN);
        let tokens_received = result_output.len().div_ceil(CHARS_PER_TOKEN);

        let _ = writeln!(
            file,
            r#""{}",{},{},{},{},"{}""#,
            iso_timestamp(),
            elapsed_ms,
            source_lines,
            tokens_sent,
            tokens_received,
            preview,
        );
    }
}

/// Return the current UTC time as an ISO 8601–style string (`YYYY-MM-DD HH:MM:SS`).
fn iso_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = epoch_to_datetime(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Decompose a Unix epoch timestamp into `(year, month, day, hour, minute, second)`.
///
/// Algorithm: <https://howardhinnant.github.io/date_algorithms.html>
#[allow(clippy::many_single_char_names)]
#[allow(clippy::cast_possible_truncation)]
const fn epoch_to_datetime(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mi = ((secs / 60) % 60) as u32;
    let h = ((secs / 3_600) % 24) as u32;

    let z = secs / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y0 + 1 } else { y0 } as u32;

    (y, mo, d, h, mi, s)
}
