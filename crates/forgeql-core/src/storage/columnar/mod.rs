//! Columnar storage engine for `ForgeQL` — Phase 03+.
//!
//! This module implements the **write side** of the columnar storage format.
//! It is opt-in; enable via `.forgeql.yaml` → `columnar.shadow_write: true`.
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

pub mod columnar_storage;
pub mod manifest;
pub mod overlay;
pub mod overlay_builder;
pub mod overlay_lock;
pub mod segment_builder;
pub mod segment_reader;
pub mod shadow_writer;

pub use columnar_storage::ColumnarStorage;
pub use manifest::Manifest;
pub use overlay_builder::OverlayBuilder;
pub use segment_builder::SegmentBuilder;
pub use segment_reader::SegmentReader;
pub use shadow_writer::ShadowWriter;

/// Type-erased, thread-safe hash function for content addressing.
///
/// Wrap a `SourceProvider::hash_content` call behind this type to keep
/// `ShadowWriter` decoupled from the concrete provider type.
/// Example: `Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec())`
pub type HashFn = std::sync::Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static>;

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
