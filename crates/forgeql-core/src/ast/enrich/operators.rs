/// Operator enrichment — indexes increment/decrement, compound assignment,
/// and shift expressions.
///
/// Creates new [`IndexRow`]s for:
/// - `update_expression`: `increment_style`, `increment_op`, `operator_category`
/// - `assignment_expression` (compound): `compound_op`, `operand`, `operator_category`
/// - `shift_expression` / binary with `<<`/`>>`: `shift_direction`, `shift_amount`, `operator_category`
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, node_text};

/// Enricher for operator analysis (increment, compound assignment, shift).
pub struct OperatorEnricher;

impl NodeEnricher for OperatorEnricher {
    fn name(&self) -> &'static str {
        "operators"
    }

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let kind = ctx.node.kind();
        let config = ctx.language_config;

        if config.update_raw_kinds.contains(&kind) && config.has_increment_decrement {
            return Self::handle_update(ctx);
        }
        if config.assignment_raw_kinds.contains(&kind) {
            return Self::handle_compound_assignment(ctx);
        }
        if (!config.binary_expression_raw_kind.is_empty()
            && kind == config.binary_expression_raw_kind)
            || config.shift_expression_raw_kinds.contains(&kind)
        {
            return Self::handle_shift(ctx);
        }
        vec![]
    }
}

impl OperatorEnricher {
    fn handle_update(ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let text = node_text(ctx.source, ctx.node);
        if text.is_empty() {
            return vec![];
        }

        let mut fields = HashMap::new();

        let is_prefix = text.starts_with("++") || text.starts_with("--");
        let style = if is_prefix { "prefix" } else { "postfix" };
        drop(fields.insert("increment_style".to_string(), style.to_string()));

        let op = if text.contains("++") { "++" } else { "--" };
        drop(fields.insert("increment_op".to_string(), op.to_string()));
        drop(fields.insert("operator_category".to_string(), "increment".to_string()));

        // Extract the operand name
        let name = if is_prefix {
            text.trim_start_matches("++").trim_start_matches("--")
        } else {
            text.trim_end_matches("++").trim_end_matches("--")
        }
        .to_string();

        vec![IndexRow {
            name,
            node_kind: ctx.node.kind().to_string(),
            fql_kind: ctx
                .language_support
                .map_kind(ctx.node.kind())
                .unwrap_or("")
                .to_string(),
            language: ctx.language_name.to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
        }]
    }

    fn handle_compound_assignment(ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let Some(op_node) = ctx.node.child_by_field_name("operator") else {
            return vec![];
        };
        let op = node_text(ctx.source, op_node);

        // Only compound assignments, not plain `=`
        if !matches!(
            op.as_str(),
            "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>="
        ) {
            return vec![];
        }

        let mut fields = HashMap::new();
        drop(fields.insert("compound_op".to_string(), op.clone()));

        let category = match op.as_str() {
            "+=" | "-=" | "*=" | "/=" | "%=" => "arithmetic",
            "&=" | "|=" | "^=" => "bitwise",
            "<<=" | ">>=" => "shift",
            _ => "unknown",
        };
        drop(fields.insert("operator_category".to_string(), category.to_string()));

        let name = ctx
            .node
            .child_by_field_name("left")
            .map(|n| node_text(ctx.source, n))
            .unwrap_or_default();

        if let Some(right) = ctx.node.child_by_field_name("right") {
            drop(fields.insert("operand".to_string(), node_text(ctx.source, right)));
        }

        vec![IndexRow {
            name,
            node_kind: ctx.language_config.compound_assignment_raw_kind.to_string(),
            fql_kind: ctx
                .language_support
                .map_kind(ctx.language_config.compound_assignment_raw_kind)
                .unwrap_or("")
                .to_string(),
            language: ctx.language_name.to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
        }]
    }

    fn handle_shift(ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let Some(op_node) = ctx.node.child_by_field_name("operator") else {
            return vec![];
        };
        let op = node_text(ctx.source, op_node);

        if !matches!(op.as_str(), "<<" | ">>") {
            return vec![];
        }

        let mut fields = HashMap::new();
        let direction = if op == "<<" { "left" } else { "right" };
        drop(fields.insert("shift_direction".to_string(), direction.to_string()));
        drop(fields.insert("operator_category".to_string(), "bitwise".to_string()));

        if let Some(left) = ctx.node.child_by_field_name("left") {
            drop(fields.insert("shift_operand".to_string(), node_text(ctx.source, left)));
        }

        if let Some(right) = ctx.node.child_by_field_name("right") {
            drop(fields.insert("shift_amount".to_string(), node_text(ctx.source, right)));
        }

        let full_text = node_text(ctx.source, ctx.node);
        let name = if full_text.len() > 80 {
            format!("{}...", &full_text[..77])
        } else {
            full_text
        };

        // Use the canonical shift kind from config for the output row,
        // since the trigger node may be a binary_expression in some grammars.
        let node_kind = ctx.node.kind();
        let output_kind = ctx
            .language_config
            .shift_expression_raw_kinds
            .first()
            .copied()
            .unwrap_or(node_kind);

        vec![IndexRow {
            name,
            node_kind: output_kind.to_string(),
            fql_kind: ctx
                .language_support
                .map_kind(output_kind)
                .unwrap_or("")
                .to_string(),
            language: ctx.language_name.to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
        }]
    }
}
