//! Locate a tree-sitter `ERROR` region and record how much of the file it ate.
//!
//! `error` rows exist to answer one question for an agent: **can I trust this
//! file's node spans enough to mutate it by handle?** `CHANGE NODE`,
//! `DELETE NODE` and `MOVE NODE` splice byte ranges, so the parse failures that
//! matter are the ones that changed the *shape* of the tree.
//!
//! An `ERROR` alone is a terrible proxy for that. tree-sitter parses C **without
//! running the preprocessor**, so it cannot know that an unknown identifier in
//! declaration-specifier position is a macro. `static ALWAYS_INLINE void f(void)`
//! therefore yields an `ERROR` beside the return type — while `f` itself indexes
//! perfectly as a `function` with correct boundaries. Zephyr has ~74 000 such
//! regions (top snippets: `struct`, `#endif`, `void`, `##_fake`, all
//! preprocessor) and essentially none of them is damage. A signal that fires on
//! idiomatic kernel C is not a signal.
//!
//! So this enricher reports **position and size, and passes no judgement** (P1):
//!
//! * `error_scope` — `root` (the ERROR *is* the file: nothing parsed, e.g. a
//!   `.c` that is not really C) / `file` (loose at top level, nothing named owns
//!   it — usually a file-scope macro the parser could not model) / `nested`
//!   (inside a node the language could name, so an indexed symbol still owns the
//!   span and its boundaries are intact).
//! * `error_bytes` — the region's byte length, from which `parse_coverage` is
//!   derived at query time.
//!
//! `parse_coverage` is the number that actually separates the two worlds: a
//! macro-heavy but perfectly healthy header stays near 1.0, while a file whose
//! extension lies parses to almost nothing and collapses toward 0.
//!
//! Ragged CSV rows and duplicate JSON keys are deliberately **not** errors — they
//! parse fine. They surface through block-group splitting instead.
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};

/// Enricher that computes `error_scope` and `error_bytes` on `ERROR` rows.
pub struct ErrorScopeEnricher;

impl NodeEnricher for ErrorScopeEnricher {
    fn name(&self) -> &'static str {
        "error_scope"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let node = ctx.node;
        if !node.is_error() {
            return;
        }

        drop(fields.insert(
            "error_bytes".to_string(),
            node.byte_range().len().to_string(),
        ));

        // Position only.  No thresholds, no judgement about whether the region
        // is "bad" — the engine reports where the parse broke and the agent
        // decides (P1).  `parse_coverage` carries the magnitude.
        //
        //   root   — the ERROR *is* the file. Nothing parsed. This is the `.c`
        //            that is not really C.
        //   file   — loose at top level: no named node owns it. Usually a
        //            file-scope macro the parser could not model
        //            (`module_param(x, int, 0);`), occasionally real damage.
        //   nested — inside a node the language could name, so an indexed symbol
        //            still owns the span and its own boundaries are intact.
        //            `static ALWAYS_INLINE void f(void)` lands here: `f` is a
        //            correct `function` row with an ERROR beside its return type.
        //
        // `extract_name` is the language's own naming rule, so core stays free of
        // language knowledge (P2) and this tracks whatever the plugin indexes.
        let scope = if node.parent().is_none() {
            "root"
        } else {
            let mut ancestor = node.parent();
            let mut owner = None;
            while let Some(parent) = ancestor {
                if ctx
                    .language_support
                    .extract_name(parent, ctx.source)
                    .is_some()
                {
                    owner = Some(parent);
                    break;
                }
                ancestor = parent.parent();
            }
            if owner.is_some() { "nested" } else { "file" }
        };
        drop(fields.insert("error_scope".to_string(), scope.to_string()));
    }
}
