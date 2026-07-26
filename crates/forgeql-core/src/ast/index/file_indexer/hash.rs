//! Content hashes behind a node's `rev`.
//!
//! Every hash here is truncated to the first eight bytes of a SHA-256, which
//! is what makes a `rev` short enough to travel in a CSV row beside its
//! handle. Changing any of this changes every rev in every index, so these
//! functions are stored-output even though they store nothing themselves.

use sha2::{Digest, Sha256};

use crate::ast::index::node_text;

pub(super) fn short_sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(super) fn first_body_statement_fingerprint(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    let text = node_text(source, first);
    if text.is_empty() {
        return None;
    }
    Some(short_sha256_hex(text.as_bytes()))
}

pub(super) fn node_content_hash(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let range = node.byte_range();
    let slice = source.get(range).unwrap_or_default();
    short_sha256_hex(slice)
}
