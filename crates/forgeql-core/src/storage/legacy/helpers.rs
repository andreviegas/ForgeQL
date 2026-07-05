//! Shared glob-filter helpers used by [`super::LegacyMemoryStorage`] resolvers
//! and by the prefilter query pipeline.
//!
//! Lifted from `engine.rs` — no algorithmic changes.

use std::path::Path;

use crate::ir::Clauses;

/// Return `false` if `path` is excluded by the `IN` or `EXCLUDE` glob of
/// `clauses`.
pub(super) fn passes_glob_filter(path: &Path, clauses: &Clauses, root: &Path) -> bool {
    if let Some(ref glob) = clauses.in_glob
        && !crate::ast::query::relative_glob_matches(path, glob, root)
    {
        return false;
    }
    if clauses
        .exclude_globs
        .iter()
        .any(|glob| crate::ast::query::relative_glob_matches(path, glob, root))
    {
        return false;
    }
    true
}
