//! Stable node-address helpers — `segment_id` and `node_id` computation.
//!
//! A `node_id` is a compact, stable handle for a named AST node:
//!
//!   `n{segment_id}.{ordinal:04}`
//!
//! where `segment_id` is the first 6 bytes of SHA-1(path) as 12 lowercase hex
//! chars, and `ordinal` is the per-file DFS counter assigned during indexing.

use sha1::{Digest, Sha1};

/// Compute the 12-hex-char segment identifier for a source file path.
///
/// Defined as the first 6 bytes of SHA-1(path_bytes) as lowercase hex.
#[must_use]
pub fn segment_id(path: &str) -> String {
    let digest = Sha1::digest(path.as_bytes());
    let mut s = String::with_capacity(12);
    for b in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Format a node identifier from a file path and per-file DFS ordinal.
///
/// Format: `n{12-hex-char segment_id}.{ordinal:04}`
#[must_use]
pub fn make_node_id(path: &str, ordinal: u32) -> String {
    format!("n{}.{ordinal:04}", segment_id(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_id_is_12_hex_chars() {
        let s = segment_id("src/main.rs");
        assert_eq!(s.len(), 12);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn make_node_id_format() {
        let id = make_node_id("src/main.rs", 42);
        assert!(id.starts_with('n'));
        assert!(id.contains('.'));
        // ordinal is zero-padded to 4 digits
        assert!(id.ends_with(".0042"));
    }

    #[test]
    fn segment_id_is_deterministic() {
        assert_eq!(segment_id("foo/bar.rs"), segment_id("foo/bar.rs"));
    }

    #[test]
    fn segment_id_differs_for_different_paths() {
        assert_ne!(segment_id("src/a.rs"), segment_id("src/b.rs"));
    }
}
