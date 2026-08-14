//! Handle and metadata stamping for `SHOW` result rows.
//!
//! The stampers do not share a schedule, and the difference is the point.
//! `stamp_path_handles` runs after LIMIT, so only the rows an agent actually
//! sees cost anything to address. `stamp_error_counts` has to run *before*
//! filtering, because the clauses filter on the very fields it writes — so it
//! is gated instead on whether the query named those fields at all, which is
//! what keeps a plain `FIND files` from paying for an index scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{ir::Clauses, result::FileEntry, storage::StorageEngine, workspace::Workspace};

/// Give every `SHOW members` row its handle and its rev.
///
/// Members are read straight off the tree-sitter AST, which knows nothing about
/// ordinals — so a member row used to be **read-only**: an agent that wanted to
/// edit a field had to go back and FIND it by name first. Mapping each member's
/// line to the indexed node restores the rule every other row obeys: if you can
/// see it, you can address it, and the rev you need is right there.
pub(super) fn stamp_member_handles(
    engine: &dyn StorageEngine,
    workspace: &Workspace,
    path: &std::path::Path,
    json: &mut serde_json::Value,
) {
    let rel = workspace.relative(path).display().to_string();
    let root = workspace.root().to_path_buf();
    let Some(members) = json.get_mut("members").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for member in members.iter_mut() {
        // Match by byte containment, not by line: the indexed field node starts
        // at the attribute/doc line above the declaration, so their lines differ.
        let Some(byte) = member.get("byte_start").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Ok(byte) = usize::try_from(byte) else {
            continue;
        };
        let Some(node_id) = engine.find_node_id_at_byte(&rel, byte) else {
            continue;
        };
        let rev = engine
            .find_node(&node_id, &root)
            .ok()
            .flatten()
            .map(|n| n.rev);
        if let Some(obj) = member.as_object_mut() {
            let _ = obj.insert("node_id".to_string(), serde_json::json!(node_id));
            if let Some(rev) = rev {
                let _ = obj.insert("rev".to_string(), serde_json::json!(rev));
            }
        }
    }
}

/// Give every listed path its handle and its rev, so a `FIND files` row is
/// actionable as it stands — `DELETE NODE '<node_id>' IF REV '<rev>'` in one
/// round trip instead of a re-read. (Handing out the handle but not the rev
/// would be worse than handing out neither: the agent would try the delete,
/// hit the mandatory-IF-REV error, and have to come back for the rev anyway.)
///
/// Runs after LIMIT: a file rev costs a read, so only the rows the agent
/// actually sees are paid for. A directory rev is a membership XOR over the
/// paths underneath it — no file is read, and it deliberately does not move
/// when file content changes (per-file revs cover that).
pub(super) fn stamp_path_handles(workspace: &Workspace, results: &mut [serde_json::Value]) {
    let root = workspace.root();
    let mut dir_revs: Option<HashMap<PathBuf, u64>> = None;
    for row in results.iter_mut() {
        let Some(path) = row.get("path").and_then(|p| p.as_str()).map(str::to_owned) else {
            continue;
        };
        let rel = path.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        let abs = root.join(rel);
        let node_id = crate::node_id::path_handle(rel);

        let rev = if path.ends_with('/') || abs.is_dir() {
            // One walk covers every directory row: fold each file's path
            // fingerprint into all of its ancestor directories, then a row's
            // rev is a map lookup. Folding per row instead re-scans the whole
            // worktree per directory — O(dirs × files), minutes of CPU on a
            // 95,000-file corpus. The XOR is order-free, so the revs are
            // identical either way.
            let revs = dir_revs.get_or_insert_with(|| {
                let mut map: HashMap<PathBuf, u64> = HashMap::new();
                for file in workspace.files() {
                    if crate::result::FileEntry::is_runtime_artifact(&file) {
                        continue;
                    }
                    let rel_file = file.strip_prefix(root).unwrap_or(&file);
                    let folded = crate::node_id::fold_path_rev(0, &rel_file.to_string_lossy());
                    for dir in rel_file.ancestors().skip(1) {
                        if dir.as_os_str().is_empty() {
                            break;
                        }
                        *map.entry(dir.to_path_buf()).or_default() ^= folded;
                    }
                }
                map
            });
            let xor = revs.get(Path::new(rel)).copied().unwrap_or(0);
            crate::node_id::format_rev_exact(xor)
        } else {
            crate::node_id::file_rev(&abs)
        };
        if let Some(obj) = row.as_object_mut() {
            let _ = obj.insert("node_id".to_owned(), serde_json::Value::String(node_id));
            if !rev.is_empty() {
                let _ = obj.insert("rev".to_owned(), serde_json::Value::String(rev));
            }
        }
    }
}

/// Populate [`FileEntry::error_count`] from the index — but only when the query
/// actually asks about it.
///
/// Deriving the counts costs one indexed `fql_kind = 'error'` scan, so a plain
/// `FIND files` must not pay for it.  Entries left at `None` mean *not asked
/// for*, never *no errors*.
pub(super) fn stamp_error_counts(
    engine: &dyn StorageEngine,
    root: &std::path::Path,
    clauses: &Clauses,
    entries: &mut [FileEntry],
) -> Result<()> {
    let want_errors = references_field(clauses, &["has_error", "error_count"]);
    let want_coverage = references_field(clauses, &["parse_coverage"]);
    if !want_errors && !want_coverage {
        return Ok(());
    }

    let probe = Clauses {
        where_predicates: vec![crate::ir::Predicate {
            field: "fql_kind".to_string(),
            op: crate::ir::CompareOp::Eq,
            value: crate::ir::PredicateValue::String(crate::ast::lang::FQL_ERROR.to_string()),
        }],
        ..Clauses::default()
    };
    let rows = engine.find_symbols(&probe, root)?;

    if want_errors {
        // Only `root` regions count: the file did not parse as its declared
        // language at all.  A `nested` region means an indexed symbol still owns
        // the span — counting those would fire on every macro-heavy C file.
        let mut roots: std::collections::HashMap<PathBuf, u32> = std::collections::HashMap::new();
        for m in &rows {
            if m.fields.get("error_scope").map(String::as_str) != Some("root") {
                continue;
            }
            if let Some(path) = m.path.clone() {
                *roots.entry(path).or_default() += 1;
            }
        }
        for entry in &mut *entries {
            entry.error_count = Some(roots.get(&entry.path).copied().unwrap_or(0));
        }
    }

    if want_coverage {
        // Magnitude, not position: EVERY unparsed byte counts here, whatever its
        // scope.  Outermost ERRORs only are emitted, so the spans never overlap
        // and a plain sum is exact.
        let mut unparsed: std::collections::HashMap<PathBuf, u64> =
            std::collections::HashMap::new();
        for m in &rows {
            let bytes: u64 = m
                .fields
                .get("error_bytes")
                .and_then(|b| b.parse().ok())
                .unwrap_or(0);
            if let Some(path) = m.path.clone() {
                *unparsed.entry(path).or_default() += bytes;
            }
        }
        for entry in entries {
            let bad = unparsed.get(&entry.path).copied().unwrap_or(0);
            let pct = if entry.size == 0 {
                100
            } else {
                let covered = entry.size.saturating_sub(bad);
                u8::try_from(covered.saturating_mul(100) / entry.size).unwrap_or(100)
            };
            entry.parse_coverage = Some(pct);
        }
    }

    Ok(())
}

/// `true` when any clause names one of `names`.
fn references_field(clauses: &Clauses, names: &[&str]) -> bool {
    let hit = |field: &str| names.contains(&field);
    clauses
        .where_predicates
        .iter()
        .chain(clauses.having_predicates.iter())
        .any(|p| hit(&p.field))
        || clauses.order_by.as_ref().is_some_and(|o| hit(&o.field))
        || matches!(&clauses.group_by, Some(crate::ir::GroupBy::Field(f)) if hit(f))
}
