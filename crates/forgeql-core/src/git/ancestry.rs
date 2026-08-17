//! Which of a set of commits a given commit descends from, and how far back.
//!
//! An attach to a commit that has no overlay of its own can be served from
//! the overlay of a commit it grew from plus the files that changed since.
//! This picks that base: the closest ancestor — by commits walked back from
//! the target — among the commits that have an overlay on disk. Ancestry is
//! read off the commit graph, never inferred from age or from a shared
//! branch: a sibling branch's tip is not a base however recent it is, and
//! nothing here would ever name it one.

#![cfg_attr(test, allow(clippy::unwrap_used))]
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository, Sort};

/// The base an attach can chain from: an ancestor of the target that the
/// caller has an artefact for, and how far behind the target it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearestAncestor {
    /// Full commit id, hex.
    pub commit: String,
    /// Commits walked back from the target before this one was reached —
    /// `1` for the target's own parent. Time-ordered, so across a merge it
    /// is a count of the commits newer than the base, not a path length.
    pub distance: usize,
}

/// Walk back from `commit_hex` and return the first ancestor in
/// `candidates`, or `None` when none is reached within `max_walk` commits.
///
/// The walk yields ancestors of the target only, newest first — the order
/// `git log` uses — so the first candidate it meets is the closest one in
/// that order. `commit_hex` itself is never a result. A candidate that does
/// not parse as a commit id is ignored: it cannot be an ancestor of anything.
///
/// The cap bounds the cost of a walk that finds nothing, and history is the
/// one place that cost is unbounded: a corpus whose only overlays sit on
/// unrelated branches would otherwise be walked to its root on every attach.
/// A base beyond the cap is not wrong, merely far, and a base that far back
/// carries a change set the caller would refuse to seed anyway.
///
/// # Errors
/// Returns `Err` when the repository at `worktree` cannot be opened, or
/// `commit_hex` does not name a commit in it.
pub fn nearest_ancestor(
    worktree: &Path,
    commit_hex: &str,
    candidates: &[String],
    max_walk: usize,
) -> Result<Option<NearestAncestor>> {
    let target = Oid::from_str(commit_hex)
        .with_context(|| format!("chain base search: {commit_hex} is not a commit id"))?;
    let wanted: HashSet<Oid> = candidates
        .iter()
        .filter_map(|c| Oid::from_str(c).ok())
        .filter(|oid| *oid != target)
        .collect();
    if wanted.is_empty() {
        return Ok(None);
    }
    let repo = Repository::open(worktree).with_context(|| {
        format!(
            "chain base search: open repository at {}",
            worktree.display()
        )
    })?;
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::NONE)?;
    walk.push(target)
        .with_context(|| format!("chain base search: {commit_hex} is not in this repository"))?;
    let mut distance = 0usize;
    for oid in walk {
        let oid = oid?;
        if oid == target {
            continue;
        }
        distance += 1;
        if distance > max_walk {
            return Ok(None);
        }
        if wanted.contains(&oid) {
            return Ok(Some(NearestAncestor {
                commit: oid.to_string(),
                distance,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::make_normal_repo;
    use super::*;

    /// Commit a change to `file.cpp` on the current branch and return the
    /// new commit id.
    fn commit_change(repo: &Repository, dir: &Path, marker: &str) -> String {
        std::fs::write(dir.join("file.cpp"), format!("int {marker}(){{}}\n")).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.cpp")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        // Later commits get later times so the time-ordered walk is
        // deterministic; a fixed clock would leave the order to tie-breaks.
        let secs = parent.time().seconds() + 60;
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(secs, 0)).unwrap();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, marker, &tree, &[&parent])
            .unwrap();
        oid.to_string()
    }

    fn set_of(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_closest_ancestor_with_an_artefact_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_normal_repo(tmp.path());
        let a = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let b = commit_change(&repo, tmp.path(), "b");
        let c = commit_change(&repo, tmp.path(), "c");

        let found = nearest_ancestor(tmp.path(), &c, &set_of(&[&a, &b]), 100).unwrap();
        assert_eq!(
            found,
            Some(NearestAncestor {
                commit: b,
                distance: 1
            })
        );
        let found = nearest_ancestor(tmp.path(), &c, &set_of(&[&a]), 100).unwrap();
        assert_eq!(
            found,
            Some(NearestAncestor {
                commit: a,
                distance: 2
            })
        );
    }

    #[test]
    fn a_sibling_is_not_a_base_and_the_target_is_never_its_own() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_normal_repo(tmp.path());
        let a = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let b = commit_change(&repo, tmp.path(), "b");
        // A sibling branch off `a`, newer than `b`.
        let a_commit = repo.find_commit(Oid::from_str(&a).unwrap()).unwrap();
        repo.branch("side", &a_commit, false).unwrap();
        repo.set_head("refs/heads/side").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let s = commit_change(&repo, tmp.path(), "s");

        // Only the sibling has an artefact: nothing qualifies.
        assert_eq!(
            nearest_ancestor(tmp.path(), &b, &set_of(&[&s]), 100).unwrap(),
            None
        );
        // The target itself is a candidate on disk (a stale copy, say): it
        // is skipped, and the real ancestor is found behind it.
        assert_eq!(
            nearest_ancestor(tmp.path(), &b, &set_of(&[&s, &b, &a]), 100).unwrap(),
            Some(NearestAncestor {
                commit: a,
                distance: 1
            })
        );
    }

    #[test]
    fn the_walk_cap_bounds_the_search_and_junk_ids_are_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_normal_repo(tmp.path());
        let a = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let _b = commit_change(&repo, tmp.path(), "b");
        let c = commit_change(&repo, tmp.path(), "c");

        // `a` is two commits back; a cap of one never reaches it.
        assert_eq!(
            nearest_ancestor(tmp.path(), &c, &set_of(&[&a]), 1).unwrap(),
            None
        );
        // A candidate that is not a commit id (a test fixture's fake commit
        // name) is not an error and not a match.
        assert_eq!(
            nearest_ancestor(tmp.path(), &c, &set_of(&["aa11masteraa11"]), 100).unwrap(),
            None
        );
    }
}
