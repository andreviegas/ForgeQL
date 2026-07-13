//! Columnar storage engine for `ForgeQL` — Phase 03+.
//!
//! This module implements the **write side** of the columnar storage format.
//! It is enabled automatically when a `.forgeql.yaml` is present for the source.
//!
//! # Architecture
//!
//! - [`SegmentBuilder`]: assembles and flushes one segment directory from a
//!   slice of `IndexRow`s that all belong to the same source file.
//! - [`ShadowWriter`]: iterates over a fully-built [`SymbolTable`] and drives
//!   one [`SegmentBuilder`] per source file.
//!
//! # On-disk layout
//!
//! ```text
//! <bare-repo>/forgeql/segments/<provider_id>/<content_id_hex>/
//! ├── header.bin            # 80-byte preamble + column entries
//! ├── col_name_id.bin       # [u32; row_count]
//! ├── col_fql_kind_id.bin   # [u32; row_count]
//! ├── col_line.bin          # [u32; row_count]
//! ├── col_byte_start.bin    # [u32; row_count]
//! ├── col_byte_end.bin      # [u32; row_count]
//! ├── col_usages_count.bin  # [u32; row_count]
//! ├── col_language_id.bin   # [u32; row_count]
//! ├── strings_offsets.bin   # [u32; string_count + 1]
//! ├── strings_data.bin      # UTF-8 bytes, concatenated
//! ├── postings_fql_kind.bin # (kind_id: u32, len: u32, bytes)* per kind
//! ├── name.fst              # fst::Map — name → packed (count | byte_offset<<32)
//! └── name_postings.bin     # flat [u32] row IDs referenced by name.fst
//! ```
//!
//! [`SymbolTable`]: crate::ast::index::SymbolTable

pub mod build_context;
pub mod columnar_storage;
pub mod delta_file;
pub mod dirty_overlay;
pub mod gc;
pub mod manifest;
pub mod overlay;
pub mod overlay_builder;
pub mod overlay_lock;
pub mod overlay_writer;
pub mod segment_builder;
pub mod segment_reader;
pub mod shadow_writer;

pub use build_context::BuildInput;
pub use build_context::ColumnarBuildContext;
pub use columnar_storage::ColumnarStorage;
pub use delta_file::{DeltaFile, StagedEntry};
pub use dirty_overlay::DirtyOverlay;
pub use manifest::Manifest;
pub use overlay_builder::OverlayBuilder;
pub use segment_builder::{SegmentBuilder, SymbolRow};
pub use segment_reader::SegmentReader;
pub use shadow_writer::ShadowWriter;

/// Type-erased, thread-safe hash function for content addressing.
///
/// Wrap a `SourceProvider::hash_content` call behind this type to keep
/// `ShadowWriter` decoupled from the concrete provider type.
/// Example: `Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec())`
pub type HashFn = std::sync::Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static>;

/// Enrichment logic version — embedded in the segment, overlay, and manifest
/// storage paths.
///
/// **Bump this constant on every enrichment logic change** (new enricher, bug
/// fix, field rename).  The new version namespace is created automatically on
/// the next `USE`; the old one is orphaned and will be removed by the GC sprint.
///
/// History:
///   1 — initial columnar engine (v0.49.0)
///   2 — `condition_tests` clause counting fix (v0.49.1)
///   3 — `has_fallthrough` annotation suppression (v0.49.3)
///   4 — `lines` clipping for absorbed function_definition (v0.49.10, partial)
///   5 — `lines` clipping extended: preproc_ifdef + ERROR/DEVICE_API (v0.49.10)
///   6 — columnar overlay split into per-blob `.bin` files (v0.49.4)
///   7 — `is_magic` semantics fixed; numbers in string literals excluded (v0.50.2)
///   8 — FQOV v3: TOC-based binary overlay replaces bincode serialization (v0.50.11)
///   9 — `POSTING_ENRICHMENT_FIELDS` expansion: string-enum and boolean enrichment
///       fields now stored as per-field posting blobs in segments, enabling fast
///       WHERE/ORDER BY without full row materialization (v0.50.12)
///  10 — stable `node_id` handles introduced via persisted ordinals
///  11 — ordinal remapper improvements for reindex stability
///  12 — Phase A node_id policy gate: only addressable `fql_kind`s receive ordinals
///  13 — B-prep: col_parent_ordinal, col_rev, col_first/next/prev_sibling_ordinal as
///          typed columns; parent_ordinal promoted from enrichment string to u32 field
///  14 — branches-as-parents: control-flow nodes (if/while/for/switch/do) become
///          parents of their body statements, so node_ids nest under the branch
///          rather than the enclosing function (plan §4.1)
///  15 — block grouping: synthetic childless block nodes (e.g. `comment_block`)
///          span a run of same-kind sibling members, sharing the parent of the
///          members instead of nesting under it (Stage 1: comments)
///  16 — block grouping Stage 2: block members carry `block_ord`/`block_off`
///          fields so FIND/SHOW surface them as `block_id(offset)`
///  17 — block grouping: block rows carry a `content_hash` field so the reindex
///          remapper can keep block node ids stable across sibling-block edits
///  18 — block grouping: clamp a block member offset to its last content line
///          so a one-line doc/block comment surfaces as a single offset
///  19 — has_doc skips leading attribute/decorator siblings, so a documented
///          item with an interposed `#[...]` attribute is still detected as documented
///  20 — comment_block rows carry a `block_label` field (first-member snippet +
///          member count) for SHOW outline display; identity name stays `comment_block`
///  21 — CMake/Make control-flow rows: `control_flow` config sections added, and
///          control-flow rows from grammars without a `condition` field are named
///          by the construct's first line (previously nameless → unfindable).
///          v20 segments for .cmake/Makefile files lack these rows entirely —
///          this is the constant to bump when a change alters WHICH ROWS a file
///          produces (segments cache per blob under `{provider}-v{ENRICH_VER}/`;
///          the overlay SCHEMA_VERSION alone does not re-index cached segments).
///  22 — BUG-019: C and Rust shift expressions now resolve to
///          `fql_kind = "shift_expression"` (config-only: `shift_kinds` +
///          `kind_map` entries mirrored from cpp.json); v21 segments carry
///          those rows with an empty fql_kind.
///  23 — segments gain `usages_fst` / `usages_postings` blobs
///          (identifier text → 1-based source lines, the reference index).
///          v22 segments lack the blobs, so readers would silently report
///          zero usages — the bump forces a full re-index.
///  24 — AUTOSAR ECUC parameter/reference values (XML family) are now named
///          by their DEFINITION-REF's last path segment instead of the bare
///          tag name; v23 segments carry those rows named
///          "ECUC-NUMERICAL-PARAM-VALUE" etc., unfindable by parameter name.
///  25 — C and C++ `union` types, `typedef` aliases (scalar, function-pointer,
///          and the anonymous `typedef struct/enum { … } Name;` forms), and
///          enum constants are now indexed as `union` / `type_alias` /
///          `enumerator` rows with node ids; v24 segments lack those rows, so
///          the bump forces a full re-index.
///  26 — C and C++ struct/class/union/enum *references* and forward
///          declarations (`struct Foo *p;`, `struct Foo;`) are no longer
///          indexed as type symbols — only the definition (which carries a
///          body) is. This lets `SHOW members` and type resolution reach the
///          definition instead of a bodyless reference; v25 segments carry
///          the spurious reference rows.
///  27, 28 — Consumed during development of 29 and never released. Dev caches
///          at those versions hold drafts of the changes below and must not be
///          trusted. (Bumping on EVERY iteration of an indexing change is not
///          optional: a v(N) cache built from an earlier draft of your own
///          change is exactly as stale as a v(N-1) cache, and reusing it makes
///          the test suite pass against code that never ran.)
///  29 — Four changes to index output, none of which v26 segments have:
///          (a) JSON/YAML containers with no identifier member are now named by
///              their key-set skeleton, and arrays by their nearest ancestor
///              pair's key — v26 segments emit no row at all for those nodes,
///              so their children are reparented onto the wrong ancestor;
///          (b) a run of 8+ adjacent JSON `array` siblings now emits an
///              `array_block` row — v26 segments lack it, leaving a keyless
///              JSON document with zero addressable rows;
///          (c) block runs are scanned over *named* siblings, so members
///              separated by anonymous punctuation (JSON's `,`) group at all —
///              before this, a 201-element array scanned as a run of ONE and no
///              block was ever emitted;
///          (d) tree-sitter `ERROR` regions now emit `error` rows, so a broken
///              file is no longer silently, partially indexed. Zero-width
///              `MISSING` tokens are deliberately NOT emitted: a row spanning
///              no bytes could be seen but not read or repaired.
///   30 — BURNED. Consumed mid-session by an abandoned draft that made `error`
///        addressable (which shifts ordinals). That draft's run wrote v30
///        segments; the code was then reverted, but the segments survive. Any
///        later change reusing 30 silently reads those poisoned ordinals — it
///        cost a full gate run to find. A version is spent the moment ANY build
///        writes segments under it, released or not.
///   31 — `error` rows carry `error_scope` (`root` / `file` / `nested`) and
///        `error_bytes`. A raw tree-sitter ERROR is a terrible danger signal:
///        tree-sitter parses C without running the preprocessor, so `static
///        ALWAYS_INLINE void f(void)` errors on the return type while `f` still
///        indexes correctly. Zephyr has ~74k such regions and essentially none
///        is damage. Position + size let `parse_coverage` separate a healthy
///        macro-heavy header (~1.0) from a file whose extension lies (~0).
///        `error` remains absent from `is_addressable_fql_kind`, so this adds
///        FIELDS only — no ordinals are consumed and no node_id moves.
pub const ENRICH_VER: u32 = 31;

/// The filename used for the columnar delta file in the repository root.
pub const DELTA_FILE_NAME: &str = ".forgeql-columnar-delta";

/// The folder name used for columnar staging segments.
pub const STAGING_DIR_NAME: &str = ".forgeql-staging";
/// Encode a byte slice as a lowercase hex string.
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}
