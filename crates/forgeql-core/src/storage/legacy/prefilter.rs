//! Pre-filter and field-map helpers for [`super::LegacyMemoryStorage`].
//!
//! Lifted from `engine.rs` — no algorithmic changes.
//!
//! Public entry points used by [`super::LegacyMemoryStorage`]:
//! - [`find_symbols_prefilter`] — fast-path index shortcuts before full scan
//! - [`validate_order_by_field`] — validate ORDER BY against known fields

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Result, bail};

use crate::{
    ast::{index::SymbolTable, lang::LanguageConfig},
    filter::{eval_predicate, like_match},
    ir::{Clauses, CompareOp},
    result::SymbolMatch,
};

use super::helpers::passes_glob_filter;

// -----------------------------------------------------------------------
// Private helpers — trigram / predicate extraction
// -----------------------------------------------------------------------

/// If `pat` is of the form `^literal$` where `literal` contains no regex
/// metacharacters (`.*+?[](){}|\`), return the literal substring.
///
/// This lets `MATCHES '^exact_name$'` be routed to the O(1) `name_index`
/// instead of invoking the regex engine per row.
fn extract_anchored_literal(pat: &str) -> Option<&str> {
    let inner = pat.strip_prefix('^')?.strip_suffix('$')?;
    // Reject anything with regex metacharacters — must be a pure literal.
    if inner.chars().any(|c| ".*+?[](){}|\\".contains(c)) {
        return None;
    }
    // Reject case-insensitive flag or other inline flags.
    if inner.starts_with("(?") {
        return None;
    }
    Some(inner)
}

/// Extract a required literal substring (>= 3 bytes) from a MATCHES regex
/// pattern for use as a trigram pre-filter.
fn regex_trigram_literal(pat: &str) -> Option<String> {
    crate::ast::trigram::extract_regex_literal(pat)
}

/// Extract a required literal substring (>= 3 bytes) from a SQL LIKE pattern
/// for use as a trigram pre-filter.
fn like_trigram_literal(pat: &str) -> Option<String> {
    crate::ast::trigram::extract_like_literal(pat)
}

/// Return the `&str` value of the first predicate matching `field` and `op`,
/// or `None` if no such predicate exists or its value is not a string.
///
/// Eliminates the repeated `iter().find_map(|p| { if p.field == X && p.op == Y … })`
/// pattern inside `find_symbols_prefilter`.
fn find_pred_string<'a>(
    preds: &'a [crate::ir::Predicate],
    field: &str,
    op: CompareOp,
) -> Option<&'a str> {
    preds.iter().find_map(|p| {
        if p.field == field && p.op == op {
            if let crate::ir::PredicateValue::String(ref s) = p.value {
                Some(s.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

// -----------------------------------------------------------------------
// Private helpers — enrichment field → node_kind inference
// -----------------------------------------------------------------------

/// Complex cast-kind getter — needed because named-cast keywords may also
/// appear under `call_expression` nodes.
fn cast_kinds(c: &LanguageConfig) -> Vec<String> {
    let mut kinds: Vec<String> = c
        .cast_kind_triples()
        .iter()
        .map(|(raw_kind, _, _)| raw_kind.clone())
        .collect();
    // Named casts (e.g. static_cast<T>()) are indexed under the
    // call_expression node kind — include it when configured.
    if !c.named_cast_keywords.is_empty() && !c.call_expression_kind().is_empty() {
        let ce = c.call_expression_kind().to_owned();
        if !kinds.contains(&ce) {
            kinds.push(ce);
        }
    }
    kinds
}

/// Qualifier-flag / is_exported getter — combines declaration_kinds + function_kinds.
fn qualifier_kinds(c: &LanguageConfig) -> Vec<String> {
    let mut kinds = c.declaration_kinds().to_vec();
    kinds.extend_from_slice(c.function_kinds());
    kinds
}

type FieldKindFn = fn(&LanguageConfig) -> Vec<String>;
type FieldKindMap = HashMap<&'static str, FieldKindFn>;

static FIELD_KIND_MAP: OnceLock<FieldKindMap> = OnceLock::new();

#[allow(clippy::too_many_lines)]
fn get_field_kind_map() -> &'static FieldKindMap {
    FIELD_KIND_MAP.get_or_init(|| {
        let mut m: FieldKindMap = HashMap::new();
        // function_definition only — metrics, redundancy, escape, shadow,
        // unused_param, fallthrough, recursion, todo, decl_distance
        for &field in &[
            "param_count",
            "return_count",
            "goto_count",
            "string_count",
            "throw_count",
            "is_inline",
            "branch_count",
            "max_condition_tests",
            "max_paren_depth",
            "has_repeated_condition_calls",
            "repeated_condition_calls",
            "null_check_count",
            "has_escape",
            "escape_tier",
            "escape_vars",
            "escape_count",
            "escape_kinds",
            "has_shadow",
            "shadow_count",
            "shadow_vars",
            "has_unused_param",
            "unused_param_count",
            "unused_params",
            "has_fallthrough",
            "fallthrough_count",
            "is_recursive",
            "recursion_count",
            "has_todo",
            "todo_count",
            "todo_tags",
            "decl_distance",
            "decl_far_count",
            "has_unused_reassign",
        ] {
            let _ = m.insert(field, |c: &LanguageConfig| c.function_kinds().to_vec());
        }
        // comments.rs
        let _ = m.insert("comment_style", |c: &LanguageConfig| {
            vec![c.comment_kind().to_owned()]
        });
        // numbers.rs
        for &field in &[
            "num_format",
            "is_magic",
            "num_suffix",
            "has_separator",
            "num_value",
            "num_sign",
        ] {
            let _ = m.insert(field, |c: &LanguageConfig| {
                c.number_literal_kinds().to_vec()
            });
        }
        // operators.rs
        for &field in &["increment_style", "increment_op"] {
            let _ = m.insert(field, |c: &LanguageConfig| c.update_kinds().to_vec());
        }
        for &field in &["compound_op", "operand"] {
            let _ = m.insert(field, |c: &LanguageConfig| {
                vec![c.compound_assignment_kind().to_owned()]
            });
        }
        for &field in &["shift_direction", "shift_operand", "shift_amount"] {
            let _ = m.insert(field, |c: &LanguageConfig| {
                c.shift_expression_kinds().to_vec()
            });
        }
        // casts.rs — per-cast-node fields
        for &field in &["cast_style", "cast_target_type", "cast_safety"] {
            let _ = m.insert(field, cast_kinds);
        }
        // casts.rs — per-function fields
        for &field in &["has_cast", "cast_count"] {
            let _ = m.insert(field, |c: &LanguageConfig| c.function_raw_kinds.clone());
        }
        // control_flow.rs
        for &field in &[
            "condition_tests",
            "paren_depth",
            "condition_text",
            "has_assignment_in_condition",
            "mixed_logic",
            "dup_logic",
            "for_style",
            "enclosing_fn",
            "duplicate_condition",
        ] {
            let _ = m.insert(field, |c: &LanguageConfig| c.control_flow_kinds().to_vec());
        }
        let _ = m.insert("has_catch_all", |c: &LanguageConfig| {
            c.switch_kinds().to_vec()
        });
        // metrics.rs — multiple definition kinds
        for &field in &["lines", "member_count", "has_doc"] {
            let _ = m.insert(field, |c: &LanguageConfig| c.definition_kinds().to_vec());
        }
        // metrics.rs — qualifier flags / scope.rs — is_exported
        for &field in &["is_const", "is_volatile", "is_static", "is_exported"] {
            let _ = m.insert(field, qualifier_kinds);
        }
        // metrics.rs — visibility
        let _ = m.insert("visibility", |c: &LanguageConfig| c.field_kinds().to_vec());
        // scope.rs — declaration only
        for &field in &["scope", "storage"] {
            let _ = m.insert(field, |c: &LanguageConfig| c.declaration_kinds().to_vec());
        }
        m
    })
}

/// Map an enrichment field name to the `node_kind`(s) that carry it.
///
/// Returns `None` for universal fields (`naming`, `name_length`) or
/// built-in fields (`name`, `node_kind`, `path`, `line`, `usages`).
fn field_to_kinds_for_config(config: &LanguageConfig, field: &str) -> Option<Vec<String>> {
    get_field_kind_map().get(field).map(|f| f(config))
}

/// Aggregate `field_to_kinds_for_config` across all registered language configs.
fn field_to_kinds(configs: &[&LanguageConfig], field: &str) -> Option<Vec<String>> {
    let mut all_kinds: Vec<String> = Vec::new();
    for config in configs {
        if let Some(kinds) = field_to_kinds_for_config(config, field) {
            for k in kinds {
                if !all_kinds.contains(&k) {
                    all_kinds.push(k);
                }
            }
        }
    }
    if all_kinds.is_empty() {
        None
    } else {
        Some(all_kinds)
    }
}

/// Whether `field` is a known enrichment field name for ANY language —
/// membership in the static field→kind map. Used by the engine to tell a
/// misspelled WHERE field (matches nothing, worth a hint) apart from a
/// valid enrichment field that simply has no matching rows.
pub(super) fn is_known_enrichment_field(field: &str) -> bool {
    get_field_kind_map().contains_key(field)
}

/// Inspect WHERE predicates for enrichment fields and, when all resolvable
/// fields agree on the same set of kinds, return that set.
///
/// Returns `None` when no enrichment fields are found, or when the
/// intersection of inferred kinds is empty (contradictory predicates).
fn infer_kinds_from_fields(
    predicates: &[crate::ir::Predicate],
    configs: &[&LanguageConfig],
) -> Option<Vec<String>> {
    let mut result: Option<Vec<String>> = None;
    for pred in predicates {
        let Some(kinds) = field_to_kinds(configs, &pred.field) else {
            continue;
        };
        result = Some(match result {
            None => kinds,
            Some(current) => {
                let intersected: Vec<String> =
                    current.into_iter().filter(|k| kinds.contains(k)).collect();
                if intersected.is_empty() {
                    // Contradictory (e.g. cast_style + comment_style) — bail.
                    return None;
                }
                intersected
            }
        });
    }
    result
}

// -----------------------------------------------------------------------
// Public entry points
// -----------------------------------------------------------------------

/// Pre-filter symbol rows using secondary indexes and WHERE predicates
/// before materializing `SymbolMatch`.  Returns `(results, remaining_clauses)`
/// where `remaining_clauses` contains only the parts not yet applied.
#[allow(clippy::too_many_lines)]
pub(super) fn find_symbols_prefilter(
    index: &SymbolTable,
    clauses: &Clauses,
    root: &Path,
    lang_configs: &[&LanguageConfig],
) -> (Vec<SymbolMatch>, Clauses) {
    use crate::ast::index::RowRef;

    // Extract a `fql_kind = value` predicate for the fql_kind_index shortcut (preferred).
    // Extract a `node_kind = value` predicate for the kind_index shortcut (power-user fallback).
    let fql_kind_exact: Option<&str> =
        find_pred_string(&clauses.where_predicates, "fql_kind", CompareOp::Eq);
    let kind_exact: Option<&str> =
        find_pred_string(&clauses.where_predicates, "node_kind", CompareOp::Eq);

    // Extract a `name LIKE 'pattern'` predicate for name filtering.
    let name_like: Option<&str> =
        find_pred_string(&clauses.where_predicates, "name", CompareOp::Like);

    // Fast path 1: `name = 'literal'` — exact equality lookup in the name_index.
    // Fast path 2: `name MATCHES '^literal$'` with no regex metacharacters is
    // equivalent to an exact equality lookup in the name_index.
    // Both skip the per-row predicate engine entirely.
    let name_eq: Option<&str> = find_pred_string(&clauses.where_predicates, "name", CompareOp::Eq);
    let name_literal: Option<&str> = name_eq.or_else(|| {
        find_pred_string(&clauses.where_predicates, "name", CompareOp::Matches)
            .and_then(extract_anchored_literal)
    });

    let is_usages_pred = |p: &crate::ir::Predicate| p.field == "usages";

    // Trigram pre-filter: extract a required literal substring from MATCHES
    // or LIKE predicates that are NOT already handled by the exact name_index
    // path.  The trigram index returns a small candidate superset; the full
    // predicate is still evaluated per-candidate in `non_usages_preds`.
    let trigram_literal: Option<String> = if name_literal.is_none() {
        // Prefer the MATCHES pattern literal (usually more selective than LIKE).
        clauses
            .where_predicates
            .iter()
            .find_map(|p| {
                if p.field == "name"
                    && p.op == CompareOp::Matches
                    && let crate::ir::PredicateValue::String(ref s) = p.value
                {
                    return regex_trigram_literal(s.as_str());
                }
                None
            })
            .or_else(|| {
                // Fall back to LIKE literal when no MATCHES pattern exists.
                name_like.and_then(like_trigram_literal)
            })
    } else {
        None
    };

    // When no explicit kind predicate, infer raw kind(s) from enrichment fields.
    // This lets us use the kind_index instead of a full scan.
    let inferred_kinds: Option<Vec<String>> = if fql_kind_exact.is_none() && kind_exact.is_none() {
        infer_kinds_from_fields(&clauses.where_predicates, lang_configs)
    } else {
        None
    };

    // Row source priority:
    //   1. name_index  — exact anchored literal (O(1), 100% correct)
    //   2. trigram     — required substring (O(candidates), superset)
    //   3. fql_kind_index
    //   4. kind_index
    //   5. inferred kinds
    //   6. full scan
    let use_name_index = name_literal.is_some();
    // trigram_literal is only computed when name_literal is None, so
    // use_trigram already implies !use_name_index.
    let use_trigram = trigram_literal.is_some();
    // Strip a predicate only when its corresponding index actually supplied
    // the candidate rows.  Before trigram was introduced, the priority was
    // name_index → fql_kind_index → kind_index, and the strip logic only
    // checked !use_name_index.  Now that trigram sits between name_index and
    // fql_kind_index, the strip logic must also account for whether trigram
    // was used — otherwise `fql_kind` and `node_kind` predicates are silently
    // dropped even though fql_kind_index / kind_index was never consulted.
    let use_fql_kind_index = !use_name_index && !use_trigram && fql_kind_exact.is_some();
    let use_kind_index =
        !use_name_index && !use_trigram && fql_kind_exact.is_none() && kind_exact.is_some();

    let candidates: Box<dyn Iterator<Item = &crate::ast::index::IndexRow>> =
        if let Some(literal) = name_literal {
            Box::new(index.rows_by_name(literal))
        } else if let Some(ref substr) = trigram_literal {
            // trigram_candidates returns None only when substr < 3 bytes,
            // which can't happen here (extract_*_literal guarantees >= 3).
            let rows = index.trigram_candidates(substr).unwrap_or_default();
            Box::new(rows.into_iter())
        } else if let Some(fql_kind) = fql_kind_exact {
            Box::new(index.rows_by_fql_kind(fql_kind))
        } else if let Some(kind) = kind_exact {
            Box::new(index.rows_by_kind(kind))
        } else if let Some(ref kinds) = inferred_kinds {
            Box::new(
                kinds
                    .iter()
                    .flat_map(|k| index.rows_by_kind(k))
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        } else {
            Box::new(index.rows.iter())
        };

    // Collect non-usages predicates not already handled by index lookups.
    // A predicate is stripped only when its index WAS the actual candidate
    // source — stripping it otherwise would silently skip correct filtering.
    let non_usages_preds: Vec<_> = clauses
        .where_predicates
        .iter()
        .filter(|p| !is_usages_pred(p))
        // Strip fql_kind = X only when fql_kind_index supplied the candidates.
        .filter(|p| !(use_fql_kind_index && p.field == "fql_kind" && p.op == CompareOp::Eq))
        // Strip node_kind = X only when kind_index supplied the candidates.
        .filter(|p| !(use_kind_index && p.field == "node_kind" && p.op == CompareOp::Eq))
        // Strip an anchored MATCHES or exact = predicate that was resolved via name_index.
        .filter(|p| {
            !(use_name_index
                && p.field == "name"
                && (p.op == CompareOp::Matches || p.op == CompareOp::Eq))
        })
        .collect();

    // Filter on raw IndexRow — no heap allocation per rejected row.
    let filtered = candidates.filter(|row| {
        if let Some(pat) = name_like
            && !like_match(index.name_of(row), pat)
        {
            return false;
        }
        if !passes_glob_filter(index.path_of(row), clauses, root) {
            return false;
        }
        non_usages_preds
            .iter()
            .all(|p| eval_predicate(&RowRef { row, table: index }, p))
    });

    // Materialize SymbolMatch only for survivors, dedup inline.
    // When no ORDER BY / GROUP BY / usages-WHERE remains we can stop as soon
    // as we hit the LIMIT — no point scanning the remaining millions of rows.
    let has_usages_pred = clauses.where_predicates.iter().any(is_usages_pred);
    let can_early_exit = !has_usages_pred
        && clauses.order_by.is_none()
        && clauses.group_by.is_none()
        && clauses.offset.is_none();
    let early_limit = if can_early_exit {
        clauses.limit.unwrap_or(usize::MAX)
    } else {
        usize::MAX
    };

    let mut seen = HashSet::new();
    let mut results: Vec<SymbolMatch> = Vec::new();
    for def in filtered {
        if results.len() >= early_limit {
            break;
        }
        let key = (def.name_id, def.path_id, def.node_kind_id, def.line);
        if !seen.insert(key) {
            continue;
        }
        // usages_count is precomputed at index-build time; no HashMap lookup needed.
        let usages = def.usages_count as usize;
        let fql = index.fql_kind_of(def);
        let lang = index.language_of(def);
        results.push(SymbolMatch {
            name: index.name_of(def).to_owned(),
            node_kind: Some(index.node_kind_of(def).to_owned()),
            fql_kind: if fql.is_empty() {
                None
            } else {
                Some(fql.to_owned())
            },
            language: if lang.is_empty() {
                None
            } else {
                Some(lang.to_owned())
            },
            path: Some(index.path_of(def).to_owned()),
            line: Some(def.line),
            usages_count: Some(usages),
            fields: index.strings.resolve_fields(&def.fields),
            count: None,
            node_id: None,
        });
    }

    // Only usages-based WHERE, GROUP/HAVING, ORDER, OFFSET, LIMIT remain.
    let remaining = Clauses {
        where_predicates: clauses
            .where_predicates
            .iter()
            .filter(|p| is_usages_pred(p))
            .cloned()
            .collect(),
        having_predicates: clauses.having_predicates.clone(),
        order_by: clauses.order_by.clone(),
        group_by: clauses.group_by.clone(),
        limit: clauses.limit,
        offset: clauses.offset,
        in_glob: None,
        exclude_globs: Vec::new(),
        depth: None,
    };

    (results, remaining)
}

/// Validate that the ORDER BY field is either a known built-in field, a known
/// enrichment field, or present in at least one result item.
///
/// When the ORDER BY field is a recognised enrichment field (from any
/// registered language config), we accept it unconditionally — items that
/// lack the field will sort to the end (`field_num` returns None, which the
/// sort comparator already handles).  This allows queries like
/// `FIND symbols WHERE has_assignment_in_condition = 'true' ORDER BY lines DESC`
/// to work even when the result set contains symbol types (e.g. `if`) that
/// don't carry the `lines` enrichment field themselves.
pub(super) fn validate_order_by_field(
    clauses: &Clauses,
    results: &[SymbolMatch],
    lang_configs: &[&LanguageConfig],
) -> Result<()> {
    use crate::filter::ClauseTarget as _;

    const STATIC_FIELDS: &[&str] = &[
        "name",
        "fql_kind",
        "node_kind",
        "path",
        "file",
        "line",
        "usages",
        "count",
    ];

    let Some(ref order) = clauses.order_by else {
        return Ok(());
    };
    if STATIC_FIELDS.contains(&order.field.as_str()) {
        return Ok(());
    }

    // Accept any known enrichment field — items without it sort to the end.
    if field_to_kinds(lang_configs, &order.field).is_some() {
        return Ok(());
    }

    // For dynamic fields (e.g. "type", "value", "signature"):
    // accept if at least one result item carries the field.
    if results
        .iter()
        .any(|r| r.field_num(&order.field).is_some() || r.field_str(&order.field).is_some())
    {
        return Ok(());
    }

    // If the result set is empty we cannot tell whether the field is valid;
    // skip reporting an error to avoid spurious failures.
    if results.is_empty() {
        return Ok(());
    }

    bail!(
        "unknown ORDER BY field '{}'; built-in fields: {}",
        order.field,
        STATIC_FIELDS.join(", ")
    )
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Clauses, OrderBy, SortDirection};
    use std::collections::HashMap;

    fn make_sym(name: &str) -> SymbolMatch {
        SymbolMatch {
            name: name.to_string(),
            node_kind: Some("function_definition".to_string()),
            fql_kind: Some("function".to_string()),
            language: Some("cpp".to_string()),
            path: Some(std::path::PathBuf::from("src/a.cpp")),
            line: Some(10),
            usages_count: Some(3),
            fields: HashMap::new(),
            count: None,
            node_id: None,
        }
    }

    fn clauses_with_order(field: &str) -> Clauses {
        Clauses {
            order_by: Some(OrderBy {
                field: field.to_string(),
                direction: SortDirection::Asc,
            }),
            ..Clauses::default()
        }
    }

    #[test]
    fn validate_order_by_field_accepts_static_fields() {
        let results = vec![make_sym("foo")];
        for field in &[
            "name",
            "fql_kind",
            "node_kind",
            "path",
            "file",
            "line",
            "usages",
            "count",
        ] {
            assert!(
                validate_order_by_field(&clauses_with_order(field), &results, &[]).is_ok(),
                "expected Ok for ORDER BY {field}"
            );
        }
    }

    #[test]
    fn validate_order_by_field_rejects_unknown_field() {
        let results = vec![make_sym("foo"), make_sym("bar")];
        let err = validate_order_by_field(&clauses_with_order("invalid_field"), &results, &[]);
        assert!(err.is_err(), "expected Err for ORDER BY invalid_field");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("invalid_field"),
            "error should mention the field name; got: {msg}"
        );
    }

    #[test]
    fn validate_order_by_field_accepts_dynamic_field_when_present() {
        let mut sym = make_sym("foo");
        sym.fields
            .insert("signature".to_string(), "void foo()".to_string());
        let results = vec![sym];
        assert!(validate_order_by_field(&clauses_with_order("signature"), &results, &[]).is_ok());
    }

    #[test]
    fn validate_order_by_field_ok_when_results_empty() {
        let results: Vec<SymbolMatch> = Vec::new();
        // Should not error even for unknown field when result set is empty.
        assert!(validate_order_by_field(&clauses_with_order("unknown_xyz"), &results, &[]).is_ok());
    }

    #[test]
    fn validate_order_by_field_no_order_by_always_ok() {
        let results = vec![make_sym("foo")];
        assert!(validate_order_by_field(&Clauses::default(), &results, &[]).is_ok());
    }
}
