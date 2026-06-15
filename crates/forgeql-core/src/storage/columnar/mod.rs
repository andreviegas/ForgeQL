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
pub const ENRICH_VER: u32 = 16;

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
