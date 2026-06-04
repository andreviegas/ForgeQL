//! Shared request context for all `SHOW` symbol operations.
//!
//! [`ShowRequest`] bundles the six parameters that every `show_body`,
//! `show_callees`, `show_signature`, and `show_members` call receives
//! identically, eliminating the need to pass them as individual arguments.
use std::path::Path;

use crate::{
    ast::{lang::LanguageRegistry, parse_cache::CachedParse},
    workspace::Workspace,
};

/// Common context for a `SHOW body / callees / signature / members` operation.
///
/// Built once in `exec_show` after `get_or_parse_for_show` succeeds and passed
/// by shared reference to every `show_*` function.  Fields that are not
/// relevant to a particular operation (e.g. `byte_range_start` for
/// `show_members`) are stored but not accessed.
pub struct ShowRequest<'a> {
    /// Cached tree-sitter parse of the symbol's source file.
    pub cached: &'a CachedParse,
    /// Absolute path to the source file.
    pub path: &'a Path,
    /// Byte offset of the symbol's definition in the source file.
    pub byte_range_start: usize,
    /// 1-based line number from the index; used to validate AST nodes and
    /// recover from tree-sitter brace-imbalance misparses.
    pub hint_line: Option<usize>,
    /// Workspace used for path relativisation and file I/O.
    pub workspace: &'a Workspace,
    /// Symbol name as supplied by the user query.
    pub symbol: &'a str,
    /// Language registry used to look up grammar-specific configuration.
    pub lang_registry: &'a LanguageRegistry,
    /// Ordinal from the index row; used to emit `node_id` on the function
    /// start line without touching the enrichment map. None for legacy rows.
    pub ordinal: Option<u32>,
}
