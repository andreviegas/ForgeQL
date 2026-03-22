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
        match ctx.node.kind() {
            "update_expression" if ctx.language_config.has_increment_decrement => {
                Self::handle_update(ctx)
            }
            "assignment_expression" => Self::handle_compound_assignment(ctx),
            "binary_expression" | "shift_expression" => Self::handle_shift(ctx),
            _ => vec![],
        }
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
            node_kind: "update_expression".to_string(),
            fql_kind: ctx
                .language_support
                .map_kind("update_expression")
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
            node_kind: "compound_assignment".to_string(),
            fql_kind: ctx
                .language_support
                .map_kind("compound_assignment")
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

        vec![IndexRow {
            name,
            node_kind: "shift_expression".to_string(),
            fql_kind: ctx
                .language_support
                .map_kind("shift_expression")
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
