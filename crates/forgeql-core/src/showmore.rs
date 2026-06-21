//! Server-side output buffer backing the `SHOW MORE` command.
//!
//! Any command whose rendered output exceeds the inline cap stores the full
//! result here and returns only a window plus a hint. The agent then retrieves
//! the remainder with `SHOW MORE [HEAD n | TAIL n | n-m]` without re-running the
//! producing command — most valuably, the full `VERIFY build` log can be
//! grep-filtered (`SHOW MORE WHERE text MATCHES 'error|fail'`) without a rebuild.
//!
//! # Storage and lifecycle
//!
//! The buffer is a single file (`.forgeql-showmore`) written into the session
//! worktree beside `.forgeql-columnar-delta`. It holds the most recent buffered
//! output for the session and is overwritten on each buffered command. Being in
//! the worktree, it participates in the transaction model exactly like the
//! columnar delta: it is included in `BEGIN TRANSACTION` checkpoints (so a
//! `ROLLBACK`'s `git reset --hard` restores the pre-transaction buffer) and
//! excluded from user-facing `COMMIT` squashes (so it never reaches published
//! history). See `git::CHECKPOINT_EXCLUDED` / `git::CLEAN_COMMIT_EXCLUDED`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Filename prefix of the per-session `SHOW MORE` ring buffers. Each slot is
/// written in the worktree root as `<prefix>-<n>` (`LAST-<n>`, 0 = most recent).
pub const SHOWMORE_FILE_NAME: &str = ".forgeql-showmore";

/// Depth of the `LAST-n` ring: slots `LAST-0` (most recent) .. `LAST-<N-1>`.
/// A buffered write pushes existing slots back one and writes the new `LAST-0`.
pub const RING_SIZE: usize = 5;

/// First-line marker identifying a valid buffer file and its format version.
const HEADER_MARKER: &str = "FQLSHOWMORE\tv1";

/// Default direction a bare `SHOW MORE` pages, recorded per buffered command.
///
/// Read-oriented output (`FIND`, `SHOW body`) reads top-down, so it defaults to
/// [`Direction::Head`]; build/log output puts the verdict last, so `VERIFY`
/// defaults to [`Direction::Tail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Page from the start of the buffer (first lines first).
    Head,
    /// Page from the end of the buffer (last lines first).
    Tail,
}

impl Direction {
    /// Wire token used in the buffer header.
    const fn as_token(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Tail => "tail",
        }
    }

    /// Parse a header token, defaulting to [`Direction::Head`] on anything else.
    fn from_token(token: &str) -> Self {
        if token == "tail" {
            Self::Tail
        } else {
            Self::Head
        }
    }
}

/// Which slice of a buffer a `SHOW MORE` invocation requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// No window given — return the whole buffer (bypasses the inline cap, the
    /// same rule an explicit `SHOW LINES n-m` range already follows).
    Full,
    /// `HEAD n` — the first `n` lines.
    Head(usize),
    /// `TAIL n` — the last `n` lines.
    Tail(usize),
    /// `n-m` — the 1-based inclusive line range, clamped to the buffer bounds.
    Range(usize, usize),
}

/// A parsed `SHOW MORE` buffer: metadata plus the full set of output lines.
#[derive(Debug, Clone)]
pub struct Buffer {
    /// Direction a bare `SHOW MORE` pages, set by the producing command.
    pub default_dir: Direction,
    /// Short label naming the command that produced the buffer (e.g. `verify_build`).
    pub label: String,
    /// The full output, one entry per line, in original order.
    pub lines: Vec<String>,
}

impl Buffer {
    /// Total number of buffered lines.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.lines.len()
    }

    /// Resolve a [`Selection`] into 1-based-indexed lines (index, text).
    ///
    /// Indices are the line's original position in the full buffer, preserved
    /// across windowing so the agent can request an exact follow-up range.
    #[must_use]
    pub fn window(&self, sel: Selection) -> Vec<(usize, &str)> {
        let n = self.lines.len();
        let (lo, hi) = match sel {
            Selection::Full => (0, n),
            Selection::Head(k) => (0, k.min(n)),
            Selection::Tail(k) => (n.saturating_sub(k), n),
            Selection::Range(a, b) => {
                // 1-based inclusive → 0-based half-open, clamped.
                let lo = a.saturating_sub(1).min(n);
                let hi = b.min(n);
                (lo, hi.max(lo))
            }
        };
        self.lines[lo..hi]
            .iter()
            .enumerate()
            .map(|(i, text)| (lo + i + 1, text.as_str()))
            .collect()
    }
}

/// Absolute path of the buffer file for a session worktree.
#[must_use]
pub fn slot_path(worktree_root: &Path, n: usize) -> PathBuf {
    worktree_root.join(format!("{SHOWMORE_FILE_NAME}-{n}"))
}

/// Write `lines` as the session's current `SHOW MORE` buffer.
///
/// `label` names the producing command and `default_dir` records how a bare
/// `SHOW MORE` should page. Overwrites any existing buffer.
///
/// # Errors
/// Returns the underlying I/O error if the file cannot be written.
pub fn write_buffer(
    worktree_root: &Path,
    default_dir: Direction,
    label: &str,
    lines: &[&str],
) -> std::io::Result<()> {
    // Push the existing buffers back one slot so this write becomes LAST-0; the
    // previous LAST-0 becomes LAST-1, and so on (git-style ring).
    rotate_ring(worktree_root);

    // Header carries the version marker, default direction, line count, and a
    // single-line label (newlines stripped so the header stays one line).
    let safe_label = label.replace(['\n', '\r'], " ");
    let mut buf = format!(
        "{HEADER_MARKER}\t{}\t{}\t{safe_label}\n",
        default_dir.as_token(),
        lines.len(),
    );
    for line in lines {
        buf.push_str(line);
        buf.push('\n');
    }
    std::fs::write(slot_path(worktree_root, 0), buf)
}

/// Shift every ring slot up by one (dropping the oldest) so a fresh `LAST-0`
/// can be written. Best-effort: a missing slot is skipped and a failed rename
/// only costs a stale page — the ring is a throwaway paging cache.
fn rotate_ring(worktree_root: &Path) {
    let _ = std::fs::remove_file(slot_path(worktree_root, RING_SIZE - 1));
    for n in (0..RING_SIZE - 1).rev() {
        let from = slot_path(worktree_root, n);
        if from.exists() {
            let _ = std::fs::rename(&from, slot_path(worktree_root, n + 1));
        }
    }
}

/// Read and parse the session's current `SHOW MORE` buffer, if one exists.
///
/// Returns `Ok(None)` when no buffer file is present or the file does not carry
/// a recognised header (treated as "no buffer" rather than an error).
///
/// # Errors
/// Returns the underlying I/O error for failures other than a missing file.
pub fn read_buffer(worktree_root: &Path) -> std::io::Result<Option<Buffer>> {
    read_buffer_n(worktree_root, 0)
}

/// Read and parse ring slot `n` (`LAST-<n>`, 0 = most recent), if present.
///
/// Returns `Ok(None)` when the slot file is absent or lacks a recognised
/// header (treated as "no buffer" rather than an error).
///
/// # Errors
/// Returns the underlying I/O error for failures other than a missing file.
pub fn read_buffer_n(worktree_root: &Path, n: usize) -> std::io::Result<Option<Buffer>> {
    let path = slot_path(worktree_root, n);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut iter = text.split('\n');
    let Some(header) = iter.next() else {
        return Ok(None);
    };
    let mut fields = header.splitn(5, '\t');
    // fields: "FQLSHOWMORE", "v1", "<dir>", "<count>", "<label>"
    if fields.next() != Some("FQLSHOWMORE") || fields.next() != Some("v1") {
        return Ok(None);
    }
    let default_dir = Direction::from_token(fields.next().unwrap_or("head"));
    let count: usize = fields.next().and_then(|c| c.parse().ok()).unwrap_or(0);
    let label = fields.next().unwrap_or_default().to_string();
    // Take exactly `count` content lines after the header; ignore any trailing
    // bytes so a partially-written file never yields a stray empty line.
    let lines: Vec<String> = iter.take(count).map(str::to_string).collect();
    Ok(Some(Buffer {
        default_dir,
        label,
        lines,
    }))
}

/// Outcome of [`finalize`].
#[derive(Debug, Clone)]
pub struct Finalized {
    /// The inline output: a window of the rendered text when buffered, or the
    /// rendered text unchanged when it fit within the cap.
    pub text: String,
    /// `true` when the output exceeded the cap and the full text was buffered.
    pub buffered: bool,
}

/// Window an over-cap rendered output and persist the full text for `SHOW MORE`.
///
/// When `rendered` has at most `cap` lines it is returned unchanged and no
/// buffer is written. Otherwise the full output is written to the session
/// buffer and a `cap`-line window (the head or tail per `default_dir`) plus a
/// one-line hint pointing at `SHOW MORE` is returned. A `cap` of 0 is treated
/// as 1 so at least one line is always shown.
///
/// # Errors
/// Returns the underlying I/O error if the buffer cannot be written.
pub fn finalize(
    worktree_root: &Path,
    rendered: &str,
    label: &str,
    default_dir: Direction,
    cap: usize,
) -> std::io::Result<Finalized> {
    let cap = cap.max(1);
    let lines: Vec<&str> = rendered.split('\n').collect();
    let total = lines.len();
    if total <= cap {
        return Ok(Finalized {
            text: rendered.to_string(),
            buffered: false,
        });
    }
    write_buffer(worktree_root, default_dir, label, &lines)?;
    let (slice, first, last) = match default_dir {
        Direction::Head => (&lines[..cap], 1, cap),
        Direction::Tail => (&lines[total - cap..], total - cap + 1, total),
    };
    let dir_word = match default_dir {
        Direction::Head => "first",
        Direction::Tail => "last",
    };
    let mut text = slice.join("\n");
    let _ = write!(
        text,
        "\n\"show_more\",\"{total} lines total; showing {dir_word} {cap} ({first}-{last}). \
         SHOW MORE 1-{total} for all, HEAD n | TAIL n for an end, or \
         WHERE text MATCHES '…' to filter.\""
    );
    Ok(Finalized {
        text,
        buffered: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static TMP_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn write_and_read(dir: Direction, label: &str, lines: &[&str]) -> Buffer {
        // Unique per call (pid + monotonic counter) so parallel tests never
        // share a temp dir and race on cleanup.
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("fql-showmore-test-{}-{seq}", std::process::id(),));
        std::fs::create_dir_all(&tmp).unwrap();
        write_buffer(&tmp, dir, label, lines).unwrap();
        let buf = read_buffer(&tmp).unwrap().unwrap();
        std::fs::remove_dir_all(&tmp).ok();
        buf
    }

    #[test]
    fn roundtrips_lines_and_metadata() {
        let buf = write_and_read(Direction::Tail, "verify_build", &["a", "b", "c"]);
        assert_eq!(buf.default_dir, Direction::Tail);
        assert_eq!(buf.label, "verify_build");
        assert_eq!(buf.lines, vec!["a", "b", "c"]);
        assert_eq!(buf.total(), 3);
    }

    #[test]
    fn missing_buffer_is_none() {
        let tmp = std::env::temp_dir().join(format!("fql-showmore-absent-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(read_buffer(&tmp).unwrap().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn preserves_lines_containing_commas_and_quotes() {
        // Buffered content is rendered CSV, which contains commas and quotes;
        // only newlines are structural, so everything else must round-trip.
        let line = "42,\"if (x == 1) { return; }\"";
        let buf = write_and_read(Direction::Head, "show_body", &[line]);
        assert_eq!(buf.lines, vec![line]);
    }

    #[test]
    fn window_head_tail_and_range() {
        let buf = write_and_read(
            Direction::Head,
            "find_symbols",
            &["l1", "l2", "l3", "l4", "l5"],
        );
        assert_eq!(buf.window(Selection::Head(2)), vec![(1, "l1"), (2, "l2")]);
        assert_eq!(buf.window(Selection::Tail(2)), vec![(4, "l4"), (5, "l5")]);
        assert_eq!(
            buf.window(Selection::Range(2, 4)),
            vec![(2, "l2"), (3, "l3"), (4, "l4")]
        );
        assert_eq!(buf.window(Selection::Full).len(), 5);
    }

    #[test]
    fn window_clamps_out_of_bounds() {
        let buf = write_and_read(Direction::Head, "find_symbols", &["l1", "l2"]);
        assert_eq!(buf.window(Selection::Head(99)).len(), 2);
        assert_eq!(buf.window(Selection::Tail(99)).len(), 2);
        assert_eq!(buf.window(Selection::Range(5, 9)), vec![]);
        assert_eq!(buf.window(Selection::Range(2, 99)), vec![(2, "l2")]);
    }

    fn tmp_dir() -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("fql-finalize-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn finalize_under_cap_is_passthrough() {
        let tmp = tmp_dir();
        let rendered = "a\nb\nc";
        let out = finalize(&tmp, rendered, "find_symbols", Direction::Head, 40).unwrap();
        assert!(!out.buffered);
        assert_eq!(out.text, rendered);
        assert!(read_buffer(&tmp).unwrap().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn finalize_over_cap_tail_buffers_full_and_windows() {
        let tmp = tmp_dir();
        let rendered = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = finalize(&tmp, &rendered, "verify_build", Direction::Tail, 3).unwrap();
        assert!(out.buffered);
        // Inline window is the last 3 lines plus a hint row.
        assert!(out.text.contains("line8"));
        assert!(out.text.contains("line10"));
        assert!(!out.text.contains("line7"));
        assert!(out.text.contains("show_more"));
        // The buffer holds the full 10 lines for SHOW MORE.
        let buf = read_buffer(&tmp).unwrap().unwrap();
        assert_eq!(buf.total(), 10);
        assert_eq!(buf.default_dir, Direction::Tail);
        assert_eq!(buf.window(Selection::Head(1)), vec![(1, "line1")]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn finalize_over_cap_head_shows_first_lines() {
        let tmp = tmp_dir();
        let rendered = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = finalize(&tmp, &rendered, "find_symbols", Direction::Head, 2).unwrap();
        assert!(out.buffered);
        assert!(out.text.contains("line1"));
        assert!(out.text.contains("line2"));
        assert!(!out.text.contains("line3\n"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn ring_pages_previous_buffers_as_last_n() {
        let tmp = tmp_dir();
        write_buffer(&tmp, Direction::Head, "first", &["a1"]).unwrap();
        write_buffer(&tmp, Direction::Head, "second", &["b1", "b2"]).unwrap();

        // LAST-0 is the most recent write; LAST-1 is the one before it.
        let last0 = read_buffer_n(&tmp, 0).unwrap().unwrap();
        assert_eq!(last0.label, "second");
        assert_eq!(last0.lines, vec!["b1", "b2"]);
        let last1 = read_buffer_n(&tmp, 1).unwrap().unwrap();
        assert_eq!(last1.label, "first");
        assert_eq!(last1.lines, vec!["a1"]);
        // A bare read_buffer is LAST-0.
        assert_eq!(read_buffer(&tmp).unwrap().unwrap().label, "second");

        // The ring is bounded to RING_SIZE slots.
        for i in 0..RING_SIZE {
            write_buffer(&tmp, Direction::Head, &format!("w{i}"), &["x"]).unwrap();
        }
        assert!(read_buffer_n(&tmp, RING_SIZE - 1).unwrap().is_some());
        assert!(read_buffer_n(&tmp, RING_SIZE).unwrap().is_none());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
