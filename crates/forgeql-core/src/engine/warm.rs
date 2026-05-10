//! Phase 05 — background warming of segments and overlays.
//!
//! `CREATE SOURCE` and `REFRESH SOURCE` return immediately after the git
//! clone / fetch.  The first `USE` then pays the full cost of building the
//! legacy index, the columnar segments and the overlay (~10–30 s on large
//! repos).
//!
//! When `ColumnarConfig::warm_on_create` / `warm_on_refresh` is enabled,
//! [`spawn_warmer`] runs that work in a background thread immediately after
//! the source op returns, so the first `USE` lands on a warm cache and only
//! pays the columnar load cost (~50–200 ms).
//!
//! ## Defaults
//!
//! Both knobs default to `enabled: false` in this phase.  Phase 08 flips the
//! defaults once benchmarks confirm the background load is benign on
//! multi-source servers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::ast::lang::LanguageRegistry;
use crate::config::{WarmPolicy, WarmPolicyKind};
use crate::git::{source::Source, worktree};
use crate::session::Session;
use crate::storage::HashFn;
use crate::storage::columnar::overlay::Overlay;
use crate::storage::git_sha1_provider::git_blob_sha1;

/// One snapshot to warm — a (`branch`, `commit_sha`) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmTarget {
    /// Local branch name.
    pub branch: String,
    /// Full commit SHA the branch HEAD pointed to when the target was picked.
    pub commit_sha: String,
}

/// Decide which snapshots to warm for a given source under `policy`.
///
/// Pure function — no I/O is performed beyond what `Source::branch_heads()`
/// already does.  Returns an empty vec when `policy.enabled == false` or
/// `policy.policy == Off`.
///
/// # Errors
/// Returns `Err` only when listing branch HEADs from `source` fails.
pub fn pick_warm_targets(source: &Source, policy: &WarmPolicy) -> Result<Vec<WarmTarget>> {
    if !policy.enabled || matches!(policy.policy, WarmPolicyKind::Off) {
        return Ok(Vec::new());
    }
    let heads = source.branch_heads()?;
    let mut targets: Vec<WarmTarget> = match policy.policy {
        WarmPolicyKind::Off => Vec::new(),
        WarmPolicyKind::DefaultBranch => {
            let default = source.default_branch()?;
            heads
                .get(&default)
                .map(|sha| {
                    vec![WarmTarget {
                        branch: default.clone(),
                        commit_sha: sha.clone(),
                    }]
                })
                .unwrap_or_default()
        }
        WarmPolicyKind::AllBranches => heads
            .iter()
            .map(|(b, sha)| WarmTarget {
                branch: b.clone(),
                commit_sha: sha.clone(),
            })
            .collect(),
        WarmPolicyKind::Pinned => policy
            .pinned
            .iter()
            .filter_map(|b| {
                heads.get(b).map(|sha| WarmTarget {
                    branch: b.clone(),
                    commit_sha: sha.clone(),
                })
            })
            .collect(),
    };
    // Deterministic order for tests and reproducible logs.
    targets.sort_by(|a, b| a.branch.cmp(&b.branch));
    Ok(targets)
}

/// Spawn a detached background thread that warms each `target` in `targets`.
///
/// The thread is named `forgeql-warm-<source>` and detaches; warm failures
/// are logged at WARN level but never propagated.  Returns immediately.
///
/// `max_concurrent` is honoured serially in this phase — targets are processed
/// one at a time inside the warmer thread.  Phase 08 may parallelise.
pub fn spawn_warmer(
    bare_repo: PathBuf,
    source_name: String,
    targets: Vec<WarmTarget>,
    data_dir: PathBuf,
    lang_registry: Arc<LanguageRegistry>,
) {
    if targets.is_empty() {
        return;
    }
    let thread_name = format!("forgeql-warm-{source_name}");
    let log_name = source_name.clone();
    let spawn_result = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            info!(
                source = %source_name,
                count = targets.len(),
                "warmer thread started"
            );
            for target in &targets {
                match warm_snapshot(&bare_repo, &source_name, target, &data_dir, &lang_registry) {
                    Ok(()) => debug!(
                        source = %source_name,
                        branch = %target.branch,
                        commit = %target.commit_sha,
                        "warm complete"
                    ),
                    Err(e) => warn!(
                        source = %source_name,
                        branch = %target.branch,
                        commit = %target.commit_sha,
                        "warm failed (non-fatal): {e}"
                    ),
                }
            }
            info!(source = %source_name, "warmer thread done");
        });
    if let Err(e) = spawn_result {
        warn!(source_name = %log_name, "failed to spawn warmer thread (non-fatal): {e}");
    }
}

/// Build segments + overlay for one snapshot in a throwaway worktree.
///
/// Mirrors the body of [`super::ForgeQLEngine::use_source`]'s overlay-build
/// path, minus the user-visible session.  Idempotent: if the overlay already
/// exists and opens cleanly the function returns immediately.
///
/// The throwaway worktree is created under
/// `<data_dir>/worktrees/__warm__<source>__<sha12>` and removed on exit.
///
/// # Errors
/// Returns `Err` when the worktree cannot be created or when
/// `Session::resume_index()` fails — both indicate a broken git state that
/// would also cause a real `USE` to fail.
pub fn warm_snapshot(
    bare_repo: &Path,
    source_name: &str,
    target: &WarmTarget,
    data_dir: &Path,
    lang_registry: &Arc<LanguageRegistry>,
) -> Result<()> {
    // Fast path: overlay already exists and opens cleanly.
    let overlay_path = bare_repo
        .join("forgeql")
        .join("overlays")
        .join("git-sha1")
        .join(format!("{}.bin", target.commit_sha));
    if overlay_path.exists() && Overlay::open(&overlay_path).is_ok() {
        debug!(
            source = %source_name,
            commit = %target.commit_sha,
            "warm_snapshot: overlay already present, skipping"
        );
        return Ok(());
    }

    let sha12: String = target.commit_sha.chars().take(12).collect();
    let safe_branch = target.branch.replace('/', "-");
    let safe_source = source_name.replace('/', "-");
    let wt_name = format!("__warm__{safe_source}__{safe_branch}__{sha12}");
    let wt_path = data_dir.join("worktrees").join(&wt_name);
    let session_branch = format!("fql/__warm__/{safe_branch}/{sha12}");

    let cleanup_path = wt_path.clone();
    let cleanup_repo = bare_repo.to_path_buf();
    let cleanup_name = wt_name.clone();
    let result: Result<()> = (|| {
        drop(worktree::create(
            bare_repo,
            &wt_name,
            &target.branch,
            &wt_path,
            Some(&session_branch),
        )?);

        let session_id = format!("__warm__{source_name}__{}", target.commit_sha);
        let mut session = Session::new(
            &session_id,
            "warmer",
            wt_path.clone(),
            source_name,
            &target.branch,
            lang_registry,
        );

        // Configure shadow-write so build_index emits segments and overlay.
        let segments_dir = bare_repo.join("forgeql").join("segments");
        let overlays_dir = bare_repo.join("forgeql").join("overlays");
        let hash_fn: HashFn = Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec());
        session.set_columnar_build(crate::storage::ColumnarBuildContext::new(
            segments_dir,
            overlays_dir,
            "git-sha1",
            hash_fn,
        ));

        // build_index rebuilds the legacy index; warm_or_open then builds
        // the columnar segments and overlay (locking internally).
        session.build_index()?;
        if let Some(ctx) = session.columnar_build().cloned() {
            let _ = crate::storage::columnar::ColumnarStorage::warm(
                &ctx,
                session.legacy_storage(),
                wt_path.clone(),
                &target.commit_sha,
            );
        }
        Ok(())
    })();

    // Best-effort cleanup of the throwaway worktree regardless of outcome.
    if cleanup_path.exists()
        && let Err(e) = worktree::remove(&cleanup_repo, &cleanup_name)
    {
        debug!(name = %cleanup_name, "warm_snapshot: worktree cleanup failed (non-fatal): {e}");
        let _ = std::fs::remove_dir_all(&cleanup_path);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WarmPolicyKind;
    use git2::{Repository, Signature};
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Build a tiny bare repo with branches `main`, `dev`, `feature`, each
    /// pointing at distinct commits.  Returns the source.
    fn make_test_source() -> (Source, TempDir) {
        let td = TempDir::new().unwrap();
        let bare = td.path().join("repo.git");
        let repo = Repository::init_bare(&bare).unwrap();
        let sig = Signature::now("t", "t@example.com").unwrap();

        // Empty tree as the initial tree.
        let empty_tree_id = {
            let mut tb = repo.treebuilder(None).unwrap();
            // Add a blob so the tree is not literally empty (some git versions
            // dislike empty trees).
            let blob = repo.blob(b"a").unwrap();
            tb.insert("a.txt", blob, 0o100_644).unwrap();
            tb.write().unwrap()
        };
        let tree = repo.find_tree(empty_tree_id).unwrap();

        // c1 on main.
        let c1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "c1", &tree, &[])
            .unwrap();
        let c1_obj = repo.find_commit(c1).unwrap();

        // dev branch with a second commit.
        let _ = repo.branch("dev", &c1_obj, false).unwrap();
        let tree2_id = {
            let mut tb = repo.treebuilder(None).unwrap();
            let blob = repo.blob(b"b").unwrap();
            tb.insert("b.txt", blob, 0o100_644).unwrap();
            tb.write().unwrap()
        };
        let tree2 = repo.find_tree(tree2_id).unwrap();
        let _c2 = repo
            .commit(Some("refs/heads/dev"), &sig, &sig, "c2", &tree2, &[&c1_obj])
            .unwrap();

        // feature branch from c1.
        let _ = repo.branch("feature", &c1_obj, false).unwrap();

        // Set HEAD to main as the default.
        repo.set_head("refs/heads/main").unwrap();

        let source = Source::open("test", bare).unwrap();
        (source, td)
    }

    #[test]
    fn pick_warm_targets_disabled_returns_empty() {
        let (src, _td) = make_test_source();
        let policy = WarmPolicy {
            enabled: false,
            policy: WarmPolicyKind::AllBranches,
            pinned: vec![],
            max_concurrent: 1,
        };
        assert!(pick_warm_targets(&src, &policy).unwrap().is_empty());
    }

    #[test]
    fn pick_warm_targets_off_returns_empty_even_when_enabled() {
        let (src, _td) = make_test_source();
        let policy = WarmPolicy {
            enabled: true,
            policy: WarmPolicyKind::Off,
            pinned: vec![],
            max_concurrent: 1,
        };
        assert!(pick_warm_targets(&src, &policy).unwrap().is_empty());
    }

    #[test]
    fn pick_warm_targets_default_branch() {
        let (src, _td) = make_test_source();
        let policy = WarmPolicy {
            enabled: true,
            policy: WarmPolicyKind::DefaultBranch,
            pinned: vec![],
            max_concurrent: 1,
        };
        let targets = pick_warm_targets(&src, &policy).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].branch, src.default_branch().unwrap());
    }

    #[test]
    fn pick_warm_targets_all_branches() {
        let (src, _td) = make_test_source();
        let policy = WarmPolicy {
            enabled: true,
            policy: WarmPolicyKind::AllBranches,
            pinned: vec![],
            max_concurrent: 1,
        };
        let targets = pick_warm_targets(&src, &policy).unwrap();
        let names: HashMap<&str, &str> = targets
            .iter()
            .map(|t| (t.branch.as_str(), t.commit_sha.as_str()))
            .collect();
        assert!(names.contains_key("main"));
        assert!(names.contains_key("dev"));
        assert!(names.contains_key("feature"));
    }

    #[test]
    fn pick_warm_targets_pinned_filters_to_listed_refs() {
        let (src, _td) = make_test_source();
        let policy = WarmPolicy {
            enabled: true,
            policy: WarmPolicyKind::Pinned,
            pinned: vec!["dev".to_string(), "no-such-branch".to_string()],
            max_concurrent: 1,
        };
        let targets = pick_warm_targets(&src, &policy).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].branch, "dev");
    }
}
