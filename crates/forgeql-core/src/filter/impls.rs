//! `ClauseTarget` implementations for all filterable result types.
//!
//! Each impl declares the field names it resolves as well as resolving them,
//! and the two must agree: `STR_FIELDS` / `NUM_FIELDS` are what the clause
//! validator refuses against, and a name listed but not matched below is a
//! query accepted and then answered with nothing.
//!
//! Only canonical spellings appear here. `eval_predicate`, `order_cmp` and
//! `apply_group_by` put a clause field through `field_tiers::canonical` before
//! it reaches a row, so `file`, `kind`, `lang`, `ext` and `content` arrive as
//! `path`, `fql_kind`, `language`, `extension` and `text`. Adding an alias is
//! a one-line change to the table, not eight changes here.

use std::path::Path;

use super::ClauseTarget;
use crate::ast::index::RowRef;

// -----------------------------------------------------------------------
// ClauseTarget implementations
// -----------------------------------------------------------------------

impl ClauseTarget for crate::result::SymbolMatch {
    const ROW: &'static str = "a symbol row";
    // `body` and `role` are not struct fields: materialisation writes them
    // into the open map, `body` when the row's source is read and `role` on an
    // occurrence row. They resolve, so they are declared.
    const STR_FIELDS: &'static [&'static str] = &[
        "name",
        "node_kind",
        "fql_kind",
        "language",
        "path",
        "node_id",
        "body",
        "role",
        "value",
        "type",
    ];
    const NUM_FIELDS: &'static [&'static str] = &["usages", "count", "line"];
    // The enrichment map is open: which names it carries depends on the
    // registered language plugins and on what each segment stored, so only the
    // backend holding the index can refuse an unlisted name.
    const OPEN_FIELDS: bool = true;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(&self.name),
            "node_kind" => self.node_kind.as_deref(),
            "fql_kind" => self.fql_kind.as_deref(),
            "language" => self.language.as_deref(),
            "path" => self.path.as_deref().and_then(|p| p.to_str()),
            "node_id" => self.node_id.as_deref(),
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
    const ROW: &'static str = "a symbol row";
    const STR_FIELDS: &'static [&'static str] =
        &["name", "node_kind", "fql_kind", "language", "path"];
    const NUM_FIELDS: &'static [&'static str] = &["line", "usages"];
    const OPEN_FIELDS: bool = true;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(self.table.name_of(self.row)),
            "node_kind" => Some(self.table.node_kind_of(self.row)),
            "fql_kind" => {
                let s = self.table.fql_kind_of(self.row);
                if s.is_empty() { None } else { Some(s) }
            }
            "language" => {
                let s = self.table.language_of(self.row);
                if s.is_empty() { None } else { Some(s) }
            }
            "path" => self.table.path_of(self.row).to_str(),
            // Dynamic enrichment fields — resolve via the intern pool (zero alloc).
            other => self.table.strings.field_str(&self.row.fields, other),
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.row.line).unwrap_or(i64::MAX)),
            "usages" => Some(i64::from(self.row.usages_count)),
            // Dynamic enrichment fields — resolve string then parse.
            _ => self
                .table
                .strings
                .field_str(&self.row.fields, field)?
                .parse()
                .ok(),
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(self.table.path_of(self.row))
    }
}

impl ClauseTarget for crate::result::FileEntry {
    const ROW: &'static str = "a file row";
    const STR_FIELDS: &'static [&'static str] = &["path", "extension", "name", "has_error"];
    const NUM_FIELDS: &'static [&'static str] =
        &["size", "depth", "count", "error_count", "parse_coverage"];
    // A file row is exactly these fields: there is no enrichment map behind it,
    // so any other name can be refused here rather than matching nothing.
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "path" => self.path.to_str(),
            "extension" => Some(&self.extension),
            // The bare file name — `FIND files WHERE name = 'Kconfig'` is the
            // idiomatic first guess (mirrors `FIND symbols WHERE name`).
            "name" => self.path.file_name().and_then(|n| n.to_str()),
            // `None` when the query never asked for error stats, so an
            // unpopulated entry matches neither `= 'true'` nor `= 'false'`
            // rather than silently claiming the file is clean.
            "has_error" => self
                .error_count
                .map(|n| if n > 0 { "true" } else { "false" }),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "size" => Some(i64::try_from(self.size).unwrap_or(i64::MAX)),
            "depth" => self.depth.map(|d| i64::try_from(d).unwrap_or(i64::MAX)),
            "count" => self.count.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            "error_count" => self.error_count.map(i64::from),
            "parse_coverage" => self.parse_coverage.map(i64::from),
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

impl ClauseTarget for crate::result::DiffFileEntry {
    const ROW: &'static str = "a diff row";
    const STR_FIELDS: &'static [&'static str] = &["path", "name", "status"];
    const NUM_FIELDS: &'static [&'static str] = &["added", "removed", "changed"];
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "path" => self.path.to_str(),
            "name" => self.path.file_name().and_then(|n| n.to_str()),
            "status" => Some(&self.status),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "added" => Some(i64::try_from(self.added).unwrap_or(i64::MAX)),
            "removed" => Some(i64::try_from(self.removed).unwrap_or(i64::MAX)),
            "changed" => {
                Some(i64::try_from(self.added.saturating_add(self.removed)).unwrap_or(i64::MAX))
            }
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

impl ClauseTarget for crate::result::OutlineEntry {
    const ROW: &'static str = "an outline row";
    const STR_FIELDS: &'static [&'static str] = &["name", "fql_kind", "path"];
    const NUM_FIELDS: &'static [&'static str] = &["line", "depth"];
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(&self.name),
            "fql_kind" => Some(&self.fql_kind),
            "path" => self.path.to_str(),
            _ => None,
        }
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "line" => Some(i64::try_from(self.line).unwrap_or(i64::MAX)),
            "depth" => Some(i64::try_from(self.depth).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

impl ClauseTarget for crate::result::MemberEntry {
    const ROW: &'static str = "a members row";
    const STR_FIELDS: &'static [&'static str] =
        &["fql_kind", "type", "text", "declaration", "name"];
    const NUM_FIELDS: &'static [&'static str] = &["line"];
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            // `type` is the other documented name for this column on a members
            // row; `kind` reaches it as an alias of `fql_kind`.
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
    const ROW: &'static str = "a source-line row";
    const STR_FIELDS: &'static [&'static str] = &["text", "marker", "node_id", "rev"];
    const NUM_FIELDS: &'static [&'static str] = &["line"];
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "text" => Some(&self.text),
            "marker" => self.marker.as_deref(),
            "node_id" => self.node_id.as_deref(),
            "rev" => self.rev.as_deref(),
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
    const ROW: &'static str = "a callees row";
    const STR_FIELDS: &'static [&'static str] = &["name", "path"];
    // Every call in one answer sits inside the single function that was
    // resolved, so every row reports that function's file and a row filter on
    // `path` can only keep all of them or none. What the agent meant is which
    // `f` — so it goes to the lookup as well.
    const LOOKUP_FIELDS: &'static [&'static str] = &["path"];
    const NUM_FIELDS: &'static [&'static str] = &["line"];
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(&self.name),
            "path" => self.path.as_deref().and_then(|p| p.to_str()),
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

impl ClauseTarget for crate::result::CommitRow {
    const ROW: &'static str = "a commit row";
    // The two printed columns, and the whole row. A commit carries no path, no
    // line and no kind, so every other name is refused here rather than
    // resolving to nothing on every row.
    const STR_FIELDS: &'static [&'static str] = &["hash", "subject"];
    const NUM_FIELDS: &'static [&'static str] = &[];
    const OPEN_FIELDS: bool = false;

    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "hash" => Some(&self.0.name),
            "subject" => self.0.fields.get("subject").map(String::as_str),
            _ => None,
        }
    }

    fn field_num(&self, _field: &str) -> Option<i64> {
        None
    }

    fn path(&self) -> Option<&Path> {
        None
    }

    fn set_count(&mut self, count: usize) {
        self.0.count = Some(count);
    }
}
