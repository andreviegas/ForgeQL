//! Stable node-address helpers — SHA-256 path hashing and `node_id` formatting.
//!
//! A `node_id` is a compact, stable handle for a named AST node:
//!
//!   `n{segment_id}.{ordinal:04}`
//!
//! The `segment_id` portion is the shortest unambiguous prefix of the SHA-256
//! of the normalized source file path.  All computation is performed **once**
//! at [`crate::storage::columnar::overlay::Overlay`] open time and cached in
//! [`crate::storage::columnar::overlay::SegmentMeta`].  Query paths call only
//! string formatting — no SHA-256, no lock, no global state.

use sha2::{Digest, Sha256};

const DEFAULT_SEGMENT_PREFIX_HEX: u8 = 12;

fn normalize_relative_path(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_owned();
    }
    p
}

/// Compute the raw SHA-256 bytes of a normalized source-file path.
///
/// Called once per file during [`crate::storage::columnar::overlay::Overlay`]
/// open time inside `decode_segment_metas`.  Never called at query time.
#[must_use]
pub fn sha256_of_path(path: &str) -> [u8; 32] {
    let normalized = normalize_relative_path(path);
    Sha256::digest(normalized.as_bytes()).into()
}

/// Find the minimum hex-prefix length that makes `hash` unambiguous among
/// `all_hashes`.
///
/// Starts at 12 hex characters (6 raw bytes) and grows by 2 on collision.
/// Returns a count of hex characters (always even, range 12–64).
///
/// Called once per file during overlay open time.  Never called at query time.
#[must_use]
pub fn shortest_prefix_len(hash: &[u8; 32], all_hashes: &[[u8; 32]]) -> u8 {
    let mut hex_len = DEFAULT_SEGMENT_PREFIX_HEX;
    while hex_len < 64 {
        let prefix_bytes = usize::from(hex_len) / 2;
        let collision = all_hashes
            .iter()
            .any(|other| other != hash && other[..prefix_bytes] == hash[..prefix_bytes]);
        if !collision {
            return hex_len;
        }
        hex_len += 2;
    }
    64
}

/// Format a `segment_id` display string from raw SHA-256 bytes.
///
/// Reads `prefix_len / 2` bytes from `sha256` and formats them as lowercase
/// hex.  Pure formatting — no SHA-256, no lock, no allocation beyond the
/// returned `String`.
///
/// Callers should prefer
/// [`crate::storage::columnar::overlay::SegmentMeta::segment_id`]
/// which reads directly from the pre-computed fields.
#[must_use]
pub fn hex_prefix(sha256: &[u8; 32], prefix_len: u8) -> String {
    let byte_count = (prefix_len as usize) / 2;
    sha256[..byte_count]
        .iter()
        .fold(String::with_capacity(prefix_len as usize), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Format a `node_id` from raw SHA-256 prefix bytes and a per-file ordinal.
///
/// Format: `n{segment_id}.{ordinal:04}`.
///
/// Callers should prefer
/// [`crate::storage::columnar::overlay::SegmentMeta::node_id`].
#[must_use]
pub fn format_node_id(sha256: &[u8; 32], prefix_len: u8, ordinal: u32) -> String {
    format!("n{}.{ordinal:04}", hex_prefix(sha256, prefix_len))
}

/// Compute a `node_id` from a source file path and per-file ordinal.
///
/// Hashes `path` with SHA-256 and uses the default 12-hex-character prefix.
/// Use this only when no
/// [`crate::storage::columnar::overlay::SegmentMeta`] is available (e.g.
/// `SHOW body`, `SHOW members`).  For columnar paths (`SHOW outline`)
/// prefer [`crate::storage::columnar::overlay::SegmentMeta::node_id`]
/// which reads a pre-computed, dedup-aware prefix from the overlay.
#[must_use]
pub fn make_node_id(path: &str, ordinal: u32) -> String {
    let hash = sha256_of_path(path);
    format_node_id(&hash, DEFAULT_SEGMENT_PREFIX_HEX, ordinal)
}

/// Format a `rev` handle from the packed u64 stored in `col_rev`.
///
/// Format: `h{:016x}` — 16 lowercase hex chars prefixed with `h`.
/// The zero sentinel (analysis-only row) returns an empty string.
#[must_use]
pub fn format_rev(rev: u64) -> String {
    if rev == 0 {
        return String::new();
    }
    format!("h{rev:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_path_is_deterministic() {
        assert_eq!(sha256_of_path("src/main.rs"), sha256_of_path("src/main.rs"));
    }

    #[test]
    fn path_normalization_strips_dot_slash() {
        assert_eq!(
            sha256_of_path("./src/main.rs"),
            sha256_of_path("src/main.rs")
        );
        assert_eq!(
            sha256_of_path("src\\main.rs"),
            sha256_of_path("src/main.rs")
        );
    }

    #[test]
    fn shortest_prefix_len_minimum_is_12() {
        let h1 = sha256_of_path("src/main.rs");
        let all = vec![h1];
        assert_eq!(shortest_prefix_len(&h1, &all), 12);
    }

    #[test]
    fn hex_prefix_length_matches_prefix_len() {
        let h = sha256_of_path("src/lib.rs");
        let s = hex_prefix(&h, 12);
        assert_eq!(s.len(), 12);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn format_node_id_shape() {
        let h = sha256_of_path("src/main.rs");
        let id = format_node_id(&h, 12, 42);
        assert!(id.starts_with('n'));
        assert!(id.contains('.'));
        assert!(id.ends_with(".0042"));
    }
}
