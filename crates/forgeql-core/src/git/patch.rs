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
