//! Unit tests for the symbol table and the indexer, grouped by what they cover.
//!
//! - `table` — the table itself: pushing rows, the secondary indexes, lookup,
//!   purge, reindex, and the intern pools
//! - `snippets` — what indexing a snippet actually produces: comment blocks,
//!   error rows, block aliases, and node-id stability across edits
//! - `cpp` — the C++ node kinds the indexer is expected to recognise
//! - `util` — the two snippet-indexing helpers the above share

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

mod cpp;
mod snippets;
mod table;
mod util;
