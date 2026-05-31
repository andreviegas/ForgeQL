//! Stable node-address helpers — `segment_id` and `node_id` computation.
//!
//! A `node_id` is a compact, stable handle for a named AST node:
//!
//!   `n{segment_id}.{ordinal:04}`

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

const DEFAULT_SEGMENT_PREFIX_HEX: usize = 12;

#[derive(Default)]
struct SegmentIdRegistry {
    /// Normalized path -> full SHA-256 hex digest.
    digests: HashMap<String, String>,
}

static SEGMENT_ID_REGISTRY: OnceLock<Mutex<SegmentIdRegistry>> = OnceLock::new();

fn normalize_relative_path(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_owned();
    }
    p
}

fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in &digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn shortest_unambiguous_prefix_hex(hash: &str, all_hashes: &[String]) -> usize {
    let mut len = DEFAULT_SEGMENT_PREFIX_HEX;
    while len < hash.len() {
        let prefix = &hash[..len];
        let collision = all_hashes
            .iter()
            .any(|other| other.as_str() != hash && other.starts_with(prefix));
        if !collision {
            return len;
        }
        len += 2;
    }
    hash.len()
}

/// Compute the segment identifier for a source file path.
///
/// Defined as SHA-256 of the normalized relative path, serialized as the
/// minimum unambiguous lowercase-hex prefix. Prefix length starts at 12 chars
/// and grows by 2 chars only when collisions are observed.
#[must_use]
pub fn segment_id(path: &str) -> String {
    let normalized = normalize_relative_path(path);
    let digest = sha256_hex(&normalized);

    let registry = SEGMENT_ID_REGISTRY.get_or_init(|| Mutex::new(SegmentIdRegistry::default()));
    let mut guard = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let _ = guard.digests.insert(normalized, digest.clone());
    let all_hashes: Vec<String> = guard.digests.values().cloned().collect();
    drop(guard);
    let prefix_len = shortest_unambiguous_prefix_hex(&digest, &all_hashes);
    digest[..prefix_len].to_owned()
}

/// Format a node identifier from a file path and per-file ordinal.
///
/// Format: `n{segment_id}.{ordinal:04}`
#[must_use]
pub fn make_node_id(path: &str, ordinal: u32) -> String {
    format!("n{}.{ordinal:04}", segment_id(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_id_is_at_least_12_hex_chars() {
        let s = segment_id("src/main.rs");
        assert!(s.len() >= 12);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn make_node_id_format() {
        let id = make_node_id("src/main.rs", 42);
        assert!(id.starts_with('n'));
        assert!(id.contains('.'));
        assert!(id.ends_with(".0042"));
    }

    #[test]
    fn segment_id_is_deterministic() {
        assert_eq!(segment_id("foo/bar.rs"), segment_id("foo/bar.rs"));
    }

    #[test]
    fn path_normalization_keeps_same_segment_id() {
        assert_eq!(segment_id("./src/main.rs"), segment_id("src/main.rs"));
        assert_eq!(segment_id("src\\main.rs"), segment_id("src/main.rs"));
    }
}
