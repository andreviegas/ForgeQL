//! `usages` on a session that serves dirty rows.
//!
//! The `usages` a `FIND symbols` row carries is the workspace total of the
//! name's usage sites, read from the overlay's usage-count aggregate. That
//! aggregate was built once, over the segments the overlay was built from,
//! so on a session with dirty rows — an edit not yet committed, a chain
//! attach seeded from a master — it counts the master's sites: a file the
//! session replaced still contributes the sites it used to hold, and the
//! segment that replaced it contributes none. This is the correction. Per
//! name, the sites of every shadowed master segment are subtracted and the
//! sites of every dirty segment are added, both read straight off the
//! per-segment usage postings, whose value for a name is its site count.
//! Only names whose count actually changed are kept: a replaced file whose
//! sites for a name are the same as before nets to zero and costs nothing
//! at lookup.
//!
//! The correction is a function of the dirty overlay, and the dirty overlay
//! changes as the session edits. Rather than trusting every mutation site
//! to invalidate it — the one that forgets would serve a stale count and
//! nothing would say so — the built table records the dirty state it was
//! built from (which segments, which shadowed paths) and is rebuilt when a
//! lookup finds that state changed. Comparing the state costs one pass over
//! the dirty overlay per query, which is small; building costs one pass
//! over the postings of the changed files, once per change.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fst::Streamer as _;

use super::super::segment_reader::SegmentReader;
use super::ColumnarStorage;

/// The dirty state a [`UsageAdjust`] was built from: every dirty segment
/// by path and content, and every shadowed path.
#[derive(Debug, PartialEq, Eq)]
struct DirtyFingerprint {
    added: Vec<(PathBuf, String)>,
    removed: Vec<PathBuf>,
}

/// Net usage-site change per name against the overlay's aggregate, for one
/// dirty state.
#[derive(Debug)]
pub(super) struct UsageAdjust {
    fingerprint: DirtyFingerprint,
    /// Names whose count changed, with the signed change. Absent means zero.
    deltas: HashMap<String, i64>,
}

impl UsageAdjust {
    /// The workspace usage count of `name` on this session: the overlay's
    /// aggregate corrected by the dirty overlay's change.
    #[must_use]
    pub(super) fn corrected(&self, name: &str, aggregate: u64) -> usize {
        let base = i64::try_from(aggregate).unwrap_or(i64::MAX);
        let n = base.saturating_add(self.deltas.get(name).copied().unwrap_or(0));
        usize::try_from(n).unwrap_or(0)
    }
}

impl ColumnarStorage {
    /// The usage-count correction for the current dirty overlay, built on
    /// first use and rebuilt whenever the dirty overlay has changed since.
    /// Callers hold the returned table across one stamping pass; a
    /// concurrent edit is picked up by the next.
    pub(super) fn usage_adjust(&self) -> Arc<UsageAdjust> {
        let fingerprint = self.dirty_fingerprint();
        let mut slot = self
            .usage_adjust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = slot.as_ref()
            && cached.fingerprint == fingerprint
        {
            return Arc::clone(cached);
        }
        let built = Arc::new(self.build_usage_adjust(fingerprint));
        *slot = Some(Arc::clone(&built));
        built
    }

    fn dirty_fingerprint(&self) -> DirtyFingerprint {
        let added = self
            .dirty
            .added
            .iter()
            .map(|ds| (ds.source_path.clone(), ds.reader.content_id_hex()))
            .collect();
        let mut removed: Vec<PathBuf> = self.dirty.removed_paths.iter().cloned().collect();
        removed.sort_unstable();
        DirtyFingerprint { added, removed }
    }

    fn build_usage_adjust(&self, fingerprint: DirtyFingerprint) -> UsageAdjust {
        let mut deltas: HashMap<Vec<u8>, i64> = HashMap::new();
        let segs = self.overlay().segments();
        let readers = self.segments();
        for path in &fingerprint.removed {
            // The segment table is sorted by source path; a path can hold
            // more than one segment (commit/promote turbulence), and every
            // one of them contributed to the aggregate, so every one is
            // subtracted.
            let Ok(hit) = segs.binary_search_by(|s| s.source_path.as_path().cmp(path)) else {
                continue;
            };
            let mut lo = hit;
            while lo > 0 && segs[lo - 1].source_path == *path {
                lo -= 1;
            }
            let mut hi = hit + 1;
            while hi < segs.len() && segs[hi].source_path == *path {
                hi += 1;
            }
            for reader in &readers[lo..hi] {
                accumulate_usage_counts(reader, -1, &mut deltas);
            }
        }
        for ds in &self.dirty.added {
            accumulate_usage_counts(&ds.reader, 1, &mut deltas);
        }
        deltas.retain(|_, d| *d != 0);
        UsageAdjust {
            fingerprint,
            deltas: deltas
                .into_iter()
                .map(|(name, d)| (String::from_utf8_lossy(&name).into_owned(), d))
                .collect(),
        }
    }
}

/// Add `sign` × the site count of every name in `reader`'s usage postings
/// to `deltas`. The postings value packs the site count in its low 32 bits,
/// exactly as the overlay's aggregate reads it at build time.
fn accumulate_usage_counts(reader: &SegmentReader, sign: i64, deltas: &mut HashMap<Vec<u8>, i64>) {
    let Some(fst) = &reader.usages_fst else {
        return;
    };
    let mut stream = fst.stream();
    while let Some((name, encoded)) = stream.next() {
        let count = i64::from(u32::try_from(encoded & 0xFFFF_FFFF).unwrap_or(u32::MAX));
        *deltas.entry(name.to_vec()).or_default() += sign * count;
    }
}
