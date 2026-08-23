//! Process-wide cache of the committed, immutable half of an open index.
//!
//! Every session used to decode its own copy of a commit: the [`Overlay`], and
//! one [`SegmentReader`] per indexed file. Those readers are the expensive part
//! — each held decoded postings and a heap map of its string pool, and they are
//! private heap rather than shared mappings, so N sessions on one commit cost N
//! times the working set. Several concurrent sessions on one corpus is a
//! routine load, and it has taken the server out of memory. (A reader is a
//! smaller thing now: postings are decoded per lookup and the string pool is
//! searched in the mapping. Sharing it still matters — what remains is its
//! blob ranges and its FSTs, per file, per session.)
//!
//! Sharing them is sound by construction rather than by convention:
//!
//! * [`Overlay`] and [`SegmentReader`] expose no `&mut self` method and hold no
//!   interior mutability, so an opened one cannot change under a reader.
//! * Every file behind an entry is content-addressed — the overlay by commit
//!   SHA, each segment by its content ID — so one key always names one
//!   byte-for-byte content.
//! * A session's own edits never reach here. They live in its `DirtyOverlay`,
//!   which stays per-session and takes precedence at query time; committing
//!   produces a new commit SHA, which is a different key and a different file.
//!
//! Together those mean there is no invalidation problem to solve: an entry
//! cannot go stale, only unused. Even a `VACUUM` deleting a file under a live
//! entry leaves it correct, because anything rebuilt at that path is rebuilt
//! with identical content.
//!
//! So the cache holds nothing of its own: an entry lives exactly as long as
//! some session holds it, and there is no idle-entry window on top. There does
//! not need to be one. A session ends only when the TTL reaper takes it —
//! nothing tells the engine a client has gone — so the session TTL is already
//! how long a commit's decode survives, and a second timer starting where the
//! TTL decided the memory should go would only extend it. Note also what an
//! entry costs, in full: the decoded half is private heap, and the mapped half
//! is resident too, counting against the same cgroup limit. Clean file pages
//! can be dropped and re-read rather than swapped, which is not the same as
//! being free — that refault churn is itself a source of memory pressure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use anyhow::Result;

use super::overlay::Overlay;
use super::segment_reader::SegmentReader;

/// The committed, immutable half of an open index, shared by every session
/// reading the same commit.
pub struct SharedOpen {
    /// Decoded overlay for this commit.
    pub overlay: Arc<Overlay>,
    /// One reader per segment, in `overlay.segments()` order.
    pub segments: Vec<Arc<SegmentReader>>,
}

/// Sharing an entry requires two properties, and this pins only the first.
///
/// **Thread-safety** is by construction: the cache lives in a `static`, which
/// already demands it, but demands it silently. Naming it here means a future
/// field that is not `Send + Sync` fails on this line, with a reason, instead
/// of somewhere inside a generic instantiation.
///
/// **Immutability** is not implied by it and cannot be asserted this way — a
/// `Mutex`, `OnceLock` or atomic field is `Send + Sync` and would pass here
/// while destroying the property the sharing actually rests on. That one is a
/// reviewed invariant: neither type has a `&mut self` method or any interior
/// mutability today, and anything added that does makes this cache unsound
/// without breaking the build.
const _: fn() = || {
    const fn assert_shareable_across_sessions<T: Send + Sync>() {}
    assert_shareable_across_sessions::<Overlay>();
    assert_shareable_across_sessions::<SegmentReader>();
    assert_shareable_across_sessions::<SharedOpen>();
};

/// Identifies one shared entry: the overlay file, and the versioned segment
/// root its readers were opened from.
///
/// Both halves are load-bearing. A build context keeps its overlay directory
/// and its segment directory as separate settings, so two contexts can name the
/// same overlay file while resolving segments somewhere else entirely, and the
/// readers belong to the segment root rather than to the overlay.
pub type OpenKey = (PathBuf, PathBuf);

/// The result of decoding one entry, and whether later callers may be served it.
pub struct Opened<V> {
    /// The decoded value. Returned to this caller either way.
    pub value: V,
    /// Whether this decode may be shared with later callers.
    ///
    /// An incomplete decode must say `false`: a cache is not allowed to make a
    /// recoverable gap permanent, and sharing a partial decode would pin the
    /// missing rows for every later caller on that key where a fresh open might
    /// have recovered. The one producer in the tree — opening the committed
    /// state — fails outright rather than handing back a partial value, so in
    /// practice it always says `true`. The flag stays because that is a
    /// property of today's producer, not of this cache, and any less strict one
    /// still has to answer honestly.
    pub shareable: bool,
}

/// Returns the shared committed state for `key`, calling `open` only when no
/// session already holds it.
pub fn shared_open<F>(key: &OpenKey, open: F) -> Result<Arc<SharedOpen>>
where
    F: FnOnce() -> Result<Opened<SharedOpen>>,
{
    cache().get_or_open(key, open)
}

/// Answers whether a commit's committed half is open right now, without
/// opening it.
///
/// This is for callers that are only *asking*. Routing such a caller through
/// [`shared_open`] would have it decode the whole committed half — overlay
/// tables and one reader per indexed file — to produce a yes or no, and then
/// drop it; the background warmer runs one thread per source, so doing that at
/// start-up builds one full working set per source at once, which is the shape
/// this cache exists to prevent.
///
/// Route through [`shared_open`] where the decode will be *used*; peek where
/// you are only asking.
///
/// The answer is a fact about the instant it was taken, and an entry can die
/// the moment after. That is safe for a caller asking whether work can be
/// skipped *because the files are there* — the warmer's question — since a
/// live entry proves that and its later death does not unprove it. It is not
/// safe as a basis for concluding you need not open the thing yourself: for
/// that, call [`shared_open`] and hold what it returns.
#[must_use]
pub fn peek(key: &OpenKey) -> Option<Arc<SharedOpen>> {
    cache().lookup(key)
}

fn cache() -> &'static OpenCache<SharedOpen> {
    static CACHE: OnceLock<OpenCache<SharedOpen>> = OnceLock::new();
    CACHE.get_or_init(OpenCache::new)
}

/// A keyed cache of immutable values, each kept alive by its users.
///
/// Generic over the value so the eviction and single-flight logic can be
/// exercised directly by tests without decoding a real corpus.
struct OpenCache<V> {
    inner: Mutex<Inner<V>>,
}

struct Inner<V> {
    /// Entries a caller still holds, and the whole of the cache's retention
    /// policy. `Weak` on purpose: the cache never keeps an entry alive by
    /// itself, so an entry costs nothing the moment the last session using it
    /// is gone.
    ///
    /// There is deliberately no idle-entry window on top of this. A session
    /// only ends when the TTL reaper takes it — nothing tells the engine a
    /// client went away — so the session TTL already *is* how long a commit's
    /// decode is held. A second timer starting where the TTL decided the memory
    /// should go would extend retention past the point that was already
    /// decided, and an entry is a whole decoded working set. If commits turn
    /// out to be dropped too eagerly, the number to argue about is the session
    /// TTL, not a cache-local one.
    live: HashMap<OpenKey, Weak<V>>,
    /// Per-key build gates, so that two callers arriving together on a cold key
    /// decode it once rather than twice.
    gates: HashMap<OpenKey, Arc<Mutex<()>>>,
}

impl<V> OpenCache<V> {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                live: HashMap::new(),
                gates: HashMap::new(),
            }),
        }
    }

    /// A poisoned cache is still a usable cache: every value in it is immutable
    /// and content-addressed, so a panic elsewhere cannot have left a wrong one
    /// behind. Refusing to serve would turn an unrelated panic into an outage.
    fn lock(&self) -> MutexGuard<'_, Inner<V>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get_or_open<F>(&self, key: &OpenKey, open: F) -> Result<Arc<V>>
    where
        F: FnOnce() -> Result<Opened<V>>,
    {
        if let Some(hit) = self.lookup(key) {
            return Ok(hit);
        }

        // Decode outside the cache lock: it takes seconds on a large corpus and
        // holding the lock across it would stall callers on unrelated commits.
        // The per-key gate is what stops two callers decoding the same one.
        let gate = {
            let mut inner = self.lock();
            Arc::clone(inner.gates.entry(key.clone()).or_default())
        };
        let _building = gate.lock().unwrap_or_else(PoisonError::into_inner);

        // A peer may have finished while we waited on the gate. Every exit from
        // here on releases the gate, and releases only the one held here: only
        // the caller that reached `open()` used to release at all, which leaked
        // an entry per raced key.
        if let Some(hit) = self.lookup(key) {
            self.release_gate(key, &gate);
            return Ok(hit);
        }

        match open() {
            Ok(opened) => {
                let shared = Arc::new(opened.value);
                if opened.shareable {
                    self.publish(key, &shared, &gate);
                } else {
                    // Serve this caller, share nothing: the next one opens the
                    // files again and may get a complete decode.
                    self.release_gate(key, &gate);
                }
                Ok(shared)
            }
            Err(e) => {
                self.release_gate(key, &gate);
                Err(e)
            }
        }
    }

    /// Releases a build gate this caller holds. See [`Inner::release_gate`] for
    /// why the removal is identity-checked rather than by key.
    fn release_gate(&self, key: &OpenKey, gate: &Arc<Mutex<()>>) {
        self.lock().release_gate(key, gate);
    }

    /// Whether `key` is open right now, without opening it.
    ///
    /// This is the whole read path — `get_or_open` calls it before deciding to
    /// decode, and `peek` exposes it to callers that only want the answer. It
    /// takes the lock, upgrades, and leaves; there is nothing else for it to
    /// touch, because a caller holding the entry is the only thing keeping it
    /// alive.
    fn lookup(&self, key: &OpenKey) -> Option<Arc<V>> {
        let inner = self.lock();
        let hit = inner.live.get(key).and_then(Weak::upgrade);
        drop(inner);
        hit
    }

    fn publish(&self, key: &OpenKey, shared: &Arc<V>, gate: &Arc<Mutex<()>>) {
        let mut inner = self.lock();
        drop(inner.live.insert(key.clone(), Arc::downgrade(shared)));
        inner.release_gate(key, gate);
        // Keys whose entry is gone would otherwise accumulate one per commit.
        inner.live.retain(|_, weak| weak.strong_count() > 0);
    }
}

impl<V> Inner<V> {
    /// Removes `key`'s build gate, but only if it is still the one `gate` names.
    ///
    /// The identity check is load-bearing, and it lives here — once — because
    /// both callers need it and a second hand-written copy is exactly what went
    /// wrong before: without it a caller can delete a gate it never held. A
    /// caller that inserted a gate, then found the entry already published,
    /// would remove whichever gate now sits at the key, possibly one a
    /// *different* caller is mid-decode under. Every arrival after that inserts
    /// a fresh gate instead of blocking on the live one, so one commit is
    /// decoded several times at once — precisely the memory this cache exists
    /// not to spend, and invisible to any test that only checks answers.
    fn release_gate(&mut self, key: &OpenKey, gate: &Arc<Mutex<()>>) {
        if self
            .gates
            .get(key)
            .is_some_and(|held| Arc::ptr_eq(held, gate))
        {
            drop(self.gates.remove(key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(overlay: &str) -> OpenKey {
        (PathBuf::from(overlay), PathBuf::from("segments-v1"))
    }

    /// A decode that may be shared — the ordinary case.
    fn shareable<V>(value: V) -> Opened<V> {
        Opened {
            value,
            shareable: true,
        }
    }

    /// A caller must never remove a build gate another caller is decoding under.
    ///
    /// Releasing by key alone allows it: a caller that inserts a gate, then
    /// finds the entry already published, would remove whatever gate now sits
    /// at that key. Everyone arriving after that inserts a fresh gate rather
    /// than blocking on the live one, so a single commit is decoded several
    /// times concurrently — the exact cost this cache exists to remove, and
    /// invisible to every other test here because nothing about the *answers*
    /// changes.
    #[test]
    fn releasing_a_gate_you_do_not_hold_leaves_the_owner_alone() {
        let cache = OpenCache::<u32>::new();

        let owner = {
            let mut inner = cache.lock();
            Arc::clone(inner.gates.entry(key("a")).or_default())
        };
        let stranger = Arc::new(Mutex::new(()));

        cache.release_gate(&key("a"), &stranger);

        let still_held = {
            let inner = cache.lock();
            inner.gates.get(&key("a")).map(Arc::clone)
        };
        assert!(
            still_held.is_some_and(|held| Arc::ptr_eq(&held, &owner)),
            "a gate belonging to an in-flight decode must survive someone else's release"
        );

        cache.release_gate(&key("a"), &owner);
        assert!(
            cache.lock().gates.is_empty(),
            "the caller that owns a gate must still be able to release it"
        );
    }

    /// `peek` answers without opening, and without creating anything.
    ///
    /// Both halves matter. Asking must not decode — the background warmer asks
    /// once per source at start-up, and answering by decoding would build one
    /// full working set per source at exactly the wrong moment. And asking must
    /// leave no trace: an entry conjured by a question would be held by nobody
    /// and freed immediately anyway, and a gate left behind would block the
    /// next real open.
    #[test]
    fn peeking_answers_without_opening_and_leaves_nothing_behind() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);

        assert!(
            cache.lookup(&key("a")).is_none(),
            "a commit no session holds must read as not open"
        );
        assert!(
            cache.lock().gates.is_empty(),
            "asking must not leave a build gate behind"
        );

        // The decisive half: asking must not have created an entry, so the open
        // that follows still does the work.
        let opened = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(1))
            })
            .unwrap();
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "a peek before the open must neither decode nor stand in for one"
        );

        assert!(
            cache
                .lookup(&key("a"))
                .is_some_and(|hit| Arc::ptr_eq(&hit, &opened)),
            "peek must hand back the entry a session is holding, not a copy"
        );

        drop(opened);
        assert!(
            cache.lookup(&key("a")).is_none(),
            "once the last holder is gone the commit is no longer open, and \
             peek must say so rather than resurrect it"
        );
    }

    /// A decode that came out incomplete is served to the caller that asked for
    /// it and shared with nobody, so the gap stays recoverable: the next caller
    /// opens the files again rather than inheriting the missing rows.
    #[test]
    fn an_unshareable_decode_is_served_but_not_cached() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);

        let partial = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(Opened {
                    value: 1,
                    shareable: false,
                })
            })
            .unwrap();
        assert_eq!(*partial, 1, "the caller still gets its decode");

        let next = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(2))
            })
            .unwrap();

        assert_eq!(*next, 2, "the next caller must decode again, not inherit");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(
            !Arc::ptr_eq(&partial, &next),
            "an unshareable decode must never be handed to a second caller"
        );
    }

    #[test]
    fn a_second_open_of_one_key_reuses_the_first_decode() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);

        let first = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(7))
            })
            .unwrap();
        let second = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(7))
            })
            .unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "one key must hand back one allocation, not an equal copy"
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn distinct_overlays_do_not_share_an_entry() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);

        let a = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(1))
            })
            .unwrap();
        let b = cache
            .get_or_open(&key("b"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(2))
            })
            .unwrap();

        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// Two keys differing only in their segment root are distinct entries.
    ///
    /// This covers the cache side only — that the second half of the key is
    /// honoured. It says nothing about whether the *caller* puts the segment
    /// root there, and would stay green if it stopped doing so; that half is
    /// pinned against `ColumnarStorage::open_key` in `tests/overlay_shared_open.rs`.
    #[test]
    fn the_segment_root_is_part_of_the_key() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);
        let same_overlay_first = (PathBuf::from("overlay.bin"), PathBuf::from("segments-v1"));
        let same_overlay_second = (PathBuf::from("overlay.bin"), PathBuf::from("segments-v2"));

        let first = cache
            .get_or_open(&same_overlay_first, || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(1))
            })
            .unwrap();
        let second = cache
            .get_or_open(&same_overlay_second, || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(2))
            })
            .unwrap();

        assert!(
            !Arc::ptr_eq(&first, &second),
            "same overlay under a different segment root is a different entry"
        );
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// An entry nobody holds is freed, not kept: the cache adds no retention of
    /// its own, so nothing accumulates once the sessions using a commit go.
    #[test]
    fn an_entry_no_session_holds_is_freed_not_resurrected() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);

        drop(
            cache
                .get_or_open(&key("a"), || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(shareable(1))
                })
                .unwrap(),
        );
        drop(
            cache
                .get_or_open(&key("a"), || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(shareable(1))
                })
                .unwrap(),
        );

        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "nothing held 'a' between the two opens, so the second must decode \
             again rather than find a copy the cache kept on its own"
        );
    }

    /// The property the whole saving rests on: a session HOLDING an entry is
    /// what prevents a second decode, and it is the only thing that does. Other
    /// commits being opened in between changes nothing.
    #[test]
    fn a_live_holder_is_what_prevents_a_second_decode() {
        let cache = OpenCache::<u32>::new();
        let builds = AtomicUsize::new(0);

        let held = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(1))
            })
            .unwrap();
        drop(
            cache
                .get_or_open(&key("b"), || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(shareable(2))
                })
                .unwrap(),
        );

        let again = cache
            .get_or_open(&key("a"), || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(shareable(1))
            })
            .unwrap();

        assert!(Arc::ptr_eq(&held, &again));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// The load this cache exists for: a pool starting several sessions on one
    /// commit at once. Without the per-key gate every thread decodes its own
    /// copy, which is exactly the memory the cache is meant to remove.
    #[test]
    #[expect(
        clippy::needless_collect,
        reason = "collecting the handles is what starts all four threads before any \
                  of them is joined; fusing the two iterators would spawn and join \
                  one thread at a time, so the concurrent cold open this test exists \
                  to create would never happen and it would pass without testing it"
    )]
    fn concurrent_first_opens_of_one_key_decode_once() {
        let cache = Arc::new(OpenCache::<u32>::new());
        let builds = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(4));

        let racers: Vec<_> = (0..4)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let builds = Arc::clone(&builds);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    cache
                        .get_or_open(&key("a"), || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            Ok(shareable(1))
                        })
                        .unwrap()
                })
            })
            .collect();

        let opened: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();

        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "the per-key gate must single-flight a cold decode"
        );
        assert!(
            opened
                .windows(2)
                .all(|pair| Arc::ptr_eq(&pair[0], &pair[1]))
        );
    }

    #[test]
    fn a_failed_open_is_not_cached_and_leaves_the_key_usable() {
        let cache = OpenCache::<u32>::new();

        assert!(
            cache
                .get_or_open(&key("a"), || Err(anyhow::anyhow!("decode failed")))
                .is_err()
        );

        let recovered = cache.get_or_open(&key("a"), || Ok(shareable(5))).unwrap();
        assert_eq!(*recovered, 5);
    }
}
