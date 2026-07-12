//! Shared structural naming rules for the structured-text formats.
//!
//! JSON, YAML and TOML differ only in their tree-sitter node-kind vocabulary;
//! the rules that turn a node into an *addressable name* are identical. They
//! live here so that changing the naming ladder is a one-file edit.
//!
//! # The ladder
//!
//! | node | name |
//! |---|---|
//! | pair / mapping entry | the (unquoted) key text |
//! | container with an identifier-like member | that member's value |
//! | container without one | the key-set skeleton (`uses`, `name,run`) |
//! | container with no members at all | the nearest ancestor pair's key |
//! | sequence | the nearest ancestor pair's key (`steps`) |
//! | anything else | `None` — no row is emitted |
//!
//! # Why a name never encodes a position
//!
//! `OrdinalRemapper` re-attaches a node to its previous ordinal by matching
//! `(name, fql_kind, parent_ordinal)`. A name that encoded a slot — `steps[0]` —
//! would follow the *position* rather than the node: swap two sibling elements
//! and each one matches the other's hint, so the two nodes trade ordinals and a
//! handle held for one silently resolves to the other. Every name produced here
//! derives from the node's own content, never from its index among its siblings
//! — the same contract as the condition skeleton that names `if` statements.

#![allow(clippy::doc_markdown)]

use forgeql_core::ast::lang::node_text;

/// Per-format node-kind vocabulary — the only thing each plugin supplies.
pub struct StructureSpec {
    /// Kinds that are key/value pairs (`pair`, `block_mapping_pair`, ...).
    pub pair_kinds: &'static [&'static str],
    /// Kinds that are keyed containers (`object`, `block_mapping`, ...).
    pub container_kinds: &'static [&'static str],
    /// Kinds that are ordered sequences (`array`, `block_sequence`, ...).
    pub sequence_kinds: &'static [&'static str],
    /// Member keys, in priority order, that name their enclosing container.
    pub identifier_keys: &'static [&'static str],
    /// Strip this format's quoting from a scalar's raw text.
    pub unquote: fn(&str) -> &str,
}

/// Upper bound on the keys folded into a key-set skeleton, so that a very wide
/// container cannot produce an unbounded name.
const MAX_SKELETON_KEYS: usize = 8;

/// The (unquoted) text of a pair's `key` field, if any.
#[must_use]
pub fn pair_key(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    spec: &StructureSpec,
) -> Option<String> {
    let key = node.child_by_field_name("key")?;
    let raw = node_text(source, key);
    let name = (spec.unquote)(&raw).to_string();
    (!name.is_empty()).then_some(name)
}

/// Name a container after the value of its first identifier-like member.
fn identifier_member(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    spec: &StructureSpec,
) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if !spec.pair_kinds.contains(&child.kind()) {
            continue;
        }
        let Some(key) = child.child_by_field_name("key") else {
            continue;
        };
        let key_raw = node_text(source, key);
        if !spec.identifier_keys.contains(&(spec.unquote)(&key_raw)) {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        let value_raw = node_text(source, value);
        let value_name = (spec.unquote)(&value_raw).to_string();
        if !value_name.is_empty() {
            return Some(value_name);
        }
    }
    None
}

/// Name an identifier-less container after its **key set**: the sorted, deduped
/// keys of its direct members, comma-joined (`uses`, `name,run`).
///
/// The key set is stable under exactly the edits that must not change identity:
/// reordering members, and editing a member's *value* (`@v4` to `@v5` leaves the
/// key set `{uses}` untouched). It changes only when a key is added or removed —
/// which genuinely is a different node.
fn key_set_skeleton(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    spec: &StructureSpec,
) -> Option<String> {
    let mut cursor = node.walk();
    let mut keys: Vec<String> = node
        .named_children(&mut cursor)
        .filter(|child| spec.pair_kinds.contains(&child.kind()))
        .filter_map(|child| pair_key(child, source, spec))
        .collect();
    if keys.is_empty() {
        return None;
    }
    keys.sort_unstable();
    keys.dedup();
    let truncated = keys.len() > MAX_SKELETON_KEYS;
    keys.truncate(MAX_SKELETON_KEYS);
    let mut name = keys.join(",");
    if truncated {
        name.push_str(",…");
    }
    Some(name)
}

/// Name a node after the key of its nearest ancestor pair — the breadcrumb.
///
/// Returns `None` for a node with no ancestor pair (an anonymous root
/// container), which therefore emits no row of its own; its children become the
/// top-level rows, matching how an anonymous root array behaves today.
fn breadcrumb_key(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    spec: &StructureSpec,
) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if spec.pair_kinds.contains(&ancestor.kind()) {
            return pair_key(ancestor, source, spec);
        }
        current = ancestor.parent();
    }
    None
}

/// The naming ladder, applied in order. Every structured-text plugin delegates
/// its `extract_name` to this single entry point.
#[must_use]
pub fn structured_name(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    spec: &StructureSpec,
) -> Option<String> {
    let kind = node.kind();
    if spec.pair_kinds.contains(&kind) {
        pair_key(node, source, spec)
    } else if spec.container_kinds.contains(&kind) {
        identifier_member(node, source, spec)
            .or_else(|| key_set_skeleton(node, source, spec))
            .or_else(|| breadcrumb_key(node, source, spec))
    } else if spec.sequence_kinds.contains(&kind) {
        breadcrumb_key(node, source, spec)
    } else {
        None
    }
}
