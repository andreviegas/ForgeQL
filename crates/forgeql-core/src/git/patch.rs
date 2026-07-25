//! `git am`-ready patch export for the commits a session branch carries.
//!
//! Patches are written with an exclude pathspec for `.forgeql-*` paths, so a
//! commit touching only runtime files exports nothing at all and a mixed
//! commit exports only its source part.

use std::path::Path;

use anyhow::{Context, Result};

use super::excludes::PATCHES_DIR_NAME;
use super::run_git;

// -----------------------------------------------------------------------
// EXPORT PATCH — format-patch export of session commits
// -----------------------------------------------------------------------

/// One mbox patch file produced by [`export_patches`].
#[derive(Debug, Clone)]
pub struct ExportedPatch {
    /// Absolute path of the patch file inside the worktree.
    pub path: std::path::PathBuf,
    /// File size in bytes.
    pub bytes: u64,
    /// SHA-256 of the file contents (hex) — verify after transfer with
    /// `sha256sum` before `git am`.
    pub sha256: String,
}

/// Write `git am`-ready mbox files for `range_args` into
/// `.forgeql-patches/` in `worktree` (the directory is cleared first).
///
/// Every patch is generated with an exclude pathspec for `.forgeql-*` paths
/// at any depth, so commits touching only `ForgeQL` runtime files — such as
/// transaction checkpoints — produce no patch at all, and commits mixing
/// source and runtime files export only their source part. `--binary`
/// includes base85 literal data so binary files survive `git am`.
///
/// `range_args` is either `["<oid>..HEAD"]` or `["-<n>", "HEAD"]`, always
/// engine-computed — never user text.
///
/// # Errors
/// Returns an error when the output directory cannot be cleared, git cannot
/// be spawned, `format-patch` fails, or a produced file cannot be read back.
pub fn export_patches(worktree: &Path, range_args: &[String]) -> Result<Vec<ExportedPatch>> {
    use sha2::{Digest, Sha256};

    let out_dir = worktree.join(PATCHES_DIR_NAME);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)
            .with_context(|| format!("could not clear {}", out_dir.display()))?;
    }

    let mut args: Vec<&str> = vec!["format-patch", "--binary", "-o", PATCHES_DIR_NAME];
    args.extend(range_args.iter().map(String::as_str));
    args.extend(["--", ":(exclude,glob)**/.forgeql-*"]);
    let stdout = run_git(worktree, &args)?;

    // format-patch prints one created file per line, in series order.
    let mut files = Vec::new();
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let abs = worktree.join(line);
        let data =
            std::fs::read(&abs).with_context(|| format!("could not read {}", abs.display()))?;
        let bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let sha256 = format!("{:x}", Sha256::digest(&data));
        files.push(ExportedPatch {
            path: abs,
            bytes,
            sha256,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use crate::git::{head_oid_of, merge_base_with, uncommitted_source_changes};

    use super::*;
    use crate::git::testutil::{make_normal_repo, raw_commit_all};
    /// The transaction-safety contract of EXPORT PATCH: commits touching only
    /// `ForgeQL` runtime files (transaction checkpoints) produce no patch, and
    /// commits mixing source with runtime files export only the source part.
    #[test]
    fn export_patches_excludes_runtime_files_and_checkpoint_commits() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = make_normal_repo(dir.path());

        std::fs::write(dir.path().join("file.cpp"), b"int main(){return 1;}\n").unwrap();
        raw_commit_all(dir.path(), "user change 1");

        // Checkpoint-style commit: runtime files only (top-level and nested).
        std::fs::write(dir.path().join(".forgeql-index"), b"idx").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.forgeql-columnar-delta"), b"d").unwrap();
        raw_commit_all(dir.path(), "forgeql: checkpoint 'txn'");

        // Mixed commit: source + runtime file in one commit.
        std::fs::write(dir.path().join("file.cpp"), b"int main(){return 2;}\n").unwrap();
        std::fs::write(dir.path().join(".forgeql-index"), b"idx2").unwrap();
        raw_commit_all(dir.path(), "user change 2");

        // Explicit range (the session merge-base..HEAD form): 3 commits in
        // range, the checkpoint-only one drops out of the series entirely.
        let files = export_patches(dir.path(), &["HEAD~3..HEAD".to_string()]).unwrap();
        assert_eq!(
            files.len(),
            2,
            "checkpoint-only commit must produce no patch"
        );
        for f in &files {
            assert!(f.bytes > 0);
            assert_eq!(f.sha256.len(), 64, "sha256 hex digest");
            let text = std::fs::read_to_string(&f.path).unwrap();
            assert!(
                !text.contains(".forgeql-"),
                "runtime files leaked into {}",
                f.path.display()
            );
        }

        // `-<n>` counts pathspec-matching commits, so LAST n means the last
        // n commits that touched source — checkpoints never consume the
        // count. -1 therefore yields the mixed commit's source part.
        let last = export_patches(dir.path(), &["-1".to_string(), "HEAD".to_string()]).unwrap();
        assert_eq!(last.len(), 1);
        let text = std::fs::read_to_string(&last[0].path).unwrap();
        assert!(text.contains("user change 2"));
        assert!(!text.contains(".forgeql-"));

        // Re-running cleared the directory instead of accumulating series.
        let on_disk = std::fs::read_dir(dir.path().join(PATCHES_DIR_NAME))
            .unwrap()
            .count();
        assert_eq!(on_disk, 1, "stale patches from earlier exports removed");
    }
    /// Range helpers and the uncommitted-changes probe used by EXPORT PATCH.
    #[test]
    fn export_patch_range_helpers_and_dirty_probe() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = make_normal_repo(dir.path());
        run_git(dir.path(), &["branch", "base"]).unwrap();

        assert_eq!(uncommitted_source_changes(dir.path()).unwrap(), 0);

        // Runtime-only dirt is invisible to the probe; source dirt counts.
        std::fs::write(dir.path().join(".forgeql-session"), b"s").unwrap();
        assert_eq!(uncommitted_source_changes(dir.path()).unwrap(), 0);
        std::fs::write(dir.path().join("new.cpp"), b"int x;\n").unwrap();
        assert_eq!(uncommitted_source_changes(dir.path()).unwrap(), 1);

        // No commits over the base branch: merge-base == HEAD.
        let mb = merge_base_with(dir.path(), "base").unwrap();
        let head = head_oid_of(dir.path()).unwrap();
        assert_eq!(mb, head);

        // One commit later the range opens up.
        raw_commit_all(dir.path(), "work");
        let mb2 = merge_base_with(dir.path(), "base").unwrap();
        assert_eq!(mb2, mb, "merge-base stays at the fork point");
        assert_ne!(head_oid_of(dir.path()).unwrap(), mb2);
    }
}
