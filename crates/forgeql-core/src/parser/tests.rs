//! Parser unit tests, split by statement family.
//!
//! `parser/mod.rs` declares this module as `#[cfg(test)] mod tests;`, so every
//! child below is test-only without carrying its own `#[cfg(test)]`. Each
//! child reaches the parser through `use crate::parser::*;` — the same set of
//! names this file used to reach through `use super::*;` when it held all 91
//! tests, read from one level deeper.

mod backends;
mod clauses;
mod find;
mod mutations;
mod show;
mod sources;
mod transactions;
