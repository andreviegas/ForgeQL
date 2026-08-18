//! Process-wide interner for the names a [`super::SegmentReader`] keeps.
//!
//! A segment spells its own enrichment column names and occurrence role names
//! in its header, and those names repeat across every segment of a workspace:
//! tens of thousands of readers name the same forty columns. One owned copy
//! per column per reader is what made the reader tables tens of megabytes,
//! while reading the name back out of the mapping instead makes every row
//! batch pay a mapping read for it. Interning is neither: each reader holds a
//! counted pointer into one shared copy of the name.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, RwLock};

/// Ceiling on the distinct names the pool retains.
///
/// The real universe is the enrichment field names the enrichers emit plus the
/// occurrence role names — a few dozen. The cap is here because these names
/// are read out of a segment header, which a corrupt or foreign file may spell
/// however it likes: past the cap `intern` still answers, with a private copy
/// instead of a shared one, so an odd corpus costs a copy per reader rather
/// than an entry the pool keeps for the life of the process.
const MAX_INTERNED: usize = 4096;

/// The shared name pool, created on first use.
fn pool() -> &'static RwLock<HashSet<Arc<str>>> {
    static POOL: OnceLock<RwLock<HashSet<Arc<str>>>> = OnceLock::new();
    POOL.get_or_init(|| RwLock::new(HashSet::new()))
}

/// The shared copy of `name`.
///
/// Returns a private copy — never an error and never a different string — when
/// the pool is full or a panic poisoned its lock, so a caller can always use
/// what it gets back.
pub(super) fn intern(name: &str) -> Arc<str> {
    if let Ok(pool) = pool().read() {
        if let Some(shared) = pool.get(name) {
            return Arc::clone(shared);
        }
    }

    let owned: Arc<str> = Arc::from(name);
    if let Ok(mut pool) = pool().write() {
        if let Some(shared) = pool.get(name) {
            // Another thread interned it between the read above and this
            // write: hand back the copy that is shared, not the private one.
            return Arc::clone(shared);
        }
        if pool.len() < MAX_INTERNED {
            let _ = pool.insert(Arc::clone(&owned));
        }
    }
    owned
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{MAX_INTERNED, intern};

    /// Both halves of the contract in one test on purpose: the pool is
    /// process-wide, so filling it in a separate test would decide by test
    /// order whether the sharing half still had room to pass.
    #[test]
    fn a_repeated_name_is_shared_and_one_past_the_cap_is_still_answered() {
        let first = intern("has_doc");
        let second = intern("has_doc");
        assert_eq!(&*first, "has_doc");
        assert!(
            Arc::ptr_eq(&first, &second),
            "a repeated name must be shared, not copied"
        );

        // Fill the pool past its ceiling, then ask for one more name: the
        // answer is a private copy, and it is still the name that was asked
        // for. A corrupt header must cost memory, never a wrong column name.
        for i in 0..MAX_INTERNED + 16 {
            let _ = intern(&format!("filler_name_{i}"));
        }
        let past_the_cap = intern("a_name_the_full_pool_has_never_seen");
        assert_eq!(&*past_the_cap, "a_name_the_full_pool_has_never_seen");
        assert!(
            !Arc::ptr_eq(
                &past_the_cap,
                &intern("a_name_the_full_pool_has_never_seen")
            ),
            "past the cap the name is answered from a private copy, not the pool"
        );
    }
}
