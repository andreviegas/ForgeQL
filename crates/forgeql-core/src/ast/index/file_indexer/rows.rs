//! Turning one tree-sitter node into the rows the index stores.
//!
//! `process_node_rows` is the centre of the indexing pass. For a single node it
//! decides whether the node is addressable, assigns its ordinal, gathers the
//! enrichment fields, and emits the addressable row plus whatever extra rows
//! the enrichers contributed.
//!
//! Nearly everything a query later sees — a row's name, kind, fields, `node_id`
//! and `rev` — is decided here. This is stored output: a change to any of it
//! must bump `ENRICH_VER`, or cached segments keep serving the old answer.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::ast::enrich::guard_utils::{
    GuardFrame, collect_attribute_guard_frames, inject_guard_fields,
};
use crate::ast::enrich::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, SymbolTable, node_text};
use crate::ast::lang::{FQL_ERROR, identifier_tokens_with};

use super::blocks::{BlockTag, attr_extended_start};
use super::hash::{first_body_statement_fingerprint, node_content_hash};
use super::ordinals::{OrdinalMatchKey, OrdinalRemapper, assign_ordinal};
use super::{IndexContext, is_addressable_fql_kind};
/// Maximum characters in a syntax-error row's name, so a long line of garbage
/// cannot produce an unbounded name.
const MAX_ERROR_SNIPPET: usize = 60;

/// A one-line, length-capped label for a syntax-error region.
///
/// Content-derived, never positional — the same contract as every other name in
/// the index. An error region's identity is inherently unstable (it changes when
/// the broken text changes, and disappears when the text is fixed), which is
/// exactly what we want: a stale handle to damage that no longer exists must not
/// resolve.
fn error_snippet(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return FQL_ERROR.to_string();
    }
    if first.chars().count() > MAX_ERROR_SNIPPET {
        let short: String = first.chars().take(MAX_ERROR_SNIPPET).collect();
        format!("{short}…")
    } else {
        first.to_string()
    }
}
/// Emit all symbol-table rows for a single (non-skipped) `node`: the syntax-error
/// row, the named row (or a re-tagged `macro_call` row), every enricher
/// `extra_rows`, and any usage site. Returns the named row's ordinal so the
/// caller can propagate it to descendant nodes. Does **not** descend into
/// children — the caller owns the cursor walk. Extracted from the
/// `collect_nodes` walk loop.
#[allow(clippy::too_many_arguments)]
pub(super) fn process_node_rows(
    ctx: &mut IndexContext<'_>,
    node: tree_sitter::Node<'_>,
    source: &[u8],
    ts_language: &tree_sitter::Language,
    guard_stack: &[GuardFrame],
    guard_file_key: u64,
    parent_kind: &'static str,
    parent_ordinal: u32,
    inside_string: bool,
    inside_error: bool,
    nearest_field: Option<&str>,
    row_ordinal_counter: &mut u32,
    block_tag: Option<&BlockTag>,
) -> Option<u32> {
    let config = ctx.language.config();
    let lang_name = ctx.language.name();
    let mut current_node_ordinal: Option<u32> = None;

    // Build the enrichment context once for this node.
    let enrich_ctx = EnrichContext {
        node,
        source,
        path: ctx.path,
        language_name: lang_name,
        language_config: config,
        language_support: ctx.language,
        guard_stack,
        guard_file_key,
        macro_table: ctx.macro_table,
        parent_kind,
        inside_string,
        inside_error,
    };

    // --- Syntax damage ---------------------------------------------------
    // A tree-sitter `ERROR` node is a region the parser could not parse. It is
    // universal to tree-sitter, so core needs no language knowledge to spot it.
    //
    // Until now these were tracked only to *suppress* phantom enrichment, and
    // emitted no rows: an agent had no way to learn that the file it was about
    // to mutate was already broken.
    //
    // Only the OUTERMOST damage is emitted (`!inside_error`) — a nested ERROR
    // would report one wound as several.
    //
    // `MISSING` nodes are deliberately NOT emitted. A missing token is
    // zero-width, so a row for it would span no bytes: an agent could see it but
    // could not `SHOW NODE` or `CHANGE NODE` it. A row you cannot act on is the
    // half-measure this whole change exists to avoid. In practice a missing
    // token almost always sits inside an ERROR region, which IS emitted.
    //
    // P1: this MAPS the damage and hands over a handle. The engine does not
    // validate, refuse, or repair; reading and fixing it is the agent's job.
    if !inside_error && node.is_error() {
        let name = error_snippet(&node_text(source, node));
        let mut sink = RowSink {
            table: ctx.table,
            enrichers: ctx.enrichers,
            remapper: ctx.ordinal_remapper.as_mut(),
            row_ordinal_counter,
        };
        current_node_ordinal = emit_addressable_row(
            &mut sink,
            &enrich_ctx,
            ts_language,
            &name,
            FQL_ERROR,
            parent_ordinal,
            block_tag,
        );
    }
    // Every named node becomes a row.
    else if let Some(name) = ctx.language.extract_name(node, source) {
        let fql_kind_val = ctx.language.map_kind(node.kind()).unwrap_or("");
        let mut sink = RowSink {
            table: ctx.table,
            enrichers: ctx.enrichers,
            remapper: ctx.ordinal_remapper.as_mut(),
            row_ordinal_counter,
        };
        current_node_ordinal = emit_addressable_row(
            &mut sink,
            &enrich_ctx,
            ts_language,
            &name,
            fql_kind_val,
            parent_ordinal,
            block_tag,
        );
    } else if let Some(mtable) = ctx.macro_table {
        // Re-tag: tree-sitter-cpp parses C macro calls as call_expression,
        // not macro_invocation.  When extract_name returns None for a
        // call_expression whose function name is in the MacroTable, emit a
        // macro_call row.
        let call_kind = config.call_expression_kind();
        if !call_kind.is_empty()
            && node.kind() == call_kind
            && let Some(func_node) = node.child_by_field_name("function")
        {
            let func_name = node_text(source, func_node);
            if !func_name.is_empty() && mtable.contains(&func_name) {
                let mut sink = RowSink {
                    table: ctx.table,
                    enrichers: ctx.enrichers,
                    remapper: ctx.ordinal_remapper.as_mut(),
                    row_ordinal_counter,
                };
                current_node_ordinal = emit_addressable_row(
                    &mut sink,
                    &enrich_ctx,
                    ts_language,
                    &func_name,
                    "macro_call",
                    parent_ordinal,
                    None,
                );
            }
        }
    }

    // Run extra_rows() for every node (even if extract_name returned None).
    let mut sink = RowSink {
        table: ctx.table,
        enrichers: ctx.enrichers,
        remapper: ctx.ordinal_remapper.as_mut(),
        row_ordinal_counter,
    };
    let extra_self = emit_extra_rows(&mut sink, &enrich_ctx, parent_ordinal);
    // §4.1: promote the nameless control-flow self-row so the body parents to it.
    if current_node_ordinal.is_none() && config.is_control_flow_kind(node.kind()) {
        current_node_ordinal = extra_self;
    }

    record_occurrences(ctx, node, source, nearest_field);
    current_node_ordinal
}

/// Record every occurrence of a name this node contributes.
///
/// Two kinds, deliberately kept together because they answer the same question
/// — where is this name written? An identifier node *is* the token, so it
/// records one site at its own position. A text-bearing node (a comment, and
/// later a string or a document) *contains* tokens, so its text is scanned and
/// each token records the line it sits on, which is not the line the node
/// opened on. A kind whose rule names a grammar field contributes only where
/// that field is the nearest labelled edge the walk crossed to reach it.
fn record_occurrences(
    ctx: &mut IndexContext<'_>,
    node: tree_sitter::Node<'_>,
    source: &[u8],
    nearest_field: Option<&str>,
) {
    let config = ctx.language.config();

    if config.is_usage_node_kind(node.kind()) {
        let name = node_text(source, node);
        if name.len() > 1 {
            let line = node.start_position().row + 1;
            ctx.table.add_usage(name, ctx.path, node.byte_range(), line);
        }
    }

    if let Some(rule) = config.mention_rule(node.kind())
        && rule
            .when_field
            .as_deref()
            .is_none_or(|want| nearest_field == Some(want))
    {
        let text = node_text(source, node);
        let start_line = node.start_position().row + 1;
        let start_byte = node.start_byte();
        let extra = config.mention_token_extra_chars();
        for (token, line, offset) in identifier_tokens_with(&text, start_line, extra) {
            let start = start_byte + offset;
            let end = start + token.len();
            ctx.table
                .add_mention(&rule.role, token, ctx.path, start..end, line);
        }
    }
}

/// Fields and identity metadata prepared for a single index row, shared by the
/// named-node and re-tagged macro-call emission paths.
struct PreparedRow {
    fields: HashMap<String, String>,
    content_hash: String,
    guard_group_id: Option<String>,
    guard_branch: Option<String>,
    first_body_statement_fingerprint: Option<String>,
}

/// Extract the raw fields of a node and inject its guard/attribute context,
/// returning the field map plus the identity metadata needed to compute a
/// stable ordinal.
///
/// Pure: derives the node, source, config, and guard stack from `ctx`, so the
/// named and macro-call paths prepare rows identically.
fn build_row_fields(ctx: &EnrichContext<'_>, ts_language: &tree_sitter::Language) -> PreparedRow {
    let node = ctx.node;
    let source = ctx.source;
    let mut fields = extract_fields(node, source, ts_language);

    // Inject guard fields from the current block-guard stack.
    if !ctx.guard_stack.is_empty() {
        inject_guard_fields(ctx.guard_stack, &mut fields);
    }

    // Inject item-level attribute guards (e.g. Rust `#[cfg(...)]`).
    let attr_guard_name = ctx.language_config.item_guard_attribute();
    if !attr_guard_name.is_empty() {
        let decorator_kind = ctx.language_config.decorator_kind().unwrap_or("");
        let attr_frames = collect_attribute_guard_frames(
            node,
            source,
            attr_guard_name,
            decorator_kind,
            ctx.guard_file_key,
        );
        if !attr_frames.is_empty() {
            inject_guard_fields(&attr_frames, &mut fields);
        }
    }

    let first_body_statement_fingerprint = first_body_statement_fingerprint(node, source);
    let content_hash = node_content_hash(node, source);
    let guard_group_id = fields.get("guard_group_id").cloned();
    let guard_branch = fields.get("guard_branch").cloned();
    if let Some(fp) = &first_body_statement_fingerprint {
        drop(fields.insert("first_body_statement_fingerprint".to_string(), fp.clone()));
    }
    drop(fields.insert("content_hash".to_string(), content_hash.clone()));

    PreparedRow {
        fields,
        content_hash,
        guard_group_id,
        guard_branch,
        first_body_statement_fingerprint,
    }
}

/// Content revision for an addressable row: the first 8 bytes of the SHA-256 of
/// the node source. Non-addressable rows (ordinal `None`) get `0`.
pub(super) fn row_rev(
    ordinal: Option<u32>,
    source: &[u8],
    byte_range: std::ops::Range<usize>,
) -> u64 {
    ordinal.map_or(0, |_| {
        let bytes = Sha256::digest(&source[byte_range]);
        u64::from_le_bytes(bytes[..8].try_into().unwrap_or([0u8; 8]))
    })
}

/// Borrowed sinks an addressable row is written into: the symbol table, the
/// active enrichers, the optional ordinal remapper, and the per-file ordinal
/// counter. Bundled so the row-emission helper stays under the argument limit.
struct RowSink<'a> {
    table: &'a mut SymbolTable,
    enrichers: &'a [Box<dyn NodeEnricher>],
    remapper: Option<&'a mut OrdinalRemapper>,
    row_ordinal_counter: &'a mut u32,
}

/// Emit one addressable index row for `node` under the given `name` and
/// `fql_kind`. Shared by the named-node path (`fql_kind` from `map_kind`) and
/// the re-tagged macro-call path (`fql_kind` = `"macro_call"`); the two differ
/// only in those two strings. Returns the assigned ordinal (or `None` when the
/// kind is not addressable) so the caller can propagate it to descendants.
fn emit_addressable_row(
    sink: &mut RowSink<'_>,
    ctx: &EnrichContext<'_>,
    ts_language: &tree_sitter::Language,
    name: &str,
    fql_kind: &str,
    parent_ordinal: u32,
    block_tag: Option<&BlockTag>,
) -> Option<u32> {
    let node = ctx.node;
    let source = ctx.source;
    let prepared = build_row_fields(ctx, ts_language);
    let mut fields = prepared.fields;

    // Run all enrichers on this row.
    for enricher in sink.enrichers {
        enricher.enrich_row(ctx, name, &mut fields);
    }

    // Stage 2: tag this row with its owning block address so FIND/SHOW can
    // surface the member node id as `block_id(offset)`.
    if let Some(tag) = block_tag {
        drop(fields.insert("block_ord".to_string(), tag.ord.clone()));
        drop(fields.insert("block_off".to_string(), tag.off.clone()));
    }

    let (name_id, node_kind_id, fql_kind_id, language_id, path_id) =
        sink.table
            .strings
            .intern_row(name, node.kind(), fql_kind, ctx.language_name, ctx.path);
    // Reuse prior ordinals when possible to keep node_id stable across re-indexes.
    let ordinal = if is_addressable_fql_kind(fql_kind) {
        Some(assign_ordinal(
            sink.remapper.as_deref_mut(),
            sink.row_ordinal_counter,
            &OrdinalMatchKey {
                name,
                fql_kind,
                parent_ordinal,
                guard_group_id: prepared.guard_group_id.as_deref(),
                guard_branch: prepared.guard_branch.as_deref(),
                first_body_statement_fingerprint: prepared
                    .first_body_statement_fingerprint
                    .as_deref(),
                content_hash: Some(prepared.content_hash.as_str()),
            },
        ))
    } else {
        None
    };
    // Fold leading `#[...]` attributes into the span (rev covers them); ordinal
    // matching keeps the unextended content_hash so attribute edits don't churn ids.
    let (start_byte, start_line) = attr_extended_start(node, ctx.language_config.decorator_kind());
    let span = start_byte..node.byte_range().end;
    let rev = row_rev(ordinal, source, span.clone());
    let fields = sink.table.strings.intern_fields(fields);
    sink.table.push_row(IndexRow {
        name_id,
        node_kind_id,
        fql_kind_id,
        language_id,
        path_id,
        byte_range: span,
        line: start_line,
        usages_count: 0,
        ordinal,
        parent_ordinal,
        rev,
        fields,
    });
    ordinal
}

/// Emit the synthetic rows contributed by the `extra_rows` of each enricher for
/// current node (e.g. usage sites, derived symbols). Runs for every node, even
/// when `extract_name` returned `None`. `parent_ordinal` is constant across the
/// extra rows of a node, so the caller computes it once.
fn emit_extra_rows(
    sink: &mut RowSink<'_>,
    ctx: &EnrichContext<'_>,
    parent_ordinal: u32,
) -> Option<u32> {
    let node = ctx.node;
    let source = ctx.source;
    let enrichers = sink.enrichers;
    let mut self_ordinal: Option<u32> = None;

    // The guard stack is fixed for this node, so the fields it produces are
    // built once here and copied onto each extra row. One node inside a guarded
    // region can emit many expression rows, and this path runs over every row in
    // the corpus. Outside a guarded region the map stays empty and the copy is a
    // no-op.
    let mut guard_fields: HashMap<String, String> = HashMap::new();
    inject_guard_fields(ctx.guard_stack, &mut guard_fields);
    for enricher in enrichers {
        for mut extra in enricher.extra_rows(ctx) {
            // A row sits inside whatever conditional region the walk is inside,
            // whatever kind of row it is. Copying the guard fields onto these
            // rows too, rather than only onto declaration-like ones, is what
            // lets a guard be paired with `is_magic`, `has_catch_all` or a
            // control-flow field at all.
            //
            // Before the `guard_group_id` read below, not after: those two feed
            // the ordinal key, so copying later would populate the fields but
            // leave the key blank.
            extra
                .fields
                .extend(guard_fields.iter().map(|(k, v)| (k.clone(), v.clone())));
            let guard_group_id = extra.fields.get("guard_group_id").map(String::as_str);
            let guard_branch = extra.fields.get("guard_branch").map(String::as_str);
            let content_hash = node_content_hash(node, source);
            let extra_path = extra.path_override.as_deref().unwrap_or(ctx.path);
            let (eni, enk, enf, enl, enp) = sink.table.strings.intern_row(
                &extra.name,
                &extra.node_kind,
                &extra.fql_kind,
                ctx.language_name,
                extra_path,
            );
            let ordinal = if is_addressable_fql_kind(&extra.fql_kind) {
                Some(assign_ordinal(
                    sink.remapper.as_deref_mut(),
                    sink.row_ordinal_counter,
                    &OrdinalMatchKey {
                        name: &extra.name,
                        fql_kind: &extra.fql_kind,
                        parent_ordinal,
                        guard_group_id,
                        guard_branch,
                        first_body_statement_fingerprint: None,
                        content_hash: Some(content_hash.as_str()),
                    },
                ))
            } else {
                None
            };
            // `is_self_row` (set by the enricher that produced the row) marks
            // the row representing the visited node itself. Capture its ordinal so
            // a control-flow node can become the parent of its body — an explicit
            // flag instead of an implicit `byte_range == node.byte_range()` match.
            if self_ordinal.is_none() && extra.is_self_row && ordinal.is_some() {
                self_ordinal = ordinal;
            }
            let rev = row_rev(ordinal, source, extra.byte_range.clone());
            let fields = sink.table.strings.intern_fields(extra.fields);
            sink.table.push_row(IndexRow {
                name_id: eni,
                node_kind_id: enk,
                fql_kind_id: enf,
                language_id: enl,
                path_id: enp,
                byte_range: extra.byte_range,
                line: extra.line,
                usages_count: 0,
                ordinal,
                parent_ordinal,
                rev,
                fields,
            });
        }
    }
    self_ordinal
}

/// Extract all grammar fields from a tree-sitter node into a string map.
fn extract_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    language: &tree_sitter::Language,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let field_count = language.field_count();

    for field_id in 1..=u16::try_from(field_count).unwrap_or(u16::MAX) {
        if let Some(child) = node.child_by_field_id(field_id)
            && let Some(field_name) = language.field_name_for_id(field_id)
        {
            let text = node_text(source, child);
            if !text.is_empty() {
                drop(fields.insert(field_name.to_string(), text));
            }
        }
    }

    fields
}
