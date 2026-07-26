//! Ordinal assignment — what keeps a `node_id` pointing at the same construct.
//!
//! The ordinal is the addressable half of a handle, and it has to survive a
//! re-index: edit one function and every other handle in the file must still
//! resolve. `OrdinalRemapper` does that by matching each node of the new pass
//! against the previous pass's hints.
//!
//! The tombstones are the subtle half. When a removal frees an ordinal, its
//! hint is marked consumed before `assign()` runs, so a byte-identical
//! surviving sibling cannot adopt it — the survivor keeps its own ordinal and
//! the deleted handle resolves to nothing, rather than silently re-pointing at
//! the survivor.

#[derive(Clone)]
pub struct OrdinalHint {
    pub name: String,
    pub fql_kind: String,
    pub parent_ordinal: u32,
    pub guard_group_id: Option<String>,
    pub guard_branch: Option<String>,
    pub first_body_statement_fingerprint: Option<String>,
    pub content_hash: Option<String>,
    pub ordinal: u32,
}

/// Per-file removed **root** ordinals staged for the next reindex.
///
/// Marking these hints consumed before `assign()` runs stops a byte-identical
/// surviving sibling from adopting a just-deleted node's ordinal — the survivor
/// keeps its own ordinal, and the deleted handle then resolves to nothing
/// instead of silently re-pointing at the survivor. Keyed by worktree-relative
/// path; empty for every non-removal mutation.
pub type OrdinalTombstones = std::collections::BTreeMap<std::path::PathBuf, Vec<u32>>;
pub struct OrdinalRemapper {
    previous: Vec<OrdinalHint>,
    used: Vec<bool>,
    next_ordinal: u32,
}

pub(super) struct OrdinalMatchKey<'a> {
    pub(super) name: &'a str,
    pub(super) fql_kind: &'a str,
    pub(super) parent_ordinal: u32,
    pub(super) guard_group_id: Option<&'a str>,
    pub(super) guard_branch: Option<&'a str>,
    pub(super) first_body_statement_fingerprint: Option<&'a str>,
    pub(super) content_hash: Option<&'a str>,
}

impl OrdinalRemapper {
    /// The next ordinal this remapper would hand out.
    ///
    /// Read-only on purpose: only `from_previous` and `assign` may move the
    /// counter, and keeping the field private is what enforces that.
    #[must_use]
    pub(super) const fn next_ordinal(&self) -> u32 {
        self.next_ordinal
    }

    #[must_use]
    pub fn from_previous(previous: Vec<OrdinalHint>) -> Self {
        let next_ordinal = previous
            .iter()
            .map(|h| h.ordinal)
            .max()
            .map_or(0, |m| m.saturating_add(1));
        let used = vec![false; previous.len()];
        Self {
            previous,
            used,
            next_ordinal,
        }
    }

    /// Mark the hints for `ordinals` as already consumed, before any `assign()`.
    ///
    /// A removal verb passes the removed node's root ordinal(s). The matching
    /// hint is then invisible to `assign`, so a byte-identical surviving sibling
    /// can no longer win it on the min-ordinal tiebreak — it keeps its own
    /// ordinal, and the removed node's handle resolves to nothing. `next_ordinal`
    /// is unaffected (the hint stays in `previous`, still bounding the max), so
    /// the retired ordinal is never reissued.
    pub fn tombstone(&mut self, ordinals: &[u32]) {
        for (idx, hint) in self.previous.iter().enumerate() {
            if ordinals.contains(&hint.ordinal) {
                self.used[idx] = true;
            }
        }
    }

    fn primary_matches(
        hint: &OrdinalHint,
        name: &str,
        fql_kind: &str,
        parent_ordinal: u32,
    ) -> bool {
        hint.name == name && hint.fql_kind == fql_kind && hint.parent_ordinal == parent_ordinal
    }

    fn guard_matches(
        hint: &OrdinalHint,
        guard_group_id: Option<&str>,
        guard_branch: Option<&str>,
    ) -> bool {
        hint.guard_group_id.as_deref() == guard_group_id
            && hint.guard_branch.as_deref() == guard_branch
    }

    pub(super) fn assign(&mut self, key: &OrdinalMatchKey<'_>) -> u32 {
        let mut candidates: Vec<usize> = self
            .previous
            .iter()
            .enumerate()
            .filter(|(idx, hint)| {
                !self.used[*idx]
                    && Self::primary_matches(hint, key.name, key.fql_kind, key.parent_ordinal)
            })
            .map(|(idx, _)| idx)
            .collect();

        if candidates.len() > 1 {
            let guard_filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|idx| {
                    Self::guard_matches(&self.previous[*idx], key.guard_group_id, key.guard_branch)
                })
                .collect();
            if !guard_filtered.is_empty() {
                candidates = guard_filtered;
            }
        }

        if candidates.len() > 1 && key.first_body_statement_fingerprint.is_some() {
            let fp_filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|idx| {
                    self.previous[*idx]
                        .first_body_statement_fingerprint
                        .as_deref()
                        == key.first_body_statement_fingerprint
                })
                .collect();
            if !fp_filtered.is_empty() {
                candidates = fp_filtered;
            }
        }

        if candidates.len() > 1 && key.content_hash.is_some() {
            let hash_filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|idx| self.previous[*idx].content_hash.as_deref() == key.content_hash)
                .collect();
            if !hash_filtered.is_empty() {
                candidates = hash_filtered;
            }
        }

        if let Some(best_idx) = candidates
            .into_iter()
            .min_by_key(|idx| self.previous[*idx].ordinal)
        {
            self.used[best_idx] = true;
            let ordinal = self.previous[best_idx].ordinal;
            crate::debug_log!(
                "assign MATCH name={:?} kind={:?} parent_ord={} -> ord={} (reused)",
                key.name,
                key.fql_kind,
                key.parent_ordinal,
                ordinal
            );
            return ordinal;
        }

        let ordinal = self.next_ordinal;
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        if crate::debug_log::is_enabled() {
            // On a fresh allocation, surface any prior hints that matched on
            // name+kind but were rejected — their parent_ordinal reveals whether
            // the miss is a structural (flat vs nested) mismatch.
            let rejected_parent_ords: Vec<u32> = self
                .previous
                .iter()
                .filter(|h| h.name == key.name && h.fql_kind == key.fql_kind)
                .map(|h| h.parent_ordinal)
                .collect();
            crate::debug_log!(
                "assign NEW   name={:?} kind={:?} parent_ord={} -> ord={} (name+kind priors={}, their parent_ords={:?})",
                key.name,
                key.fql_kind,
                key.parent_ordinal,
                ordinal,
                rejected_parent_ords.len(),
                rejected_parent_ords
            );
        }
        ordinal
    }
}
/// Assign a node ordinal: reuse a prior one via the remapper when available
/// (keeps `node_id` handles stable across re-indexes), otherwise hand out the
/// next value from the per-file counter.
pub(super) fn assign_ordinal(
    remapper: Option<&mut OrdinalRemapper>,
    row_ordinal_counter: &mut u32,
    key: &OrdinalMatchKey<'_>,
) -> u32 {
    remapper.map_or_else(
        || {
            let next = *row_ordinal_counter;
            *row_ordinal_counter = row_ordinal_counter.saturating_add(1);
            next
        },
        |remapper| remapper.assign(key),
    )
}
