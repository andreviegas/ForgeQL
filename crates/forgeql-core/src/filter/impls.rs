//! `ClauseTarget` implementations for all filterable result types.
use std::path::Path;

use super::ClauseTarget;
use crate::ast::index::IndexRow;

// -----------------------------------------------------------------------
// ClauseTarget implementations
// -----------------------------------------------------------------------
impl ClauseTarget for crate::result::SymbolMatch {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(&self.name),
            "node_kind" => self.node_kind.as_deref(),
            "fql_kind" => self.fql_kind.as_deref(),
            "language" | "lang" => self.language.as_deref(),
            "path" | "file" => self.path.as_deref().and_then(|p| p.to_str()),
            other => self.fields.get(other).map(String::as_str),
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "usages" => self
                .usages_count
                .map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            "count" => self.count.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            "line" => self.line.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            _ => self.fields.get(field)?.parse().ok(),
        }
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn set_count(&mut self, count: usize) {
        self.count = Some(count);
    }
}

impl ClauseTarget for IndexRow {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(self.name.as_str()),
            "node_kind" => Some(self.node_kind.as_str()),
            "fql_kind" => {
                if self.fql_kind.is_empty() {
                    None
                } else {
                    Some(self.fql_kind.as_str())
                }
            }
            "language" | "lang" => {
                if self.language.is_empty() {
                    None
                } else {
                    Some(self.language.as_str())
                }
            }
            "path" | "file" => self.path.to_str(),
            other => self.fields.get(other).map(String::as_str),
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.line).unwrap_or(i64::MAX)),
            // "usages" requires the index; annotate via SymbolMatch instead.
            _ => self.fields.get(field)?.parse().ok(),
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

impl ClauseTarget for crate::result::FileEntry {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "path" | "file" => self.path.to_str(),
            "extension" | "ext" => Some(&self.extension),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "size" => Some(i64::try_from(self.size).unwrap_or(i64::MAX)),
            "depth" => self.depth.map(|d| i64::try_from(d).unwrap_or(i64::MAX)),
            "count" => self.count.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn set_count(&mut self, count: usize) {
        self.count = Some(count);
    }
}

impl ClauseTarget for crate::result::OutlineEntry {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(&self.name),
            "fql_kind" => Some(&self.fql_kind),
            "path" | "file" => self.path.to_str(),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.line).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

impl ClauseTarget for crate::result::MemberEntry {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "fql_kind" | "type" => Some(&self.fql_kind),
            "text" | "declaration" | "name" => Some(&self.text),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.line).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        None
    }
}

impl ClauseTarget for crate::result::SourceLine {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "text" | "content" => Some(&self.text),
            "marker" => self.marker.as_deref(),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.line).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        None
    }
}

impl ClauseTarget for crate::result::CallGraphEntry {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(&self.name),
            "path" | "file" => self.path.as_deref().and_then(|p| p.to_str()),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => self.line.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}
