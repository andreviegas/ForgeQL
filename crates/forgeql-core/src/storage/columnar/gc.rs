//! Version-keyed garbage collection of the columnar cache.
//!
//! Each indexed repository accumulates versioned cache directories under
//! `<repo>.git/forgeql/overlays/<provider>-v<N>/` and
//! `<repo>.git/forgeql/segments/<provider>-v<N>/`, where `<N>` is the
//! [`ENRICH_VER`](super::ENRICH_VER) that produced them. Only the current
//! `ENRICH_VER` is live; older ones are dead weight that accumulates on every
//! bump.
//!
//! Classification keys **purely on the parsed `<N>`** — the `<provider>` prefix
//! is deliberately ignored, so a `git-sha256-v20` directory is treated exactly
//! like `git-sha1-v20`. `ENRICH_VER` is a single global sequence shared by every
//! provider (it tracks enrichment/row-content logic, not the hash scheme), so
//! comparing version numbers across providers is meaningful and no provider
//! knowledge is required here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::ENRICH_VER;

/// How a discovered version directory relates to the current [`ENRICH_VER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionClass {
    /// `N == ENRICH_VER` — the live cache.
    Current,
    /// `N > ENRICH_VER` — written by a newer binary; kept unless `all`.
    Newer,
    /// `N < ENRICH_VER` — stale; a deletion candidate.
    Older,
}

/// One `<provider>-v<N>` directory discovered directly under a cache root.
#[derive(Debug, Clone)]
pub struct VersionDir {
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// The directory's leaf name (e.g. `git-sha1-v24`).
    pub name: String,
    /// The parsed version number `N`.
    pub version: u32,
    /// Classification against the current [`ENRICH_VER`].
    pub class: VersionClass,
    /// Total size on disk, in bytes (best-effort sum of regular files).
    pub size_bytes: u64,
}

/// Knobs controlling which version directories are selected for deletion.
#[derive(Debug, Clone, Copy, Default)]
pub struct VacuumOptions {
    /// Keep the `keep` highest OLDER version numbers in addition to the current
    /// and any newer ones (default `0` — keep only current/newer).
    pub keep: usize,
    /// Delete every version, including the current and any newer ones.
    pub all: bool,
}

/// Parse the trailing `-v<N>` from a cache directory name, returning `N`.
///
/// This is the inverse of `ColumnarBuildContext::versioned_provider`, which
/// formats `<provider>-v<ENRICH_VER>`. The provider prefix is discarded — only
/// the version participates. Returns `None` for names without a `-v<digits>`
/// suffix (such names are foreign and left untouched by the caller).
#[must_use]
pub fn parse_version(name: &str) -> Option<u32> {
    let idx = name.rfind("-v")?;
    let digits = &name[idx + 2..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Classify a version number against the current [`ENRICH_VER`].
#[must_use]
pub fn classify(version: u32) -> VersionClass {
    use std::cmp::Ordering;
    match version.cmp(&ENRICH_VER) {
        Ordering::Equal => VersionClass::Current,
        Ordering::Greater => VersionClass::Newer,
        Ordering::Less => VersionClass::Older,
    }
}

/// Enumerate the `<provider>-v<N>` directories directly under one cache root.
///
/// `root` is `.../overlays` or `.../segments`; each returned directory is
/// classified and sized. Entries that are not directories, or whose names lack
/// a `-v<digits>` suffix, are ignored. A missing or unreadable root yields an
/// empty vector rather than an error — vacuuming a repo that was never indexed
/// is a no-op, not a failure.
#[must_use]
pub fn scan_cache_root(root: &Path) -> Vec<VersionDir> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(version) = parse_version(name) else {
            continue;
        };
        out.push(VersionDir {
            name: name.to_string(),
            version,
            class: classify(version),
            size_bytes: dir_size(&path),
            path,
        });
    }
    out
}

/// Recursively sum the size of all regular files under `path`. Best-effort:
/// unreadable entries are skipped rather than aborting the walk.
#[must_use]
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            total += dir_size(&entry.path());
        } else if ft.is_file()
            && let Ok(meta) = entry.metadata()
        {
            total += meta.len();
        }
    }
    total
}

/// Decide, for the version directories of ONE repository (both cache roots
/// concatenated), which are selected for deletion under `opts`. Returns the
/// indices into `dirs` that should be removed.
///
/// - `all`: every directory is selected, including current and newer.
/// - otherwise: only `Older` directories are candidates. The `keep` highest
///   older version *numbers* are retained (every directory sharing a retained
///   version number is kept — the same `v20` under two provider prefixes counts
///   as one version); all remaining older directories are selected. Current and
///   newer directories are never selected.
#[must_use]
pub fn plan_deletions(dirs: &[VersionDir], opts: VacuumOptions) -> Vec<usize> {
    if opts.all {
        return (0..dirs.len()).collect();
    }
    let mut older_versions: Vec<u32> = dirs
        .iter()
        .filter(|d| d.class == VersionClass::Older)
        .map(|d| d.version)
        .collect();
    older_versions.sort_unstable_by(|a, b| b.cmp(a));
    older_versions.dedup();
    let retained: HashSet<u32> = older_versions.into_iter().take(opts.keep).collect();

    dirs.iter()
        .enumerate()
        .filter(|(_, d)| d.class == VersionClass::Older && !retained.contains(&d.version))
        .map(|(i, _)| i)
        .collect()
}

/// What `vacuum` will do (or did) to one version directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VacuumAction {
    /// Selected for deletion (removed, when applied).
    Delete,
    /// Retained — the current version or a newer one.
    Keep,
    /// Selected for deletion but removal failed (apply only).
    Error,
}

impl VacuumAction {
    /// Stable lowercase label (`"delete"` / `"keep"` / `"error"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Keep => "keep",
            Self::Error => "error",
        }
    }
}

/// One version directory described by a [`VacuumReport`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct VacuumEntry {
    /// Source (repository) name the directory belongs to.
    pub source: String,
    /// Directory leaf name (e.g. `git-sha1-v24`).
    pub name: String,
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// Parsed version number.
    pub version: u32,
    /// Classification against the current [`ENRICH_VER`].
    pub class: VersionClass,
    /// What vacuum did / would do to this directory.
    pub action: VacuumAction,
    /// Directory size on disk, in bytes.
    pub size_bytes: u64,
}

/// The outcome of a vacuum scan (and optional apply) across one or more sources.
///
/// `entries` is the full, untruncated list (sorted deletions-first, then by
/// source and name); callers cap it for display as they see fit. The totals
/// are authoritative regardless of any display cap.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VacuumReport {
    /// Every discovered version directory across the scanned sources.
    pub entries: Vec<VacuumEntry>,
    /// Directories selected for deletion (successfully removed, when applied;
    /// removal failures are counted in `errors`, not here).
    pub delete_count: usize,
    /// Total bytes across the deleted (or to-be-deleted) directories.
    pub delete_bytes: u64,
    /// Number of sources scanned.
    pub source_count: usize,
    /// Whether the deletion was applied (`true`) or only previewed (`false`).
    pub applied: bool,
    /// Directories whose removal failed during apply.
    pub errors: usize,
}

/// Format a byte count as a human-readable size (e.g. `1.5 GiB`).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_extracts_trailing_number_ignoring_provider() {
        assert_eq!(parse_version("git-sha1-v24"), Some(24));
        assert_eq!(parse_version("git-sha256-v7"), Some(7));
        assert_eq!(parse_version("mock-v0"), Some(0));
    }

    #[test]
    fn parse_version_rejects_non_version_names() {
        assert_eq!(parse_version("git-sha1"), None);
        assert_eq!(parse_version("git-sha1-vX"), None);
        assert_eq!(parse_version("git-sha1-v"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("manifest.json"), None);
    }

    fn dir(name: &str, version: u32, class: VersionClass) -> VersionDir {
        VersionDir {
            path: PathBuf::from(name),
            name: name.to_string(),
            version,
            class,
            size_bytes: 0,
        }
    }

    #[test]
    fn default_plan_deletes_only_older_versions() {
        let dirs = vec![
            dir("a-v25", 25, VersionClass::Current),
            dir("a-v26", 26, VersionClass::Newer),
            dir("a-v24", 24, VersionClass::Older),
            dir("a-v23", 23, VersionClass::Older),
        ];
        let del = plan_deletions(&dirs, VacuumOptions::default());
        assert_eq!(del, vec![2, 3]);
    }

    #[test]
    fn keep_retains_the_n_highest_older_versions() {
        let dirs = vec![
            dir("a-v24", 24, VersionClass::Older),
            dir("a-v23", 23, VersionClass::Older),
            dir("a-v22", 22, VersionClass::Older),
        ];
        let del = plan_deletions(
            &dirs,
            VacuumOptions {
                keep: 1,
                all: false,
            },
        );
        assert_eq!(del, vec![1, 2]);
    }

    #[test]
    fn keep_counts_distinct_versions_across_provider_prefixes() {
        let dirs = vec![
            dir("git-sha1-v24", 24, VersionClass::Older),
            dir("git-sha256-v24", 24, VersionClass::Older),
            dir("git-sha1-v23", 23, VersionClass::Older),
        ];
        let del = plan_deletions(
            &dirs,
            VacuumOptions {
                keep: 1,
                all: false,
            },
        );
        assert_eq!(del, vec![2]);
    }

    #[test]
    fn all_selects_every_directory() {
        let dirs = vec![
            dir("a-v25", 25, VersionClass::Current),
            dir("a-v26", 26, VersionClass::Newer),
            dir("a-v24", 24, VersionClass::Older),
        ];
        let del = plan_deletions(&dirs, VacuumOptions { keep: 0, all: true });
        assert_eq!(del, vec![0, 1, 2]);
    }
}
