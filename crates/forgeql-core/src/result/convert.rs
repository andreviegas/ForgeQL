//! JSON and CSV serialization for `ForgeQLResult`.
use super::ForgeQLResult;

// -----------------------------------------------------------------------
// Conversion helpers
// -----------------------------------------------------------------------
impl ForgeQLResult {
    /// Serialize to a JSON string for MCP tool responses and pipe output.
    ///
    /// For query results, only the projected `SymbolRow` fields are emitted —
    /// the raw `SymbolMatch.fields` `HashMap` is never exposed.
    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            Self::Query(query) => query.to_query_json(false),
            _ => serde_json::to_string(self).unwrap_or_default(),
        }
    }

    /// Serialize to a pretty-printed JSON string.
    ///
    /// For query results, only the projected `SymbolRow` fields are emitted.
    #[must_use]
    pub fn to_json_pretty(&self) -> String {
        match self {
            Self::Query(query) => query.to_query_json(true),
            _ => serde_json::to_string_pretty(self).unwrap_or_default(),
        }
    }
}

impl super::QueryResult {
    /// Build a JSON representation using projected rows only.
    fn to_query_json(&self, pretty: bool) -> String {
        let rows: Vec<serde_json::Value> = self
            .projected_rows()
            .iter()
            .map(|sr| {
                let mut obj = serde_json::json!({
                    "name": sr.name,
                    "kind": sr.kind,
                    "path": sr.path,
                    "line": sr.line,
                });
                if let Some(ref fn_name) = sr.enclosing_fn {
                    obj["enclosing_fn"] = serde_json::json!(fn_name);
                }
                if let Some(usages) = sr.usages {
                    obj["usages"] = serde_json::json!(usages);
                }
                if let Some(count) = sr.count {
                    obj["count"] = serde_json::json!(count);
                }
                if let Some(ref mv) = sr.metric_value {
                    obj["metric_value"] = serde_json::json!(mv);
                }
                if let Some(ref gk) = sr.group_key {
                    obj["group_key"] = serde_json::json!(gk);
                }
                obj
            })
            .collect();
        let result = serde_json::json!({
            "op": self.op,
            "total": self.total,
            "results": rows,
        });
        if pretty {
            serde_json::to_string_pretty(&result).unwrap_or_default()
        } else {
            serde_json::to_string(&result).unwrap_or_default()
        }
    }
}
