//! `SHOW outline` rendering for [`ColumnarStorage`]: file/glob and node-subtree forms.

use crate::storage::columnar::columnar_storage::ColumnarStorage;
use crate::storage::columnar::segment_reader::SegmentReader;
use crate::workspace::Workspace;

impl ColumnarStorage {
    pub(super) fn show_outline_for_file_impl(
        &self,
        workspace: &Workspace,
        file: &str,
        all: bool,
    ) -> serde_json::Value {
        let root = workspace.root();

        // Subtree form: `file` is actually a node_id → outline that node and its
        // descendants. File / glob form otherwise.
        let results = if let Some((hex, ordinal)) = parse_outline_node_target(file) {
            self.outline_subtree(workspace, root, file, &hex, ordinal, all)
        } else {
            self.outline_glob(workspace, root, file, all)
        };

        serde_json::json!({
            "op":      "show_outline",
            "file":    file,
            "results": results,
        })
    }

    /// Subtree outline: `file` is a node_id. Outline that node and its
    /// descendants, preferring the committed segment (authoritative nav) and
    /// falling back to the dirty overlay for a brand-new file.
    fn outline_subtree(
        &self,
        workspace: &Workspace,
        root: &std::path::Path,
        file: &str,
        hex: &str,
        ordinal: u32,
        all: bool,
    ) -> Vec<serde_json::Value> {
        let mut results: Vec<serde_json::Value> = Vec::new();
        if let Some(seg_idx) = self.overlay.seg_idx_for_node_id_prefix(hex) {
            let seg_idx = seg_idx as usize;
            if let (Some(seg), Some(seg_meta)) = (
                self.segments.get(seg_idx),
                self.overlay.segments().get(seg_idx),
            ) {
                let abs_path = root.join(&seg_meta.source_path);
                let rel_path = workspace.relative(&abs_path).display().to_string();
                let node_id_for = |ord: u32| seg_meta.node_id(ord);
                push_outline_tree(
                    seg,
                    &rel_path,
                    &node_id_for,
                    all,
                    Some(ordinal),
                    &mut results,
                );
                return results;
            }
        }
        for ds in &self.dirty.added {
            let src = ds.source_path.to_string_lossy().into_owned();
            if crate::node_id::make_node_id(&src, ordinal) != file {
                continue;
            }
            let abs_path = root.join(&ds.source_path);
            let rel_path = workspace.relative(&abs_path).display().to_string();
            let node_id_for = |ord: u32| crate::node_id::make_node_id(&src, ord);
            push_outline_tree(
                ds.reader.as_ref(),
                &rel_path,
                &node_id_for,
                all,
                Some(ordinal),
                &mut results,
            );
            break;
        }
        results
    }

    /// File / glob outline: committed segments are authoritative and matched
    /// first; the dirty overlay is consulted only for paths with no committed
    /// segment yet, or whose committed segment is stale after a session edit
    /// (BUG-013 — the read-side trigger for BUG-012).
    fn outline_glob(
        &self,
        workspace: &Workspace,
        root: &std::path::Path,
        file: &str,
        all: bool,
    ) -> Vec<serde_json::Value> {
        let mut results: Vec<serde_json::Value> = Vec::new();
        let mut committed_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (seg_idx, seg_meta) in self.overlay.segments().iter().enumerate() {
            if !crate::ast::query::glob_matches(&seg_meta.source_path, file) {
                continue;
            }
            // A file edited this session is authoritative in the dirty overlay;
            // its committed segment is stale (deleted nodes still listed, old line
            // numbers, pre-edit node_ids). Skip it here and let the dirty overlay
            // loop below render the file's current structure.
            if self
                .dirty
                .added
                .iter()
                .any(|ds| ds.source_path == seg_meta.source_path)
            {
                continue;
            }
            let seg = &self.segments[seg_idx];
            let abs_path = root.join(&seg_meta.source_path);
            let rel_path = workspace.relative(&abs_path).display().to_string();
            let _ = committed_paths.insert(seg_meta.source_path.to_string_lossy().into_owned());
            let node_id_for = |ord: u32| seg_meta.node_id(ord);
            push_outline_tree(seg, &rel_path, &node_id_for, all, None, &mut results);
        }
        for ds in &self.dirty.added {
            if committed_paths.contains(&ds.source_path.to_string_lossy().into_owned()) {
                continue;
            }
            if !crate::ast::query::glob_matches(&ds.source_path, file) {
                continue;
            }
            let src = ds.source_path.to_string_lossy().into_owned();
            let abs_path = root.join(&ds.source_path);
            let rel_path = workspace.relative(&abs_path).display().to_string();
            let node_id_for = |ord: u32| crate::node_id::make_node_id(&src, ord);
            push_outline_tree(
                ds.reader.as_ref(),
                &rel_path,
                &node_id_for,
                all,
                None,
                &mut results,
            );
        }
        results
    }
}
/// Structural declaration kinds shown in a default (non-`ALL`) outline.
fn outline_is_structural(kind: &str) -> bool {
    matches!(
        kind,
        // Code declarations.
        "function"
            | "class"
            | "struct"
            | "enum"
            | "interface"
            | "trait"
            | "union"
            | "namespace"
            | "module"
            | "type_alias"
            | "macro"
            // Data-language containers (JSON/YAML/TOML/…) — so a plain
            // `SHOW outline` on a data file shows its structure rather than
            // nothing (BUG-015). Leaf nodes (pair/scalar) stay out; they are
            // visible only with the `ALL` flag.
            | "object"
            | "array"
    )
}

/// Recognize a `SHOW outline OF '<target>'` argument that is a node_id rather
/// than a file path. Returns `(hex_prefix, ordinal)` on success.
fn parse_outline_node_target(s: &str) -> Option<(String, u32)> {
    // node_ids look like `n<hex>.<ordinal>` and never contain a path separator.
    if s.contains('/') {
        return None;
    }
    let rest = s.strip_prefix('n')?;
    let (hex, ord) = rest.split_once('.')?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let ordinal: u32 = ord.parse().ok()?;
    Some((hex.to_string(), ordinal))
}

/// Walk a segment's nodes as a tree (pre-order DFS) using the `parent_ordinal`
/// nav column and append outline entries to `out`.
///
/// `all` includes every node; otherwise only structural declarations are
/// emitted, with `depth` counting structural ancestors so the tree stays
/// compact. `root_ordinal` scopes the walk to a single node's subtree; `None`
/// walks every top-level node in the file.
fn push_outline_tree(
    reader: &SegmentReader,
    rel_path: &str,
    node_id_for: &dyn Fn(u32) -> String,
    all: bool,
    root_ordinal: Option<u32>,
    out: &mut Vec<serde_json::Value>,
) {
    use std::collections::{HashMap, HashSet};

    let n = reader.row_count;
    // Per-row facts: `ord` is None for analysis-only rows (no node handle).
    let ords: Vec<Option<u32>> = (0..n).map(|r| reader.ordinal_of(r)).collect();
    let parents: Vec<u32> = (0..n).map(|r| reader.parent_ordinal_of(r)).collect();
    let lines: Vec<usize> = (0..n).map(|r| reader.line_of(r) as usize).collect();

    // Ordinals that exist in this file — candidate parents.
    let present: HashSet<u32> = ords.iter().copied().flatten().collect();
    // parent_ordinal → child row indices, ordered by source line.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in 0..n {
        children.entry(parents[r as usize]).or_default().push(r);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|&r| lines[r as usize]);
    }

    // (row index, emit depth) stack; push children reversed so siblings pop in line order.
    let mut stack: Vec<(u32, usize)> = Vec::new();
    if let Some(target) = root_ordinal {
        if let Some(r) = (0..n).find(|&r| ords[r as usize] == Some(target)) {
            stack.push((r, 0));
        }
    } else {
        let mut roots: Vec<u32> = (0..n)
            .filter(|&r| {
                let p = parents[r as usize];
                p == u32::MAX || !present.contains(&p)
            })
            .collect();
        roots.sort_by_key(|&r| lines[r as usize]);
        for &r in roots.iter().rev() {
            stack.push((r, 0));
        }
    }

    while let Some((r, depth)) = stack.pop() {
        let kind = reader.fql_kind_of(r);
        let emit = all || outline_is_structural(kind);
        let child_depth = if emit {
            out.push(serde_json::json!({
                "name": reader.name_of(r),
                "fql_kind": if kind.is_empty() { "unknown" } else { kind },
                "path": rel_path,
                "line": lines[r as usize],
                "node_id": ords[r as usize].map(node_id_for),
                "depth": depth,
            }));
            depth + 1
        } else {
            depth
        };
        if let Some(o) = ords[r as usize]
            && let Some(kids) = children.get(&o)
        {
            for &cr in kids.iter().rev() {
                stack.push((cr, child_depth));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::outline_is_structural;

    #[test]
    fn structural_includes_code_declarations() {
        assert!(outline_is_structural("function"));
        assert!(outline_is_structural("struct"));
        assert!(outline_is_structural("class"));
    }

    #[test]
    fn structural_includes_data_containers() {
        // BUG-015: JSON/YAML container nodes must count as structural so a plain
        // `SHOW outline` on a data file isn't empty (the emit gate in
        // push_outline_tree is `all || outline_is_structural(kind)`).
        assert!(outline_is_structural("object"));
        assert!(outline_is_structural("array"));
    }

    #[test]
    fn structural_excludes_leaves_and_statements() {
        // Leaf/detail nodes stay out of the plain outline (visible only with ALL).
        assert!(!outline_is_structural("pair"));
        assert!(!outline_is_structural("number"));
        assert!(!outline_is_structural("if"));
        assert!(!outline_is_structural(""));
    }
}
