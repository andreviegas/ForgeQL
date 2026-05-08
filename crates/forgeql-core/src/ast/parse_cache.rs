//! Per-session LRU parse cache for tree-sitter parses.
//!
//! Keyed by SHA-1 content hash so entries naturally become stale when
//! file content changes — no explicit invalidation needed.
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use sha1::{Digest, Sha1};

use crate::ast::lang::LanguageRegistry;

// -----------------------------------------------------------------------
// CachedParse
// -----------------------------------------------------------------------

/// A parsed source file held in the per-session parse cache.
pub struct CachedParse {
    /// Source bytes read from disk.
    pub source: Arc<Vec<u8>>,
    /// Parsed tree-sitter syntax tree.
    pub tree: tree_sitter::Tree,
}

// -----------------------------------------------------------------------
// ParseCache
// -----------------------------------------------------------------------

/// Per-session LRU parse cache keyed by SHA-1 content hash.
///
/// Capacity defaults to 32 entries. Eviction is LRU — the least-recently
/// used entry is dropped when the cache is full and a new entry is inserted.
///
/// Stale-safety: when a file is changed its SHA-1 hash changes, so the old
/// entry is bypassed on the next lookup without explicit invalidation.
pub struct ParseCache {
    capacity: usize,
    /// LRU order: front = least-recently-used, back = most-recently-used.
    order: VecDeque<[u8; 20]>,
    map: HashMap<[u8; 20], Arc<CachedParse>>,
}

impl ParseCache {
    /// Create a new `ParseCache` with the given entry capacity (minimum 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    /// Look up a cached parse by SHA-1 hash.
    ///
    /// Moves the entry to the MRU position on hit.
    pub fn get(&mut self, hash: &[u8; 20]) -> Option<Arc<CachedParse>> {
        let parse = self.map.get(hash).cloned()?;
        if let Some(pos) = self.order.iter().position(|h| h == hash) {
            let _ = self.order.remove(pos);
            self.order.push_back(*hash);
        }
        Some(parse)
    }

    /// Insert a new cache entry, evicting the LRU entry when at capacity.
    ///
    /// No-op if the hash is already present.
    pub fn insert(&mut self, hash: [u8; 20], parse: Arc<CachedParse>) {
        if self.map.contains_key(&hash) {
            return;
        }
        if self.order.len() >= self.capacity
            && let Some(lru) = self.order.pop_front()
        {
            let _ = self.map.remove(&lru);
        }
        self.order.push_back(hash);
        let _ = self.map.insert(hash, parse);
    }

    /// Get or parse the file at `path`, using the cache for repeated reads.
    ///
    /// On cache miss: reads bytes, computes SHA-1, parses with the grammar
    /// registered for the path extension, and inserts the result.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, no language is registered
    /// for the file extension, or tree-sitter fails to parse the content.
    pub fn get_or_parse(
        &mut self,
        path: &Path,
        lang_registry: &LanguageRegistry,
    ) -> Result<Arc<CachedParse>> {
        self.get_or_parse_with_hint(path, lang_registry, None)
    }

    /// Like [`get_or_parse`] but accepts a pre-computed SHA-1 hint.
    ///
    /// When `blob_sha` is `Some(sha)`:
    /// - **Cache hit**: returns immediately — no file read, no SHA computation.
    /// - **Cache miss**: reads the file and parses it, but uses `sha` directly
    ///   as the cache key (skips `sha1_of_bytes`).
    ///
    /// When `blob_sha` is `None` the behaviour is identical to `get_or_parse`.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, no language is registered
    /// for the file extension, or tree-sitter fails to parse the content.
    pub fn get_or_parse_with_hint(
        &mut self,
        path: &Path,
        lang_registry: &LanguageRegistry,
        blob_sha: Option<&[u8; 20]>,
    ) -> Result<Arc<CachedParse>> {
        if let Some(sha) = blob_sha {
            // Fast path: hit check without touching disk.
            if let Some(hit) = self.get(sha) {
                return Ok(hit);
            }
            // Cache miss: read and parse, store under the provided key.
            let bytes = crate::workspace::file_io::read_bytes(path)?;
            return self.parse_and_insert(*sha, path, bytes, lang_registry);
        }

        // No hint: read the file and derive the key from its content.
        let bytes = crate::workspace::file_io::read_bytes(path)?;
        let hash = sha1_of_bytes(&bytes);
        if let Some(hit) = self.get(&hash) {
            return Ok(hit);
        }
        self.parse_and_insert(hash, path, bytes, lang_registry)
    }

    /// Parse `bytes` with the language inferred from `path`, insert the result
    /// under `hash`, and return it.
    fn parse_and_insert(
        &mut self,
        hash: [u8; 20],
        path: &Path,
        bytes: Vec<u8>,
        lang_registry: &LanguageRegistry,
    ) -> Result<Arc<CachedParse>> {
        let lang = lang_registry
            .language_for_path(path)
            .ok_or_else(|| anyhow::anyhow!("no language registered for {}", path.display()))?;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .map_err(|e| anyhow::anyhow!("tree-sitter language error: {e}"))?;
        let tree = parser
            .parse(&bytes, None)
            .ok_or_else(|| anyhow::anyhow!("failed to parse {}", path.display()))?;
        let cached = Arc::new(CachedParse {
            source: Arc::new(bytes),
            tree,
        });
        self.insert(hash, Arc::clone(&cached));
        Ok(cached)
    }

    /// Number of entries currently in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns `true` if the cache holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Compute SHA-1 of the given byte slice.
#[must_use]
pub fn sha1_of_bytes(bytes: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(bytes);
    h.finalize().into()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_capacity_minimum_is_one() {
        let c = ParseCache::with_capacity(0);
        assert_eq!(c.capacity, 1);
    }

    #[test]
    fn get_on_empty_returns_none() {
        let mut c = ParseCache::with_capacity(4);
        assert!(c.get(&[0u8; 20]).is_none());
    }

    #[test]
    fn is_empty_on_fresh_cache() {
        let c = ParseCache::with_capacity(4);
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn sha1_is_deterministic() {
        let h1 = sha1_of_bytes(b"hello");
        let h2 = sha1_of_bytes(b"hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha1_different_content_differs() {
        let h1 = sha1_of_bytes(b"hello");
        let h2 = sha1_of_bytes(b"world");
        assert_ne!(h1, h2);
    }
}
