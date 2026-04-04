/// Unused-parameter enrichment.
///
/// The actual detection is performed by `DeclDistanceEnricher`, which shares
/// the identifier-walk it already performs for dead-store and decl-distance
/// analysis.  This enricher is a deliberate no-op but is kept in the enricher
/// pipeline for backward-compatibility.
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};

/// Enricher for unused parameter detection (delegated to `DeclDistanceEnricher`).
pub struct UnusedParamEnricher;

impl NodeEnricher for UnusedParamEnricher {
    fn name(&self) -> &'static str {
        "unused_param"
    }

    fn enrich_row(
        &self,
        _ctx: &EnrichContext<'_>,
        _name: &str,
        _fields: &mut HashMap<String, String>,
    ) {
        // Unused-param detection is now performed by DeclDistanceEnricher,
        // which reuses the identifier-walk it already does for dead-store and
        // decl-distance analysis.  No work needed here.
    }
}
