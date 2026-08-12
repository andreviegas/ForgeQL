//! Resident-memory readings, stamped beside the indexing `TIMING` logs.
//!
//! Indexing a large corpus has a peak that nothing here used to measure. The
//! `TIMING` lines said which phase was slow and never which phase was large,
//! so a report of "the reindex used 25 GB" could not be attributed to a phase
//! without guessing, and every fix aimed at it was a guess too.
//!
//! **The split between the two resident numbers is the point, not the total.**
//! `anon` is heap and thread stacks — memory this process owns, that no one
//! else can reclaim, and that the OOM killer counts. `file` is pages backed by
//! a file, which for `ForgeQL` is overwhelmingly the mmapped `.fqsf` segments:
//! it is page cache, it is shared between every process and session that
//! mapped the same segment, and the kernel drops it under pressure without
//! telling anyone. A peak that is mostly `file` is not a memory problem, and
//! reading only the total cannot tell the two apart — `top`, `ps` and the RSS
//! column of every process viewer report their sum.
//!
//! `file` is also inflated by construction: it is counted per page-table
//! entry, so N mappings of one segment in one process count N times. Measured
//! on a mid-size corpus, the file-backed RSS read 2,365 MB where the
//! proportional-set figure that divides each page by its sharers read 638 MB.
//! Treat it as an upper bound on a number that is not the one that matters.

use std::fmt;

/// One reading of this process's resident memory.
///
/// [`Unavailable`](Self::Unavailable) is not an error worth propagating: the
/// reading is diagnostic, every caller is a log line, and a platform that does
/// not publish these numbers should log the phase without them rather than
/// fail the indexing run that was the actual work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemSnapshot {
    /// Kilobyte counts as the kernel reported them.
    Read {
        /// Anonymous resident memory: heap and thread stacks. The number that
        /// grows with the work and the number an OOM kill is decided on.
        anon_kb: u64,
        /// File-backed resident memory: mapped segments, and the binary
        /// itself. Shared and reclaimable — see the module docs before
        /// treating a large value here as consumption.
        file_kb: u64,
        /// Peak resident set size since this process started (`VmHWM`), the
        /// sum of both kinds.
        ///
        /// Monotonic and never reset, so across several indexing runs in one
        /// long-lived process only the first run's peak is its own. What still
        /// attributes a phase in later runs is the movement of `anon_kb`
        /// between consecutive lines.
        peak_kb: u64,
    },
    /// The kernel does not publish these numbers here, or they could not be
    /// read.
    Unavailable,
}

impl fmt::Display for MemSnapshot {
    /// Renders as `anon=1.2GiB file=3.4GiB peak=5.0GiB`, in the order the
    /// module docs argue they should be read: what this process owns first.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Read {
                anon_kb,
                file_kb,
                peak_kb,
            } => write!(
                f,
                "anon={} file={} peak={}",
                Gib(anon_kb),
                Gib(file_kb),
                Gib(peak_kb)
            ),
            Self::Unavailable => write!(f, "unavailable"),
        }
    }
}

/// Kilobytes rendered in the largest unit that keeps the number readable.
struct Gib(u64);

impl fmt::Display for Gib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a memory reading is displayed to three significant figures; \
                      the lost bits are far below the precision of the reading itself"
        )]
        let kb = self.0 as f64;
        if self.0 >= 1024 * 1024 {
            write!(f, "{:.1}GiB", kb / (1024.0 * 1024.0))
        } else if self.0 >= 1024 {
            write!(f, "{:.0}MiB", kb / 1024.0)
        } else {
            write!(f, "{}KiB", self.0)
        }
    }
}

/// Read this process's resident memory now.
///
/// One `read` of a small pseudo-file per call. Callers are phase boundaries in
/// the indexing path — a few dozen times per build — so the cost is not worth
/// a cache that could hand back a stale reading.
#[must_use]
pub fn snapshot() -> MemSnapshot {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return MemSnapshot::Unavailable;
    };
    parse_status(&status)
}

/// The parsing half of [`snapshot`], split out so it can be tested against a
/// known blob rather than against whatever this machine happens to report.
///
/// Every field must be present: a partial reading would render as a memory
/// profile with a zero in it, which reads as a measurement rather than as a
/// missing one.
fn parse_status(status: &str) -> MemSnapshot {
    let (mut anon_kb, mut file_kb, mut peak_kb) = (None, None, None);
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "RssAnon" => &mut anon_kb,
            "RssFile" => &mut file_kb,
            "VmHWM" => &mut peak_kb,
            _ => continue,
        };
        // "RssAnon:\t  123456 kB" — the unit is always kB on the fields read
        // here, so the number is the only token that has to be understood.
        *slot = value.split_whitespace().next().and_then(|n| n.parse().ok());
    }
    match (anon_kb, file_kb, peak_kb) {
        (Some(anon_kb), Some(file_kb), Some(peak_kb)) => MemSnapshot::Read {
            anon_kb,
            file_kb,
            peak_kb,
        },
        _ => MemSnapshot::Unavailable,
    }
}

#[cfg(test)]
mod tests;
