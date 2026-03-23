/// Trait-based enrichment architecture for the symbol index.
///
/// Each enricher is a standalone struct that adds computed metadata fields
/// to indexed AST nodes.  Adding a new enrichment requires three steps:
///
/// 1. Create `new_thing.rs` implementing [`NodeEnricher`]
/// 2. Add `pub mod new_thing;` here
/// 3. Add `Box::new(NewThingEnricher)` to [`default_enrichers`]
///
/// Zero changes to `collect_nodes()`, `index_file()`, or `filter.rs`.
use std::collections::HashMap;
use std::path::Path;

use super::index::{IndexRow, SymbolTable};
use super::lang::{LanguageConfig, LanguageSupport};

pub mod casts;
pub mod comments;
pub mod control_flow;
pub mod data_flow_utils;
pub mod decl_distance;
pub mod escape;
pub mod fallthrough;
pub mod member;
pub mod metrics;
pub mod naming;
pub mod numbers;
pub mod operators;
pub mod recursion;
pub mod redundancy;
pub mod scope;
pub mod shadow;
pub mod unused_param;

// -----------------------------------------------------------------------
// EnrichContext — the read-only view of a node available to enrichers
// -----------------------------------------------------------------------

/// Read-only context passed to every enricher for each AST node.
pub struct EnrichContext<'a> {
    /// The tree-sitter node being processed.
    pub node: tree_sitter::Node<'a>,
    /// Full source text of the file (bytes).
    pub source: &'a [u8],
    /// Absolute path to the source file.
    pub path: &'a Path,
    /// Language identifier (e.g. `"cpp"`).
    pub language_name: &'a str,
    /// Language-specific configuration (node kinds, separators, etc.).
    pub language_config: &'a LanguageConfig,
    /// Full language support trait object (for `extract_name`, `map_kind`, etc.).
    pub language_support: &'a dyn LanguageSupport,
}

// -----------------------------------------------------------------------
// NodeEnricher trait
// -----------------------------------------------------------------------

/// A pluggable enrichment pass that adds computed fields to index rows.
pub trait NodeEnricher: Send + Sync {
    /// Human-readable name for debugging / logging.
    fn name(&self) -> &'static str;

    /// Add fields to a row that `extract_name()` already accepted.
    ///
    /// Called once per named node, after grammar fields are extracted.
    /// The default implementation is a no-op.
    #[allow(unused_variables)]
    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        name: &str,
        fields: &mut HashMap<String, String>,
    ) {
    }

    /// Produce NEW [`IndexRow`]s for node kinds that `extract_name()` skips.
    ///
    /// Called for *every* AST node during the walk (even unnamed ones).
    /// The default implementation returns an empty vec.
    #[allow(unused_variables)]
    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        vec![]
    }

    /// Post-process the symbol table after all files have been indexed.
    ///
    /// Used for aggregation passes (e.g. computing `max_condition_tests`
    /// on parent function rows from child control-flow rows).
    /// The default implementation is a no-op.
    #[allow(unused_variables)]
    fn post_pass(&self, table: &mut SymbolTable) {}
}

// -----------------------------------------------------------------------
// Default enricher set
// -----------------------------------------------------------------------

/// Build the standard set of enrichers shipped with `ForgeQL`.
#[must_use]
pub fn default_enrichers() -> Vec<Box<dyn NodeEnricher>> {
    vec![
        Box::new(scope::ScopeEnricher),
        Box::new(naming::NamingEnricher),
        Box::new(comments::CommentEnricher),
        Box::new(numbers::NumberEnricher),
        Box::new(control_flow::ControlFlowEnricher),
        Box::new(operators::OperatorEnricher),
        Box::new(metrics::MetricsEnricher),
        Box::new(casts::CastEnricher),
        Box::new(redundancy::RedundancyEnricher),
        Box::new(member::MemberEnricher),
        Box::new(decl_distance::DeclDistanceEnricher),
        Box::new(escape::EscapeEnricher),
        Box::new(shadow::ShadowEnricher),
        Box::new(unused_param::UnusedParamEnricher),
        Box::new(fallthrough::FallthroughEnricher),
        Box::new(recursion::RecursionEnricher),
    ]
}
