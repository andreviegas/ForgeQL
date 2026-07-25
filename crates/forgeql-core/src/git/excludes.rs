//! Which files ForgeQL refuses to commit, and the runtime-exclude block.
//!
//! Two separate policies live here. `CLEAN_COMMIT_EXCLUDED` is what a
//! user-facing commit drops; `CHECKPOINT_EXCLUDED` is the narrower set an
//! internal checkpoint drops. `ensure_runtime_excludes` writes the managed
//! block into the worktree's exclude file so git itself stops offering
//! ForgeQL's own runtime artifacts.

use std::path::Path;

use tracing::debug;

/// True when `path` is a `ForgeQL` runtime or control file and must never
/// appear in a diff or a patch.
pub(super) fn is_runtime_or_control(path: &Path) -> bool {
    is_clean_commit_excluded(path)
        || path.components().any(|c| {
            matches!(c, std::path::Component::Normal(n)
                if n.to_str().is_some_and(|s| s.starts_with(".forgeql-")))
        })
}

/// Files excluded from **user-facing** commits (`COMMIT MESSAGE`, squash).
/// The index cache is stripped so published history stays clean.
const CLEAN_COMMIT_EXCLUDED: &[&str] = &[
    ".forgeql-index",
    ".forgeql-session",
    crate::storage::columnar::DELTA_FILE_NAME,
    ".forgeql-checkpoints", // FT6: never in user-facing history
    // The armed FIND set: session runtime state, meaningless outside its
    // worktree. Written by every FIND, so without this entry every
    // post-FIND commit would ship the binary set file.
    crate::session::found_set::FILE_NAME,
];

/// Files excluded from **internal checkpoint** commits (`BEGIN TRANSACTION`).
/// The index cache is intentionally *included* so that `git reset --hard`
/// restores it automatically, giving instant rollback without re-indexing.
/// `.forgeql-staging/` holds binary segment data that is never committed —
/// GC via `DeltaFile::gc_orphaned_staging` keeps it clean on rollback.
pub(super) const CHECKPOINT_EXCLUDED: &[&str] = &[
    ".forgeql-session",
    crate::storage::columnar::STAGING_DIR_NAME,
    PATCHES_DIR_NAME,
];

/// Directory (inside a worktree) where `EXPORT PATCH` writes its mbox files.
///
/// Never committed by any path: patch files are transfer artifacts, not
/// source, and exporting them into history would nest patches in patches.
pub const PATCHES_DIR_NAME: &str = ".forgeql-patches";

pub(super) fn is_clean_commit_excluded(path: &std::path::Path) -> bool {
    // Leaf-name checks cover single control files; the staging and patch
    // directories are checked component-wise because their entries
    // (`.forgeql-staging/<hex>/…`, `.forgeql-patches/0001-….patch`) have
    // ordinary leaf names and previously (staging) slipped into user-facing
    // commits as a block of binary segment files.
    is_in_component_dir(path, crate::storage::columnar::STAGING_DIR_NAME)
        || is_in_component_dir(path, PATCHES_DIR_NAME)
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                CLEAN_COMMIT_EXCLUDED.contains(&name)
                    // The SHOW MORE ring writes `<prefix>-<n>` slot files — exclude
                    // every slot (and the legacy single-file name) by prefix.
                    || name.starts_with(crate::showmore::SHOWMORE_FILE_NAME)
                    || name.starts_with(crate::undo::UNDO_FILE_NAME)
            })
}

/// `true` when any component of `path` is the directory named `dir`.
/// Files inside runtime directories have ordinary leaf names, so the whole
/// path must be inspected, not just `file_name()`.
fn is_in_component_dir(path: &std::path::Path, dir: &str) -> bool {
    path.components()
        .any(|c| matches!(c, std::path::Component::Normal(n) if n.to_str() == Some(dir)))
}

/// Remove every index entry whose path is clean-commit excluded.
///
/// `add_all`'s exclusion callback only sees paths staged from the working
/// tree.  Entries inherited from a checkpoint commit — the undo ring's
/// `.forgeql-undo-<n>` slots are checkpoint-committed on purpose so `git
/// reset --hard` restores them — are already in the index and pass through
/// untouched, so they must be swept explicitly or they resurface in the
/// user-facing commit.
pub(super) fn purge_excluded_index_entries(index: &mut git2::Index) {
    let stale: Vec<String> = index
        .iter()
        .filter_map(|e| String::from_utf8(e.path).ok())
        .filter(|p| is_clean_commit_excluded(std::path::Path::new(p)))
        .collect();
    for p in stale {
        let _ = index.remove_path(std::path::Path::new(&p));
    }
}

pub(super) fn is_checkpoint_excluded(path: &std::path::Path) -> bool {
    // Check every path component, not just the leaf name, so that files
    // inside `.forgeql-staging/<hex>/` are excluded even though their
    // own file_name() is something like `names.col`. SHOW MORE paging
    // buffers are also kept out: committing them makes host pre-commit
    // hooks (e.g. trailing-whitespace fixers) rewrite ForgeQL's own
    // runtime state during later verify runs.
    path.components().any(|c| {
        matches!(c, std::path::Component::Normal(n)
            if n.to_str().is_some_and(|s| CHECKPOINT_EXCLUDED.contains(&s)))
    }) || path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.starts_with(crate::showmore::SHOWMORE_FILE_NAME))
}

/// Marker heading for the runtime-artifact block in `info/exclude`.
pub(super) const RUNTIME_EXCLUDE_MARKER: &str = "# ForgeQL runtime artifacts (managed block)";

/// Write `ForgeQL`'s never-committed runtime artifacts to the repository's
/// `info/exclude` so they stay out of `git status`, host pre-commit hooks,
/// and any tooling that walks untracked files.
///
/// Only artifacts that **no** commit path wants are listed. Checkpoint
/// commits intentionally include `.forgeql-index`, `.forgeql-undo`, and the
/// columnar delta so `git reset --hard` restores them — those must NOT be
/// ignored here, because `add_all` honours ignore rules.
///
/// `repo_path` is the bare repository; linked worktrees share its
/// `info/exclude` via the common git dir. Idempotent and best-effort.
pub fn ensure_runtime_excludes(repo_path: &Path) {
    let info_dir = repo_path.join("info");
    let exclude = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let patches_line = format!("{PATCHES_DIR_NAME}/");
    let updated = if existing.contains(RUNTIME_EXCLUDE_MARKER) {
        // Block already present (written by an earlier version). Entries added
        // since then are appended individually so upgrades pick them up.
        if existing.contains(&patches_line) {
            return;
        }
        format!("{}{patches_line}\n", ensure_trailing_newline(existing))
    } else {
        let block = format!(
            "{RUNTIME_EXCLUDE_MARKER}\n.forgeql-session\n{}/\n{}*\n{patches_line}\n",
            crate::storage::columnar::STAGING_DIR_NAME,
            crate::showmore::SHOWMORE_FILE_NAME,
        );
        format!("{}{block}", ensure_trailing_newline(existing))
    };
    if std::fs::create_dir_all(&info_dir).is_ok()
        && let Err(e) = std::fs::write(&exclude, updated)
    {
        debug!(path = %exclude.display(), "info/exclude not updated (non-fatal): {e}");
    }
}

/// Append a trailing newline when `text` is non-empty and lacks one.
fn ensure_trailing_newline(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}
