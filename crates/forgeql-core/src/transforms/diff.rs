//! Unified diff generation for [`TransformPlan`]s.
//!
//! Applies a [`FileEdit`] to an in-memory buffer (no disk writes) and
//! generates a standard unified diff (`--- a/…`, `+++ b/…`, `@@ … @@`
//! hunks) suitable for human review.
//!
//! The diff algorithm is a line-level LCS (O(m·n)) which is acceptable
//! for typical source files.  Very large files (product m·n > 4 000 000)
//! fall back to a simple "replace everything" representation — still correct,
//! just without common-context compression.
//!
//! ## Compact diff preview
//!
//! [`compact_diff_plan`] produces a token-bounded summary of each file's
//! changes.  Parameters live in [`CompactDiffConfig`] and can be overridden
//! at the call site or — in the future — via CLI flags / config file.

mod apply;
mod compact;
mod lcs;

pub use apply::{diff_file_edit, diff_plan};
pub use compact::{CompactDiffConfig, compact_diff_addressed, compact_diff_plan};

#[cfg(test)]
mod tests;
