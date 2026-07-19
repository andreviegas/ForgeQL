//! Result types for source and session lifecycle operations.

use serde::{Deserialize, Serialize};

/// Internal stats for one loaded session, produced by `SHOW STATS`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    /// Session alias (the identifier passed to `USE … AS`).
    pub session_id: String,
    /// Source name (repo) bound to this session.
    pub source: String,
    /// Branch name bound to this session.
    pub branch: String,
    /// Total number of indexed rows (symbols).
    pub rows: usize,
    /// Distinct symbol names in the intern pool.
    pub distinct_names: usize,
    /// Distinct file paths in the intern pool.
    pub distinct_paths: usize,
    /// Number of distinct symbols that have at least one usage site.
    pub usage_symbols: usize,
    /// Total number of individual usage-site records.
    pub usage_sites: usize,
    /// Number of distinct trigrams in the trigram index.
    pub trigram_distinct: usize,
    /// Approximate heap bytes consumed by the index (all components).
    pub mem_total_bytes: usize,
    /// Approximate heap bytes: `rows` `Vec` + per-row enrichment `HashMap`s.
    pub mem_rows_bytes: usize,
    /// Approximate heap bytes: usages `HashMap`.
    pub mem_usages_bytes: usize,
    /// Approximate heap bytes: `name/kind/fql_kind` secondary indexes.
    pub mem_indexes_bytes: usize,
    /// Approximate heap bytes: trigram index posting lists.
    pub mem_trigram_bytes: usize,
    /// Approximate heap bytes: all five intern pools (`ColumnarTable`).
    pub mem_strings_bytes: usize,
    /// By-language symbol counts (from `IndexStats`).
    pub by_language: std::collections::HashMap<String, usize>,
    /// By-fql_kind symbol counts (from `IndexStats`).
    pub by_fql_kind: std::collections::HashMap<String, usize>,
}

// -----------------------------------------------------------------------
// Source/session operation results
// -----------------------------------------------------------------------

/// Result of a source or session lifecycle operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceOpResult {
    /// Operation name: `"create_source"`, `"use_source"`, `"disconnect"`, etc.
    pub op: String,
    /// Source repository name (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Session identifier (returned by USE, consumed by subsequent commands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Available branches (returned by CREATE SOURCE, SHOW BRANCHES).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// Number of symbols indexed (returned by USE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols_indexed: Option<usize>,
    /// Whether an existing session was resumed (vs. created fresh).
    #[serde(default)]
    pub resumed: bool,
    /// Human-readable status message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Base commit the session was created on (USE only) - the resolved hash a
    /// branch name or `<commit-hex>` peeled to, so a stacking agent can confirm
    /// exactly what it based on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}
