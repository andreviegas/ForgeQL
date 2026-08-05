//! The source and session verbs, split by what they act on.
//!
//! - [`admin`] — `CREATE SOURCE`, `REFRESH SOURCE`, `VACUUM`: the registry and
//!   stored index data.
//! - [`attach`] — the `USE` pipeline, which creates or resumes a session.
//! - [`readouts`] — `SHOW SOURCES` / `BRANCHES` / `COMMITS` / `VERSION` /
//!   `STATS`, which report engine state rather than acting on it.
//!
//! Each child carries its own `impl ForgeQLEngine` block. Inherent methods
//! resolve through the type rather than the module path, so nothing here needs
//! re-exporting for the dispatcher in `engine.rs` to reach them.

mod admin;
mod attach;
mod readouts;
