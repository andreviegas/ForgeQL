//! The mutation verbs, one module per verb family.
//!
//! - `raw_text` — `CHANGE FILE` (with the indexed-file gate) and `COPY LINES` / `MOVE LINES`
//! - `change` — `CHANGE NODE`, whole-span or `MATCHING 'a' WITH 'b'`
//! - `insert` — `INSERT NODE FOR '<path>'` and `INSERT BEFORE|AFTER NODE`
//! - `delete` — `DELETE NODE`, including whole-file / whole-directory unlink
//! - `relocate` — `MOVE NODE … BEFORE|AFTER` and `MOVE|COPY NODE … TO`
//! - `found` — the FOUND set and the bulk `… NODES FOUND` verbs
//! - `plan` — the shared plan → apply → reindex → diff pipeline and UNDO
//! - `resolve` — handle → span resolution and the `IF REV` guards

mod change;
mod delete;
mod found;
mod insert;
mod plan;
mod raw_text;
mod relocate;
mod resolve;
