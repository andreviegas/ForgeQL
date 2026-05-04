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

pub mod segment_builder;
pub mod shadow_writer;

pub use segment_builder::SegmentBuilder;
pub use shadow_writer::ShadowWriter;

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
