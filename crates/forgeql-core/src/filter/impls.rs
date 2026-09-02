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
            // The empty kind is a value this row carries, not a missing one —
            // see `SymbolMatch`'s arm and `segment_reader::materialize_rows`.
            "fql_kind" => Some(self.table.fql_kind_of(self.row)),
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
            // The one rule, read here too. This view never becomes a
            // `SymbolMatch`: the in-memory backend's symbol lookup — what
            // serves `SHOW body` / `signature` / `context` / `outline` /
            // `members` — filters its candidates straight through this
            // `field_num`, so a raw column here answers a `usages` predicate
            // for a row `FIND symbols` has stopped answering it for. The
            // FIND path never reaches this arm (its prefilter splits
            // `usages` predicates out and applies them to the built row),
            // which is exactly why the divergence was invisible.
            "usages" => (!crate::result::usages_is_absent_on(
                Some(self.table.fql_kind_of(self.row)),
                self.table.strings.field_str(&self.row.fields, "scope"),
            ))
            .then(|| i64::from(self.row.usages_count)),
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

/// A segment row read in place, before any `SymbolMatch` is built.
///
/// Every arm here must answer exactly what the materialised row would answer,
/// because the caller filters with this view and then materialises only the
/// survivors — a difference is a missing result, not a slow one. The agreement
/// is kept by `SegmentReader::row_field`, which is also what decides whether a
/// predicate may be filtered early at all; a field it declines is not dropped,
/// it is left to the filter that runs on the built rows.
impl ClauseTarget for crate::storage::columnar::segment_reader::SegRowRef<'_> {
    const ROW: &'static str = "a symbol row";
    const STR_FIELDS: &'static [&'static str] = &["name", "fql_kind", "language", "path"];
    const NUM_FIELDS: &'static [&'static str] = &["line"];
    // Enrichment columns: which names a segment carries depends on the
    // registered language plugins and on what that segment stored.
    const OPEN_FIELDS: bool = true;

    fn field_str(&self, field: &str) -> Option<&str> {
        self.str_value(field)
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        self.num_value(field)
    }

    fn path(&self) -> Option<&Path> {
        self.source_path
    }
}

/// A segment row carried without building it, with its ranked fields cached.
///
/// It answers every field the reader above answers, because it asks that
/// reader for everything it did not resolve when it was made — and what it did
/// resolve, it resolved through the same reader. The two field lists are taken
/// from that impl rather than restated, so a field admitted to one is admitted
/// to the other by construction.
impl ClauseTarget for crate::storage::columnar::segment_reader::RowView<'_> {
    const ROW: &'static str =
        <crate::storage::columnar::segment_reader::SegRowRef<'static> as ClauseTarget>::ROW;
    const STR_FIELDS: &'static [&'static str] =
        <crate::storage::columnar::segment_reader::SegRowRef<'static> as ClauseTarget>::STR_FIELDS;
    const NUM_FIELDS: &'static [&'static str] =
        <crate::storage::columnar::segment_reader::SegRowRef<'static> as ClauseTarget>::NUM_FIELDS;
    const OPEN_FIELDS: bool =
        <crate::storage::columnar::segment_reader::SegRowRef<'static> as ClauseTarget>::OPEN_FIELDS;

    fn field_str(&self, field: &str) -> Option<&str> {
        self.str_value(field)
    }

    fn field_num(&self, field: &str) -> Option<i64> {
        self.num_value(field)
    }

    fn path(&self) -> Option<&Path> {
        self.source_path()
    }
}

/// An occurrence site read as the row `FIND usages` would build from it.
///
/// The row is a symbol row with almost every field empty — the name queried,
/// the file, the line, and a `role` where the backend tags one — so the view
/// mirrors it arm for arm rather than reaching for anything. The field lists
/// are taken from the row's own impl rather than restated, because the row is
/// what a clause is validated against before either of them sees it.
///
/// A field this view answers `None` for is a field the row answers `None` for:
/// an occurrence row's open map holds `role` and nothing else, so every other
/// unlisted name misses on both. That is what makes filtering and ordering the
/// sites before they are built decide what filtering and ordering the rows
/// would have decided.
impl ClauseTarget for crate::storage::SiteView<'_> {
    const ROW: &'static str = <crate::result::SymbolMatch as ClauseTarget>::ROW;
    const STR_FIELDS: &'static [&'static str] =
        <crate::result::SymbolMatch as ClauseTarget>::STR_FIELDS;
    const NUM_FIELDS: &'static [&'static str] =
        <crate::result::SymbolMatch as ClauseTarget>::NUM_FIELDS;
    const OPEN_FIELDS: bool = <crate::result::SymbolMatch as ClauseTarget>::OPEN_FIELDS;

    #[expect(
        clippy::match_same_arms,
        reason = "the arms are the mirror: each names a field the row answers \
                  and says what it answers there, so folding the `None` ones \
                  into the wildcard would hide the agreement this rests on"
    )]
    fn field_str(&self, field: &str) -> Option<&str> {
        match field {
            "name" => Some(self.name),
            // An occurrence row leaves these `None`; a site carries no more.
            // On `fql_kind` that `None` is load-bearing and means something
            // narrower than it used to: a row whose kind is EMPTY now reports
            // the empty string, because that is a value the engine publishes and
            // answers on. `None` here says this shape has no kind column at all,
            // which is why `WHERE fql_kind = …` keeps matching no site instead
            // of matching every one of them. `usage_row` writes the same `None`,
            // and `a_site_view_reads_every_field_as_the_row_it_builds` pins the
            // two together.
            "node_kind" | "fql_kind" | "language" | "node_id" => None,
            "path" => self.path.to_str(),
            // The open map, which holds this one key and no other.
            "role" => self.role,
            _ => None,
        }
    }

    #[expect(
        clippy::match_same_arms,
        reason = "as above: the arms are the mirror, not a lookup table"
    )]
    fn field_num(&self, field: &str) -> Option<i64> {
        match field {
            "usages" | "count" => None,
            "line" => Some(i64::try_from(self.line).unwrap_or(i64::MAX)),
            // The row parses its map value; so does this, on the same value.
            "role" => self.role.and_then(|role| role.parse().ok()),
            _ => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(self.path)
    }
}
