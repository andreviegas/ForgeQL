//! JSON and CSV serialization for `ForgeQLResult`.
use super::{ForgeQLResult, compact_name};

// -----------------------------------------------------------------------
// Conversion helpers
// -----------------------------------------------------------------------
impl ForgeQLResult {
    /// Serialize to a JSON string for MCP tool responses and pipe output.
    ///
    /// All inner types derive `Serialize`, so serialization cannot fail
    /// under normal conditions.
    #[must_use]
    pub fn to_json(&self) -> String {
        // Safety: all fields are Serialize-derived primitives, Strings,
        // PathBufs, and Vecs — serialization is infallible.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Serialize to a pretty-printed JSON string.
    ///
    /// All inner types derive `Serialize`, so serialization cannot fail
    /// under normal conditions.
    #[must_use]
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Serialize query results to a compact CSV-style envelope.
    ///
    /// Produces a minimal JSON object with a `results` array where each entry
    /// is a flat string array `["name", "node_kind", "path", "count"]`.
    ///
    /// The `count` column covers two cases:
    /// - `usages_count` — number of references to the symbol (FIND queries)
    /// - `count` — per-file hit count (COUNT … GROUP BY file)
    ///
    /// This format is ~60% smaller than the full JSON and keeps large query
    /// responses within the MCP inline-response threshold.
    ///
    /// Non-query results (mutations, show, source ops) fall back to `to_json()`
    /// since they lack a uniform tabular structure.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let Self::Query(query) = self else {
            return self.to_json();
        };
        // For FIND usages (no GROUP BY) there is no count — each row is one
        // call site and the 4th column carries the line number. Label it
        // honestly so callers are not confused by a "count" that is actually
        // a line number.
        let count_col = if query.op == "find_usages" {
            "line"
        } else {
            "count"
        };
        let mut all_rows: Vec<serde_json::Value> =
            vec![serde_json::json!(["name", "node_kind", "path", count_col])];
        all_rows.extend(query.results.iter().map(|row| {
            // `usages_count` is populated by FIND queries;
            // `count` is populated by COUNT … GROUP BY;
            // `line` is the fallback for FIND usages rows (makes each
            // call site distinguishable when multiple appear in the same file).
            let count_str = row
                .usages_count
                .or(row.count)
                .map(|n| n.to_string())
                .or_else(|| row.line.map(|l| l.to_string()))
                .unwrap_or_default();
            serde_json::json!([
                compact_name(&row.name),
                row.node_kind.as_deref().unwrap_or(""),
                row.path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                count_str,
            ])
        }));
        serde_json::to_string(&serde_json::json!({
            "total": query.total,
            "results": all_rows,
        }))
        .unwrap_or_default()
    }
}
