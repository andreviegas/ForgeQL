//! `ForgeQL` core library.
//!
//! The transform engine, AST index, workspace manager, DSL parser, and git
//! integration all live here as a pure library with no async runtime dependency.
//! The server and CLI crates bring in tokio/axum/clap on top of this.

// TODO: remove this allow before 1.0 — all public items must be documented.
#![allow(missing_docs)]
// Tests use unwrap/expect intentionally — the pedantic lint is for library code only.
// unused_results is suppressed in tests because helper functions return git2 objects
// that callers intentionally discard (the side-effect is what matters).
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        unused_results
    )
)]

pub mod ast;
pub mod config;
pub mod context;
pub mod engine;
pub mod error;
pub mod filter;
pub mod git;
pub mod ir;
pub mod parser;
pub mod result;
pub mod session;
pub mod transforms;
pub mod verify;
pub mod workspace;
