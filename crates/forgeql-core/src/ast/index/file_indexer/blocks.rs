//! Block groups — a run of adjacent siblings surfaced as one addressable node.
//!
//! A `comment_block` or `array_block` is not in any grammar. The indexer
//! synthesises it from a run of same-kind siblings so an agent can address the
//! whole run at once. The members keep their own rows and node ids — the block
//! is their *sibling*, not their parent — and each is tagged with the block's
//! address so it can also be reached through it.
//!
//! `scan_block_run` walks with `next_named_sibling` rather than
//! `next_sibling`, and that distinction is load-bearing: run members are often
//! separated by anonymous punctuation, so a raw sibling walk breaks the run at
//! the first separator. It is why `array_block` once shipped emitting nothing
//! at all, and why the failure hid — Rust comment runs have no separator, so
//! for them the two walks agree.

use std::collections::HashMap;

use crate::ast::lang::{BlockGroupSpec, LanguageSupport};

use crate::ast::index::IndexRow;

use super::IndexContext;
use super::hash::short_sha256_hex;
use super::ordinals::{OrdinalMatchKey, assign_ordinal};
use super::rows::row_rev;

/// Grouping key for a block-group member. Members that share a key AND are
/// adjacent tree siblings coalesce into one block. For `split_on_attr =
/// "comment_style"` the key is the comment style, so `///` doc runs and `//`
/// line runs form separate blocks; otherwise every member of the kind shares
/// one key.
/// State for the block currently being spanned, carried across loop iterations
/// so each member of the run can be tagged with the block's address.
pub(super) struct ActiveBlock {
    /// 4-digit ordinal suffix of the block node (matches the `node_id` format),
    /// e.g. `"0123"` for ordinal 123.
    pub(super) ord_suffix: String,
    /// 1-based start line of the block (used to compute member offsets).
    pub(super) start_line: usize,
    /// End byte of the block span; once a node starts at/after this, the block is
    /// closed.
    pub(super) end_byte: usize,
    /// FQL kind of the run's members (only these nodes are tagged).
    pub(super) member_fql_kind: String,
}

/// Per-member block address, written onto the member row as `block_ord` /
/// `block_off` fields so `FIND`/`SHOW` can surface the member as
/// `block_id(offset)` (Stage 2 alias).
pub(super) struct BlockTag {
    /// 4-digit ordinal suffix of the owning block node.
    pub(super) ord: String,
    /// 1-based offset (or `start-end` range) of the member within the block.
    pub(super) off: String,
}
/// The grouping key for one block-group member.
///
/// Core knows only the rule "same key groups, different key splits the run"; it
/// asks the *language* what the key is. Before this, the key was computed here
/// by matching the literal string `"comment_style"` — a language-shaped fact
/// living in the language-agnostic core. Now the whole decision belongs to
/// `LanguageSupport::block_group_key`, so a new format (CSV grouping records by
/// field count, JSON grouping array elements) needs no core change at all.
pub(super) fn block_group_key(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    lang: &dyn LanguageSupport,
    spec: &BlockGroupSpec,
) -> String {
    lang.block_group_key(node, source, spec.split_on_attr.as_deref())
}

/// Walk forward over tree siblings, extending a run while each sibling is the
/// same member kind and grouping key. Blank lines are bridged for free: blank
/// lines are not tree nodes, so two same-kind declarations separated only by
/// blank lines are adjacent siblings. A node of any other kind ends the run.
/// Returns `(member_count, run_end_byte)`.
pub(super) fn scan_block_run(
    first: tree_sitter::Node<'_>,
    source: &[u8],
    lang: &dyn LanguageSupport,
    spec: &BlockGroupSpec,
    key: &str,
) -> (usize, usize) {
    let mut count = 1usize;
    let mut end_byte = first.byte_range().end;
    let mut cursor = first;
    // `next_named_sibling`, NOT `next_sibling`: a run's members are often
    // separated by anonymous punctuation. JSON array elements are separated by
    // `,` tokens, whose `map_kind` is empty — walking raw siblings breaks the
    // run at the first comma, so a 201-element array scanned as a run of ONE and
    // no block was ever emitted. Rust comment runs have no separator between
    // members, which is why this went unnoticed: for them the two walks agree.
    while let Some(sib) = cursor.next_named_sibling() {
        if lang.map_kind(sib.kind()).unwrap_or("") != spec.member_fql_kind {
            break;
        }
        if block_group_key(sib, source, lang, spec) != key {
            break;
        }
        count += 1;
        end_byte = sib.byte_range().end;
        cursor = sib;
    }
    (count, end_byte)
}

/// Emit a synthetic, childless "block" row spanning a run of grouped members.
/// The block shares the `parent_ordinal` of its members — it is their sibling,
/// never their parent — and gives one addressable handle over the whole run.
/// The member rows are emitted normally and keep their own node ids.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_block_row(
    ctx: &mut IndexContext<'_>,
    spec: &BlockGroupSpec,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    parent_ordinal: u32,
    row_ordinal_counter: &mut u32,
    source: &[u8],
    label: &str,
) -> u32 {
    let span = start_byte..end_byte;
    let block_kind = spec.block_fql_kind.as_str();
    let content_hash = short_sha256_hex(source.get(span.clone()).unwrap_or_default());
    let path = ctx.path;
    let lang_name = ctx.language.name();
    let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = ctx
        .table
        .strings
        .intern_row(block_kind, block_kind, block_kind, lang_name, path);
    let ordinal = assign_ordinal(
        ctx.ordinal_remapper.as_mut(),
        row_ordinal_counter,
        &OrdinalMatchKey {
            name: block_kind,
            fql_kind: block_kind,
            parent_ordinal,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: None,
            content_hash: Some(content_hash.as_str()),
        },
    );
    let rev = row_rev(Some(ordinal), source, span.clone());
    // Carry the content hash so the reindex hint can disambiguate this block from
    // sibling blocks (which all share the constant `comment_block` identity name)
    // and keep its node id stable across edits to other blocks. `block_label` is a
    // display-only field (first-member snippet + member count) surfaced by SHOW
    // outline; the identity name_id stays `comment_block`.
    let mut block_fields = HashMap::new();
    drop(block_fields.insert("content_hash".to_string(), content_hash.clone()));
    drop(block_fields.insert("block_label".to_string(), label.to_string()));
    let fields = ctx.table.strings.intern_fields(block_fields);
    ctx.table.push_row(IndexRow {
        name_id,
        node_kind_id,
        fql_kind_id,
        language_id,
        path_id,
        byte_range: span,
        line: start_line,
        usages_count: 0,
        ordinal: Some(ordinal),
        parent_ordinal,
        rev,
        fields,
    });
    ordinal
}

/// Walk back over the contiguous run of leading attribute items (`#[...]`)
/// preceding `node` and return the `(start_byte, 1-based start_line)` of the
/// first attribute, so a node's span folds in its operational attributes.
/// Falls back to the node's own start when there are none.
///
/// Matches `collect_attribute_guard_frames`' detection (`attribute_item` via
/// `prev_named_sibling`), so today this only folds Rust attributes; other
/// languages' attribute kinds don't match and are left unchanged.
pub(super) fn attr_extended_start(
    node: tree_sitter::Node<'_>,
    decorator_kind: Option<&str>,
) -> (usize, usize) {
    let Some(attr_kind) = decorator_kind else {
        return (node.start_byte(), node.start_position().row + 1);
    };
    let mut start_byte = node.start_byte();
    let mut start_line = node.start_position().row + 1;
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        if sib.kind() != attr_kind {
            break;
        }
        start_byte = sib.start_byte();
        start_line = sib.start_position().row + 1;
        prev = sib.prev_named_sibling();
    }
    (start_byte, start_line)
}
