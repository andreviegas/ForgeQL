/// Trigram inverted index for fast substring search over symbol names.
///
/// For a query string `q`, any row whose name contains `q` must contain
/// every consecutive 3-byte window (trigram) of `q`.  We intersect the
/// posting lists for all trigrams of `q` to produce a small candidate
/// set; the caller then applies the full predicate to only those rows.
///
/// Build cost  : O(N × `avg_name_len`) — one pass over all names.
/// Query cost  : proportional to posting list sizes and candidate count.
/// Space cost  : O(N × `avg_name_len`) entries total across all lists.
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

// -----------------------------------------------------------------------
// TrigramIndex
// -----------------------------------------------------------------------

/// Trigram inverted index keyed by consecutive 3-byte windows.
///
/// **Case-insensitive (ASCII).**  Both `insert` and `candidates` lowercase
/// their input before deriving trigrams, mirroring `filter::like_match`'s
/// ASCII case-insensitive semantics.  Non-ASCII bytes pass through unchanged
/// — full Unicode case-folding is not required because `like_match` itself
/// only does ASCII case folding.
///
/// Posting lists are maintained in **sorted order** (ascending row index).
/// This is guaranteed by always inserting via `push_row` / `merge` which
/// visit rows in index order.
#[derive(Debug, Default, Clone)]
pub struct TrigramIndex {
    /// trigram → sorted list of row indices that contain it in their name.
    posting: HashMap<[u8; 3], Vec<usize>>,
}

impl TrigramIndex {
    /// Drop every posting list.  Used by incremental update paths
    /// (e.g. `SymbolTable::purge_file`) before a full rebuild.
    pub fn clear(&mut self) {
        self.posting.clear();
    }

    /// Record that `row_idx` has a name containing every trigram of `text`.
    ///
    /// Must be called with monotonically increasing `row_idx` to preserve
    /// sorted posting lists.
    pub fn insert(&mut self, row_idx: usize, text: &str) {
        let bytes = text.as_bytes();
        if bytes.len() < 3 {
            return;
        }
        // Deduplicate trigrams per name to avoid inflated posting lists.
        // HashSet keeps insert at O(name_length); a Vec::contains scan would
        // be quadratic and matter for very long names (e.g. 9 KB comment
        // bodies indexed under their own `name` field).
        let mut seen: HashSet<[u8; 3]> = HashSet::new();
        for w in bytes.windows(3) {
            let t = [
                w[0].to_ascii_lowercase(),
                w[1].to_ascii_lowercase(),
                w[2].to_ascii_lowercase(),
            ];
            if seen.insert(t) {
                self.posting.entry(t).or_default().push(row_idx);
            }
        }
    }

    /// Return the sorted list of row indices whose names contain `substr`,
    /// or `None` if `substr` is shorter than 3 bytes (cannot use trigrams).
    ///
    /// Returns `Some(empty)` when no row can possibly match.
    #[must_use]
    pub fn candidates(&self, substr: &str) -> Option<Vec<usize>> {
        let bytes = substr.as_bytes();
        if bytes.len() < 3 {
            return None;
        }

        // Collect unique trigrams of the query string (lowercased to match
        // the case-insensitive insert path).
        let mut trigrams: Vec<[u8; 3]> = Vec::new();
        for w in bytes.windows(3) {
            let t = [
                w[0].to_ascii_lowercase(),
                w[1].to_ascii_lowercase(),
                w[2].to_ascii_lowercase(),
            ];
            if !trigrams.contains(&t) {
                trigrams.push(t);
            }
        }

        // Gather posting lists; any missing trigram means zero candidates.
        let mut lists: Vec<&Vec<usize>> = Vec::with_capacity(trigrams.len());
        for t in &trigrams {
            match self.posting.get(t) {
                Some(list) => lists.push(list),
                None => return Some(Vec::new()), // trigram absent → no match
            }
        }

        // Sort by ascending length — start with most selective list.
        lists.sort_unstable_by_key(|v| v.len());

        // Intersect all lists.
        let mut result = lists[0].clone();
        for list in &lists[1..] {
            result = intersect_sorted(&result, list);
            if result.is_empty() {
                return Some(Vec::new());
            }
        }
        Some(result)
    }

    /// Number of distinct trigrams stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.posting.len()
    }

    /// `true` if no trigrams have been indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.posting.is_empty()
    }
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Intersect two **sorted** slices in O(a + b).
fn intersect_sorted(a: &[usize], b: &[usize]) -> Vec<usize> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
        }
    }
    result
}

// -----------------------------------------------------------------------
// Literal extraction helpers — used by the query planner
// -----------------------------------------------------------------------

/// Extract the longest contiguous literal run (≥ 3 bytes, no regex metacharacters)
/// from a regex pattern.
///
/// Used to find a required substring that every regex match must contain,
/// enabling a trigram pre-filter before regex evaluation.
///
/// Returns `None` if no literal run of length ≥ 3 exists.
#[must_use]
pub fn extract_regex_literal(pat: &str) -> Option<String> {
    const META: &[u8] = b".*+?[](){}|\\";
    // Strip anchors — they don't affect which literal substrings are required.
    let s = pat.strip_prefix('^').unwrap_or(pat);
    let s = s.strip_suffix('$').unwrap_or(s);

    let bytes = s.as_bytes();
    let mut best_start = 0usize;
    let mut best_len = 0usize;
    let mut cur_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            // Escaped character — end the current literal run; skip both bytes.
            let run = i - cur_start;
            if run > best_len {
                best_len = run;
                best_start = cur_start;
            }
            i += 2; // skip '\' and the next byte
            cur_start = i;
            continue;
        }
        if META.contains(&b) {
            let run = i - cur_start;
            if run > best_len {
                best_len = run;
                best_start = cur_start;
            }
            cur_start = i + 1;
        }
        i += 1;
    }
    // Check the final run.
    let run = bytes.len() - cur_start;
    if run > best_len {
        best_len = run;
        best_start = cur_start;
    }

    if best_len >= 3 {
        Some(s[best_start..best_start + best_len].to_owned())
    } else {
        None
    }
}

/// Extract the longest contiguous literal run (≥ 3 bytes) from a SQL LIKE
/// pattern where `%` matches any sequence and `_` matches any single char.
///
/// Returns `None` if no such run exists.
#[must_use]
pub fn extract_like_literal(pat: &str) -> Option<String> {
    let bytes = pat.as_bytes();
    let mut best_start = 0usize;
    let mut best_len = 0usize;
    let mut cur_start = 0usize;

    for (i, &b) in bytes.iter().enumerate() {
        if b == b'%' || b == b'_' {
            let run = i - cur_start;
            if run > best_len {
                best_len = run;
                best_start = cur_start;
            }
            cur_start = i + 1;
        }
    }
    let run = bytes.len() - cur_start;
    if run > best_len {
        best_len = run;
        best_start = cur_start;
    }

    if best_len >= 3 {
        Some(pat[best_start..best_start + best_len].to_owned())
    } else {
        None
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn build_index(names: &[&str]) -> TrigramIndex {
        let mut idx = TrigramIndex::default();
        for (i, name) in names.iter().enumerate() {
            idx.insert(i, name);
        }
        idx
    }

    #[test]
    fn exact_match_found() {
        let idx = build_index(&["gpio_pin_set", "gpio_pin_get", "uart_write"]);
        let cands = idx.candidates("gpio_pin_set").unwrap();
        assert!(cands.contains(&0));
        assert!(!cands.contains(&2));
    }

    #[test]
    fn substring_match() {
        let idx = build_index(&["k_thread_create", "k_thread_join", "k_sem_take"]);
        let cands = idx.candidates("k_thread_").unwrap();
        assert!(cands.contains(&0), "k_thread_create should match");
        assert!(cands.contains(&1), "k_thread_join should match");
        assert!(!cands.contains(&2), "k_sem_take should not match");
    }

    #[test]
    fn absent_trigram_returns_empty() {
        let idx = build_index(&["foo_bar", "baz_qux"]);
        let cands = idx.candidates("zzz").unwrap();
        assert!(cands.is_empty());
    }

    #[test]
    fn short_query_returns_none() {
        let idx = build_index(&["foo"]);
        assert!(idx.candidates("fo").is_none());
        assert!(idx.candidates("").is_none());
    }

    #[test]
    fn extract_regex_literal_prefix() {
        assert_eq!(
            extract_regex_literal("^k_thread_.*$").as_deref(),
            Some("k_thread_")
        );
    }

    #[test]
    fn extract_regex_literal_suffix() {
        assert_eq!(extract_regex_literal("^.*_init$").as_deref(), Some("_init"));
    }

    #[test]
    fn extract_regex_literal_middle() {
        // get_.*_config → longest run is "_config" (7) > "get_" (4)
        assert_eq!(
            extract_regex_literal("^get_.*_config$").as_deref(),
            Some("_config")
        );
    }

    #[test]
    fn extract_regex_literal_pure_literal_returns_none_when_short() {
        // "ab" is only 2 chars → None
        assert!(extract_regex_literal("^ab$").is_none());
    }

    #[test]
    fn extract_like_literal_middle_wildcard() {
        // `_` is a single-char wildcard in LIKE, so "CONFIG_BT" is split at `_`.
        // Longest literal run is "CONFIG" (6 bytes).
        assert_eq!(
            extract_like_literal("%CONFIG_BT%").as_deref(),
            Some("CONFIG")
        );
    }

    #[test]
    fn extract_like_literal_single_char_wildcards() {
        // "gpio_pin_set" — underscores are wildcards in LIKE, so runs are
        // "gpio", "pin", "set".  Longest is "gpio" (4).
        assert_eq!(
            extract_like_literal("gpio_pin_set").as_deref(),
            Some("gpio")
        );
    }

    #[test]
    fn extract_like_literal_no_useful_literal() {
        assert!(extract_like_literal("a_b").is_none());
    }

    #[test]
    fn case_insensitive_lookup() {
        // Insert mixed-case names; lookup with uppercase pattern must
        // still find them.  Mirrors `like_match`'s ASCII case-insensitive
        // semantics.
        let idx = build_index(&["encenderMotor", "apagarMotor", "uart_write"]);
        let cands = idx.candidates("MOTOR").unwrap();
        assert!(cands.contains(&0), "MOTOR should match encenderMotor");
        assert!(cands.contains(&1), "MOTOR should match apagarMotor");
        assert!(!cands.contains(&2), "MOTOR should not match uart_write");
    }

    #[test]
    fn case_insensitive_lookup_lowercase_query_matches_uppercase_name() {
        let idx = build_index(&["GPIO_PIN_SET", "uart_write"]);
        let cands = idx.candidates("gpio").unwrap();
        assert!(cands.contains(&0), "gpio should match GPIO_PIN_SET");
        assert!(!cands.contains(&1));
    }

    #[test]
    fn clear_resets_index() {
        let mut idx = build_index(&["foo_bar", "baz_qux"]);
        assert!(!idx.is_empty());
        idx.clear();
        assert!(idx.is_empty());
        // After clear, re-inserting a different row produces a fresh index.
        idx.insert(0, "new_name");
        let cands = idx.candidates("new").unwrap();
        assert_eq!(cands, vec![0]);
    }

    #[test]
    fn long_name_dedup_is_linear() {
        // Names with many repeated trigrams must not regress to O(K²)
        // when re-indexed.  Use a name that fully exercises the dedup
        // path (~1 KB).  The test merely guards correctness; perf is
        // verified by the linear HashSet-based implementation.
        let long = "ab".repeat(512); // 1024 bytes, only 2 unique trigrams: "aba", "bab"
        let mut idx = TrigramIndex::default();
        idx.insert(0, &long);
        // Posting list for trigram "aba" should contain row 0 exactly once.
        let cands = idx.candidates("ababab").unwrap();
        assert_eq!(cands, vec![0]);
    }
}
