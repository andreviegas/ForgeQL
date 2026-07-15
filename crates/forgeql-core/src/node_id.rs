//! Stable node-address helpers — SHA-256 path hashing and `node_id` formatting.
//!
//! A `node_id` is a compact, stable handle for a named AST node:
//!
//!   `n{segment_id}.{ordinal:04}`
//!
//! The `segment_id` portion is the shortest unambiguous prefix of the SHA-256
//! of the normalized source file path.  All computation is performed **once**
//! at [`crate::storage::columnar::overlay::Overlay`] open time and cached in
//! [`crate::storage::columnar::overlay::SegmentMeta`].  Query paths call only
//! string formatting — no SHA-256, no lock, no global state.

use sha2::{Digest, Sha256};

const DEFAULT_SEGMENT_PREFIX_HEX: u8 = 12;

fn normalize_relative_path(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_owned();
    }
    p
}

/// Compute the raw SHA-256 bytes of a normalized source-file path.
///
/// Called once per file during [`crate::storage::columnar::overlay::Overlay`]
/// open time inside `decode_segment_metas`.  Never called at query time.
#[must_use]
pub fn sha256_of_path(path: &str) -> [u8; 32] {
    let normalized = normalize_relative_path(path);
    Sha256::digest(normalized.as_bytes()).into()
}

/// Find the minimum hex-prefix length that makes `hash` unambiguous among
/// `all_hashes`.
///
/// Starts at 12 hex characters (6 raw bytes) and grows by 2 on collision.
/// Returns a count of hex characters (always even, range 12–64).
///
/// Called once per file during overlay open time.  Never called at query time.
#[must_use]
pub fn shortest_prefix_len(hash: &[u8; 32], all_hashes: &[[u8; 32]]) -> u8 {
    let mut hex_len = DEFAULT_SEGMENT_PREFIX_HEX;
    while hex_len < 64 {
        let prefix_bytes = usize::from(hex_len) / 2;
        let collision = all_hashes
            .iter()
            .any(|other| other != hash && other[..prefix_bytes] == hash[..prefix_bytes]);
        if !collision {
            return hex_len;
        }
        hex_len += 2;
    }
    64
}

/// Format a `segment_id` display string from raw SHA-256 bytes.
///
/// Reads `prefix_len / 2` bytes from `sha256` and formats them as lowercase
/// hex.  Pure formatting — no SHA-256, no lock, no allocation beyond the
/// returned `String`.
///
/// Callers should prefer
/// [`crate::storage::columnar::overlay::SegmentMeta::segment_id`]
/// which reads directly from the pre-computed fields.
#[must_use]
pub fn hex_prefix(sha256: &[u8; 32], prefix_len: u8) -> String {
    let byte_count = (prefix_len as usize) / 2;
    sha256[..byte_count]
        .iter()
        .fold(String::with_capacity(prefix_len as usize), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Format a `node_id` from raw SHA-256 prefix bytes and a per-file ordinal.
///
/// Format: `n{segment_id}.{ordinal:04}`.
///
/// Callers should prefer
/// [`crate::storage::columnar::overlay::SegmentMeta::node_id`].
#[must_use]
pub fn format_node_id(sha256: &[u8; 32], prefix_len: u8, ordinal: u32) -> String {
    format!("n{}.{ordinal:04}", hex_prefix(sha256, prefix_len))
}

/// Compute a `node_id` from a source file path and per-file ordinal.
///
/// Hashes `path` with SHA-256 and uses the default 12-hex-character prefix.
/// Use this only when no
/// [`crate::storage::columnar::overlay::SegmentMeta`] is available (e.g.
/// `SHOW body`, `SHOW members`).  For columnar paths (`SHOW outline`)
/// prefer [`crate::storage::columnar::overlay::SegmentMeta::node_id`]
/// which reads a pre-computed, dedup-aware prefix from the overlay.
#[must_use]
pub fn make_node_id(path: &str, ordinal: u32) -> String {
    let hash = sha256_of_path(path);
    format_node_id(&hash, DEFAULT_SEGMENT_PREFIX_HEX, ordinal)
}

/// Extract the ordinal from a base node handle (`n<hex>.<ordinal>`).
///
/// This is the form [`split_node_offset`] returns. Returns `None` for a
/// bare-hex whole-file or directory handle (no `.<ordinal>` suffix), which
/// addresses a path rather than an indexed node.
#[must_use]
pub fn ordinal_of(base_id: &str) -> Option<u32> {
    base_id
        .rsplit_once('.')
        .and_then(|(_, ord)| ord.parse().ok())
}

/// Surface a node's handle for display.
///
/// When `block_ord`/`block_off` are present
/// (the node is a block member), return `block_id(offset)` — reusing `own_id`'s
/// segment prefix so only the ordinal and offset change. Otherwise return
/// `own_id` unchanged. FIND and SHOW outline both call this, so a block member
/// surfaces the same way everywhere.
/// The block node id (`{seg}.{block_ord}`) for a member whose handle is `own_id`.
/// Strips the member ordinal and substitutes the block ordinal, so block members and the
/// block itself share one segment-qualified identity.
#[must_use]
pub fn block_node_id(own_id: &str, block_ord: &str) -> String {
    let seg = own_id.rsplit_once('.').map_or(own_id, |(s, _)| s);
    format!("{seg}.{block_ord}")
}

#[must_use]
pub fn surface_block_id(own_id: &str, block_ord: Option<&str>, block_off: Option<&str>) -> String {
    if let (Some(ord), Some(off)) = (block_ord, block_off) {
        format!("{}({off})", block_node_id(own_id, ord))
    } else {
        own_id.to_string()
    }
}

/// Format a `rev` handle from the packed u64 stored in `col_rev`.
///
/// Format: `h{:016x}` — 16 lowercase hex chars prefixed with `h`.
/// The zero sentinel (analysis-only row) returns an empty string.
#[must_use]
pub fn format_rev(rev: u64) -> String {
    if rev == 0 {
        return String::new();
    }
    format!("h{rev:016x}")
}

/// Format a rev that must always render, including zero.
///
/// [`format_rev`] renders a zero rev as the empty string ("no rev"), which is
/// right for analysis-only rows. A directory rev is a membership XOR and is
/// legitimately zero for an empty directory; an empty string there would read
/// as "no rev" and defeat the mandatory `IF REV` gate on a recursive delete.
#[must_use]
pub fn format_rev_exact(rev: u64) -> String {
    format!("h{rev:016x}")
}

/// Rev of a raw byte span: SHA-256, first 8 bytes little-endian.
///
/// Identical to the `row_rev` computation the file indexer applies to a node
/// byte range, so a whole-file rev and a node rev are the same scheme.
#[must_use]
pub fn rev_of_bytes(bytes: &[u8]) -> u64 {
    let digest = Sha256::digest(bytes);
    u64::from_le_bytes(digest[..8].try_into().unwrap_or([0u8; 8]))
}

/// XOR-fold a path fingerprint into a directory membership rev.
///
/// The directory rev is one flat XOR of the first 8 bytes of
/// `sha256_of_path` over every file underneath the directory, at any depth.
/// It moves when the membership of the subtree changes (add / remove / rename
/// / move) and deliberately does NOT move when file content changes — content
/// staleness is what the per-file rev is for. No file is read to compute it.
#[must_use]
pub fn fold_path_rev(acc: u64, path: &str) -> u64 {
    let hash = sha256_of_path(path);
    acc ^ u64::from_le_bytes(hash[..8].try_into().unwrap_or([0u8; 8]))
}

/// `(base_id, Some((start, end)))` — the split form of a `node_id` that may
/// carry a `(n)` / `(n-m)` line-offset suffix.
type NodeOffsetSplit<'a> = (&'a str, Option<(u32, u32)>);

/// Split a `node_id` that may carry a node-relative line offset suffix into its
/// base id and the optional 1-based, inclusive `(start, end)` offsets.
///
/// Accepts three shapes:
/// * `id` — no suffix, returns `(id, None)` (the whole node).
/// * `id(n)` — single offset, returns `(id, Some((n, n)))`.
/// * `id(n-m)` — inclusive range, returns `(id, Some((n, m)))`.
///
/// # Errors
/// Returns an error string when the suffix is malformed: a `(` without a closing
/// `)`, a non-numeric or empty offset, a `0` offset (offsets are 1-based), or an
/// inverted range (`m < n`).
pub fn split_node_offset(node_id: &str) -> Result<NodeOffsetSplit<'_>, String> {
    let Some(open) = node_id.find('(') else {
        return Ok((node_id, None));
    };
    if !node_id.ends_with(')') {
        return Err(format!(
            "malformed node offset in '{node_id}': '(' without closing ')'"
        ));
    }
    let base = &node_id[..open];
    let inner = &node_id[open + 1..node_id.len() - 1];
    let (start, end) = if let Some((a, b)) = inner.split_once('-') {
        (parse_offset(a, node_id)?, parse_offset(b, node_id)?)
    } else {
        let n = parse_offset(inner, node_id)?;
        (n, n)
    };
    if start == 0 {
        return Err(format!(
            "node offset in '{node_id}' is 1-based; offset 0 is invalid"
        ));
    }
    if end < start {
        return Err(format!(
            "node offset range in '{node_id}' is inverted ({start}-{end})"
        ));
    }
    Ok((base, Some((start, end))))
}

fn parse_offset(s: &str, node_id: &str) -> Result<u32, String> {
    s.trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid node offset '{s}' in '{node_id}'"))
}

/// Resolve node-relative offsets to absolute, 1-based, inclusive source lines.
///
/// `node_line`/`node_end_line` are the addressed node's absolute span. With no
/// offset the whole node span is returned; with `Some((a, b))` the range is
/// `node_line + a - 1 ..= node_line + b - 1`.
///
/// # Errors
/// Returns an error string when the offset runs past the node's last line
/// (out-of-bounds — a corruption guard).
pub fn offset_lines(
    node_line: usize,
    node_end_line: usize,
    offset: Option<(u32, u32)>,
) -> Result<(usize, usize), String> {
    let Some((a, b)) = offset else {
        return Ok((node_line, node_end_line));
    };
    // A directory node spans no lines (end_line < line); an offset into it is
    // meaningless, and the span arithmetic below would underflow.
    if node_end_line < node_line {
        return Err("node has no addressable lines".to_owned());
    }
    let span = node_end_line - node_line + 1;
    let off_start = usize::try_from(a).unwrap_or(usize::MAX);
    let off_end = usize::try_from(b).unwrap_or(usize::MAX);
    if off_end > span {
        return Err(format!(
            "node offset {a}-{b} runs past the node's {span} line(s)"
        ));
    }
    Ok((node_line + off_start - 1, node_line + off_end - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_path_is_deterministic() {
        assert_eq!(sha256_of_path("src/main.rs"), sha256_of_path("src/main.rs"));
    }

    #[test]
    fn path_normalization_strips_dot_slash() {
        assert_eq!(
            sha256_of_path("./src/main.rs"),
            sha256_of_path("src/main.rs")
        );
        assert_eq!(
            sha256_of_path("src\\main.rs"),
            sha256_of_path("src/main.rs")
        );
    }

    #[test]
    fn shortest_prefix_len_minimum_is_12() {
        let h1 = sha256_of_path("src/main.rs");
        let all = vec![h1];
        assert_eq!(shortest_prefix_len(&h1, &all), 12);
    }

    #[test]
    fn hex_prefix_length_matches_prefix_len() {
        let h = sha256_of_path("src/lib.rs");
        let s = hex_prefix(&h, 12);
        assert_eq!(s.len(), 12);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn format_node_id_shape() {
        let h = sha256_of_path("src/main.rs");
        let id = format_node_id(&h, 12, 42);
        assert!(id.starts_with('n'));
        assert!(id.contains('.'));
        assert!(id.ends_with(".0042"));
    }

    #[test]
    fn split_node_offset_no_suffix() {
        assert_eq!(split_node_offset("nabc.0042"), Ok(("nabc.0042", None)));
    }

    #[test]
    fn split_node_offset_single() {
        assert_eq!(
            split_node_offset("nabc.0042(3)"),
            Ok(("nabc.0042", Some((3, 3))))
        );
    }

    #[test]
    fn split_node_offset_inclusive_range() {
        assert_eq!(
            split_node_offset("nabc.0042(2-5)"),
            Ok(("nabc.0042", Some((2, 5))))
        );
    }

    #[test]
    fn split_node_offset_rejects_zero() {
        assert!(split_node_offset("nabc.0042(0)").is_err());
    }

    #[test]
    fn split_node_offset_rejects_inverted_range() {
        assert!(split_node_offset("nabc.0042(5-2)").is_err());
    }

    #[test]
    fn split_node_offset_rejects_unclosed_paren() {
        assert!(split_node_offset("nabc.0042(2").is_err());
    }

    #[test]
    fn split_node_offset_rejects_non_numeric() {
        assert!(split_node_offset("nabc.0042(x)").is_err());
        assert!(split_node_offset("nabc.0042()").is_err());
    }

    #[test]
    fn offset_lines_whole_node_when_none() {
        assert_eq!(offset_lines(26, 29, None), Ok((26, 29)));
    }

    #[test]
    fn offset_lines_single_offset_maps_to_one_line() {
        // first line of the node, then the last line of a 4-line node
        assert_eq!(offset_lines(26, 29, Some((1, 1))), Ok((26, 26)));
        assert_eq!(offset_lines(26, 29, Some((4, 4))), Ok((29, 29)));
    }

    #[test]
    fn surface_block_id_builds_handle_for_members() {
        assert_eq!(
            surface_block_id("nabc123def456.0011", Some("0007"), Some("2")),
            "nabc123def456.0007(2)"
        );
    }

    #[test]
    fn surface_block_id_passes_through_non_members() {
        assert_eq!(
            surface_block_id("nabc123def456.0011", None, None),
            "nabc123def456.0011"
        );
        // Only one of the two fields present is not a block member.
        assert_eq!(
            surface_block_id("nabc123def456.0011", Some("0007"), None),
            "nabc123def456.0011"
        );
    }

    #[test]
    fn offset_lines_inclusive_range_maps_interior() {
        assert_eq!(offset_lines(26, 29, Some((2, 3))), Ok((27, 28)));
    }

    #[test]
    fn offset_lines_rejects_out_of_bounds() {
        // node spans 4 lines (26..=29); anything past line 4 is a corruption guard
        assert!(offset_lines(26, 29, Some((5, 5))).is_err());
        assert!(offset_lines(26, 29, Some((1, 9))).is_err());
    }
}
