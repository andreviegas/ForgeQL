//! Symbol resolution logic for [`super::LegacyMemoryStorage`].
//!
//! Lifted from `engine.rs` — no algorithmic changes.
//!
//! Three public entry points are used by the [`StorageEngine`] trait
//! implementations in [`super`]:
//! - [`resolve_symbol`]   — general-purpose name→location lookup
//! - [`resolve_type_symbol`] — prefers type definitions with members
//! - [`resolve_body_symbol`] — follows `body_symbol` out-of-line redirects

use std::path::Path;

use anyhow::{Result, bail};

use crate::{
    ast::index::{IndexRow, SymbolTable},
    filter::eval_predicate,
    ir::Clauses,
};

use super::helpers::passes_glob_filter;

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

/// Split a qualified name like `CachedIndex::save` or `MyClass.method` into
/// `(owner, member)`.  Returns `None` for bare names without a separator.
///
/// Tries `::` first (Rust, C++), then `.` (Python, JS, Java).
/// This is language-agnostic — the separator is detected from the name itself.
fn split_qualified_name(name: &str) -> Option<(&str, &str)> {
    // Try `::` first (higher precedence — avoids false splits in `A::B.c`)
    if let Some(pos) = name.rfind("::") {
        let owner = &name[..pos];
        let member = &name[pos + 2..];
        if !owner.is_empty() && !member.is_empty() {
            return Some((owner, member));
        }
    }
    // Fall back to `.`
    if let Some(pos) = name.rfind('.') {
        let owner = &name[..pos];
        let member = &name[pos + 1..];
        if !owner.is_empty() && !member.is_empty() {
            return Some((owner, member));
        }
    }
    None
}

// -----------------------------------------------------------------------
// Public resolvers
// -----------------------------------------------------------------------

/// Resolve a symbol name to a single [`IndexRow`] using SHOW command clauses.
///
/// 1. Finds all definition rows matching `name` in the index.
/// 2. Filters by `IN`/`EXCLUDE` globs and `WHERE` predicates from `clauses`.
/// 3. If the surviving candidates span multiple languages, returns an error
///    asking the user to disambiguate with `WHERE language = '...'` or
///    `IN '*.ext'`.
/// 4. Returns the last matching row (preserving v1 last-write-wins semantics
///    within a single language).
#[allow(clippy::too_many_lines)]
pub(super) fn resolve_symbol<'a>(
    index: &'a SymbolTable,
    name: &str,
    clauses: &Clauses,
    root: &Path,
) -> Result<&'a IndexRow> {
    use crate::ast::index::RowRef;

    // Qualified name resolution: split on `::` or `.` separators.
    // If the name contains a separator, look up the member name and filter
    // by the `enclosing_type` enrichment field set by MemberEnricher.
    if let Some((owner, member)) = split_qualified_name(name) {
        let candidates = index.find_all_defs(member);
        let matched: Vec<&IndexRow> = candidates
            .into_iter()
            .filter(|row| {
                index
                    .strings
                    .field_str(&row.fields, "enclosing_type")
                    .is_some_and(|et| et == owner)
            })
            .collect();
        if !matched.is_empty() {
            #[allow(clippy::expect_used)]
            return Ok(matched.last().expect("matched is non-empty"));
        }
        // Fall through: the qualified name may be resolved via body_symbol
        // redirect (C++ out-of-line definitions) or as-is in the index.
    }

    let candidates = index.find_all_defs(name);
    if candidates.is_empty() {
        let suggestions = index.suggest_similar(name, 5);
        if suggestions.is_empty() {
            bail!("symbol '{name}' not found in index");
        }
        bail!(
            "symbol '{name}' not found in index. \
             Did you mean one of: {}? \
             Use FIND symbols WHERE name LIKE \
             '%{name}%' to search.",
            suggestions.join(", ")
        );
    }

    // Single candidate — fast path, skip filtering.
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }

    let filtered: Vec<&IndexRow> = candidates
        .into_iter()
        .filter(|row| {
            if !passes_glob_filter(index.path_of(row), clauses, root) {
                return false;
            }
            clauses
                .where_predicates
                .iter()
                .all(|p| eval_predicate(&RowRef { row, table: index }, p))
        })
        .collect();

    if filtered.is_empty() {
        use std::fmt::Write;
        let mut hint = format!(
            "symbol '{name}' exists in the index \
             but all candidates were eliminated by filters."
        );
        if let Some(ref glob) = clauses.in_glob {
            let _ = write!(hint, " IN '{glob}' excluded all matches.");
        }
        if let Some(ref glob) = clauses.exclude_glob {
            let _ = write!(hint, " EXCLUDE '{glob}' removed matches.");
        }
        if !clauses.where_predicates.is_empty() {
            hint.push_str(" WHERE predicates filtered all remaining candidates.");
        }
        let _ = write!(
            hint,
            " Try removing filters or use \
             FIND symbols WHERE name = '{name}' to see all occurrences."
        );
        bail!("{hint}");
    }

    // Prefer actual definitions (non-empty fql_kind) over reference-only
    // index rows such as scoped_identifier / qualified_identifier nodes
    // that happen to share the bare name.
    let defs: Vec<&IndexRow> = filtered
        .iter()
        .copied()
        .filter(|row| !index.fql_kind_of(row).is_empty())
        .collect();
    let best = if defs.is_empty() { &filtered } else { &defs };

    // Check cross-language ambiguity.
    let mut languages: Vec<&str> = best
        .iter()
        .filter_map(|r| {
            let lang = index.language_of(r);
            if lang.is_empty() { None } else { Some(lang) }
        })
        .collect();
    languages.sort_unstable();
    languages.dedup();

    if languages.len() > 1 {
        bail!(
            "symbol '{name}' exists in multiple languages: [{}]. \
             Use WHERE language = '...' or IN '*.ext' to disambiguate",
            languages.join(", ")
        );
    }

    // Last match — preserves v1 last-write-wins within a single language.
    // SAFETY: `best` is guaranteed non-empty by the bail above.
    #[allow(clippy::expect_used)]
    Ok(best.last().expect("filtered is non-empty"))
}

/// Like [`resolve_symbol`] but prefers type definitions (`struct`/`class`/`enum`)
/// that have members — used for `SHOW members OF` lookups.
///
/// When a type is heavily referenced as a pointer (`struct Foo *`) there may be
/// many reference-only index rows with the same name.  The generic
/// `resolve_symbol` returns the last one in insertion order, which may be a
/// reference rather than the definition.  This variant rescans all candidates
/// and picks the one whose `fql_kind` is a type kind and whose `member_count`
/// is > 0 when possible.
pub(super) fn resolve_type_symbol<'a>(
    index: &'a SymbolTable,
    name: &str,
    clauses: &Clauses,
    root: &Path,
) -> Result<&'a IndexRow> {
    let def = resolve_symbol(index, name, clauses, root)?;

    // Fast path: the resolved row already looks like a type definition with members.
    let fql_kind = index.fql_kind_of(def);
    let has_members = index
        .strings
        .field_str(&def.fields, "member_count")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        > 0;
    if (fql_kind == "struct" || fql_kind == "class" || fql_kind == "enum") && has_members {
        return Ok(def);
    }

    // Slow path: scan all candidates for a type definition with members.
    let best_type = index.find_all_defs(name).into_iter().rfind(|row| {
        let fk = index.fql_kind_of(row);
        (fk == "struct" || fk == "class" || fk == "enum")
            && index
                .strings
                .field_str(&row.fields, "member_count")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0)
                > 0
    });

    Ok(best_type.unwrap_or(def))
}

/// Like [`resolve_symbol`] but follows the `body_symbol` redirect.
///
/// If the resolved row carries a `body_symbol` field (set by the
/// `MemberEnricher` for out-of-line member function definitions), follow
/// the redirect to find the actual function body.
pub(super) fn resolve_body_symbol<'a>(
    index: &'a SymbolTable,
    name: &str,
    clauses: &Clauses,
    root: &Path,
) -> Result<&'a IndexRow> {
    let def = resolve_symbol(index, name, clauses, root)?;
    if let Some(target) = index.strings.field_str(&def.fields, "body_symbol")
        && let Some(redirected) = index.find_def(target)
    {
        return Ok(redirected);
    }
    Ok(def)
}