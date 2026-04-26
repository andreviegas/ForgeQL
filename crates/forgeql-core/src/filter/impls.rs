//! `ClauseTarget` implementations for all filterable result types.
use std::path::Path;

use super::ClauseTarget;
use crate::ast::index::RowRef;

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

impl ClauseTarget for RowRef<'_> {
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(self.table.name_of(self.row)),
            "node_kind" => Some(self.table.node_kind_of(self.row)),
            "fql_kind" => {
                let s = self.table.fql_kind_of(self.row);
                if s.is_empty() { None } else { Some(s) }
            }
            "language" | "lang" => {
                let s = self.table.language_of(self.row);
                if s.is_empty() { None } else { Some(s) }
            }
            "path" | "file" => self.table.path_of(self.row).to_str(),
            other => self.row.fields.get(other).map(String::as_str),
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.row.line).unwrap_or(i64::MAX)),
            "usages" => Some(i64::from(self.row.usages_count)),
            _ => self.row.fields.get(field)?.parse().ok(),
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(self.table.path_of(self.row))
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
