//! Build, query and incremental-update methods for [`super::SymbolTable`].
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::ast::enrich::default_enrichers;
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::intern::ColumnarTable;
use crate::ast::lang::LanguageRegistry;
use crate::ast::trigram::TrigramIndex;
use crate::error::ForgeError;
use crate::workspace::Workspace;

use super::file_indexer::{
    IndexContext, OrdinalHint, OrdinalRemapper, collect_macro_defs_for_file, index_file,
    index_file_from_source,
};
use super::{
    IndexRow, IndexStats, MemEstimate, SegmentBuildCtx, SymbolTable, UsageSite, reassign_intern_ids,
};
/// Builder that holds disjoint borrows of all secondary-index fields in
/// `SymbolTable`, enabling `insert` to read `strings` (immutable borrow) while
/// simultaneously mutating the index maps, stats, and trigram index.
///
/// Constructing the builder from individual field borrows (rather than `&mut self`)
/// lets the borrow checker track the accesses as disjoint, which a `&mut self`
/// method cannot do.
struct SecondaryIndexBuilder<'a> {
    name_index: &'a mut HashMap<u32, Vec<u32>>,
    kind_index: &'a mut HashMap<u32, Vec<u32>>,
    fql_kind_index: &'a mut HashMap<u32, Vec<u32>>,
    stats: &'a mut IndexStats,
    trigram_index: &'a mut TrigramIndex,
    strings: &'a ColumnarTable,
}

impl SecondaryIndexBuilder<'_> {
    fn insert(&mut self, row: &IndexRow, idx: u32) {
        self.name_index.entry(row.name_id).or_default().push(idx);
        self.kind_index
            .entry(row.node_kind_id)
            .or_default()
            .push(idx);
        if !self.strings.fql_kinds.get(row.fql_kind_id).is_empty() {
            self.fql_kind_index
                .entry(row.fql_kind_id)
                .or_default()
                .push(idx);
            *self.stats.by_fql_kind.entry(row.fql_kind_id).or_insert(0) += 1;
        }
        if !self.strings.languages.get(row.language_id).is_empty() {
            *self.stats.by_language.entry(row.language_id).or_insert(0) += 1;
        }
        // `get` returns a `&str` borrowed from the pool — zero allocation.
        self.trigram_index
            .insert(idx as usize, self.strings.names.get(row.name_id));
    }
}

impl SymbolTable {
    /// Build a `SymbolTable` by parsing all supported files in the workspace.
    ///
    /// Files are parsed and enriched **in parallel** using rayon.  Each thread
    /// creates its own `Parser` and enricher set, producing a per-file table.
    /// Results are merged sequentially, then post-pass enrichment runs.
    ///
    /// # Errors
    /// Returns `Err` if the tree-sitter language cannot be set.
    #[allow(clippy::too_many_lines)]
    pub fn build(
        workspace: &Workspace,
        lang_registry: &LanguageRegistry,
        seg_ctx: Option<&SegmentBuildCtx>,
    ) -> Result<(Self, MacroTable)> {
        // 1 — collect file paths that have a registered language.
        let paths: Vec<PathBuf> = workspace
            .files()
            .filter(|p| lang_registry.language_for_path(p).is_some())
            .collect();

        debug!(files = paths.len(), "indexing files in parallel");

        // Pass 1 — collect macro definitions (parallel, per-file, then merged).
        // All parallel parse+enrich passes run on one pool built for this build,
        // whose workers have a large stack — AST enrichers recurse over the
        // syntax tree and would otherwise overflow rayon's default ~2 MiB worker
        // stack on deeply nested source (see `build_indexing_pool`).
        // One pool for the whole build, dropped when `build` returns — that is
        // what hands the worker stacks back.
        let index_pool = Self::build_indexing_pool()?;

        let t_build = std::time::Instant::now();
        let t_step = std::time::Instant::now();
        let macro_table = index_pool.install(|| Self::collect_macro_table(&paths, lang_registry));

        info!(
            ms = t_step.elapsed().as_millis(),
            macro_defs = macro_table.def_count(),
            heap = %macro_table.heap_breakdown(),
            mem = %crate::mem::snapshot(), "TIMING build pass1: macro collection"
        );

        // ── Columnar fast-path ─────────────────────────────────────────────
        // When a SegmentBuildCtx is provided, segments are written inline
        // per-file during index_file() (including per-file post_pass).
        // No merge, full-table post_pass, or populate_usage_counts is needed —
        // the columnar engine never queries the SymbolTable after build.
        // This eliminates the ~2-minute sequential bottleneck on large repos.
        if seg_ctx.is_some() {
            let t_fast = std::time::Instant::now();
            index_pool.install(|| {
                Self::build_columnar_segments(
                    &paths,
                    lang_registry,
                    &macro_table,
                    seg_ctx,
                    workspace,
                );
            });
            info!(
                ms = t_fast.elapsed().as_millis(),
                files = paths.len(),
                mem = %crate::mem::snapshot(), "TIMING build total: SymbolTable::build (columnar fast-path, no merge)"
            );
            return Ok((Self::default(), macro_table));
        }

        // Pass 2 — parse + enrich each file in parallel, merging via tree
        // reduction so merges also happen across multiple cores.
        let t_step = std::time::Instant::now();
        let mut table = index_pool.install(|| {
            Self::parse_and_reduce(&paths, lang_registry, &macro_table, seg_ctx, workspace)
        });

        info!(
            ms = t_step.elapsed().as_millis(),
            rows = table.rows.len(),
            mem = %crate::mem::snapshot(), "TIMING build pass2: parse + reduce"
        );

        // Post-pass — run post_pass for each enricher (aggregation, cross-row metrics).
        // `None` scope = process the entire table (full build).
        let t_step = std::time::Instant::now();
        let enrichers = default_enrichers();
        for enricher in &enrichers {
            enricher.post_pass(&mut table, None);
        }
        info!(ms = t_step.elapsed().as_millis(), mem = %crate::mem::snapshot(), "TIMING build post_pass");

        // Precompute per-row usages_count from the completed usages map.
        let t_step = std::time::Instant::now();
        table.populate_usage_counts();
        info!(
            ms = t_step.elapsed().as_millis(),
            rows = table.rows.len(),
            usages = table.usages.values().map(Vec::len).sum::<usize>(),
            mem = %crate::mem::snapshot(), "TIMING build populate_usage_counts"
        );

        info!(
            ms = t_build.elapsed().as_millis(),
            mem = %crate::mem::snapshot(), "TIMING build total: SymbolTable::build"
        );
        Ok((table, macro_table))
    }

    /// Worker threads for an index build's parse + enrich passes.
    ///
    /// Half the machine's cores by default. How many per-file peaks can
    /// overlap is set by how many workers there are, and two index builds
    /// whose peaks coincided drove a 24 GB machine into a 2,824-second swap
    /// event. `FORGEQL_INDEX_THREADS` overrides it; zero, empty or malformed
    /// falls back to the default, as with the other indexing knobs.
    fn index_thread_count() -> usize {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let default = u64::try_from(cores.div_ceil(2)).unwrap_or(1);
        usize::try_from(Self::parse_size_knob(
            std::env::var("FORGEQL_INDEX_THREADS").ok().as_deref(),
            default,
        ))
        .unwrap_or(1)
    }

    /// Build the rayon thread pool for one index run: the parallel parse +
    /// enrich passes, with a large per-worker stack.
    ///
    /// AST enrichers (`casts`, `metrics`, `escape`, `recursion`, `fallthrough`,
    /// `todo`, …) walk the syntax tree recursively. The tree depth of real-world
    /// source — deeply nested expressions, long macro expansions, generated data
    /// tables — can exceed rayon's default ~2 MiB worker stack and abort the whole
    /// process with `fatal runtime error: stack overflow`. Reserving a generous
    /// per-worker stack (virtual address space, paged in on demand) makes indexing
    /// robust to depth without asking users to raise their ambient `ulimit -s`.
    /// Keep that property.
    ///
    /// The pool used to be `'static`, which meant every stack page an enricher
    /// had ever touched stayed mapped for the life of the process — a cost the
    /// machine went on paying long after the build that caused it. Building one
    /// per run hands those pages back when the run ends, so a caller must hold
    /// the pool for the whole run and let it drop at the end.
    ///
    /// `pub(crate)` so the incremental reindex paths (`SymbolTable::reindex_files`
    /// and `ColumnarStorage::reindex_files_impl`) can run their per-file
    /// parse+enrich on the same big-stack workers — they call `index_file` too and
    /// would otherwise overflow the small default stack when a single edited file
    /// is deeply nested.
    /// `pub(crate)` so the incremental reindex paths (`SymbolTable::reindex_files`
    /// and `ColumnarStorage::reindex_files_impl`) can run their per-file
    /// parse+enrich on the same big-stack workers — they call `index_file` too and
    /// would otherwise overflow the small default stack when a single edited file
    /// is deeply nested.
    ///
    /// # Errors
    /// Returns `Err` when the OS refuses to spawn the workers. That used to be a
    /// panic, which was defensible while the pool was built once at first index
    /// and is not now that one is built per run: a transient spawn refusal on a
    /// loaded machine would take down a mutation.
    pub(crate) fn build_indexing_pool() -> Result<rayon::ThreadPool> {
        Self::pool_with_threads(Self::index_thread_count())
    }

    /// A big-stack pool with `threads` workers.
    fn pool_with_threads(threads: usize) -> Result<rayon::ThreadPool> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .stack_size(256 * 1024 * 1024)
            .thread_name(|i| format!("forgeql-index-{i}"))
            .build()
            .context("building the indexing thread pool")
    }

    /// Run `f` on a single big-stack worker and drop the pool when it returns.
    ///
    /// One worker, not [`Self::index_thread_count`], because this is for a
    /// caller that needs the stack and not the parallelism: the incremental
    /// reindex paths walk their files sequentially with one parser and contain
    /// no parallel iterator at all, so a pool sized for the build's passes
    /// would spawn every worker but one to do nothing — on the path every
    /// successful mutation takes. `install` runs the closure on a pool worker
    /// whatever the pool's size, so the stack guarantee is unchanged.
    ///
    /// # Errors
    /// As [`Self::build_indexing_pool`].
    pub(crate) fn with_indexing_pool<R: Send>(f: impl FnOnce() -> R + Send) -> Result<R> {
        Ok(Self::pool_with_threads(1)?.install(f))
    }

    /// Merge another `SymbolTable` into this one.
    ///
    /// Row indices in `name_index` and `kind_index` are offset by the
    /// current row count so they remain correct after the merge.
    fn merge(&mut self, other: Self) {
        let offset = self.rows.len();

        // Merge rows and fix secondary indexes.
        for (i, mut row) in other.rows.into_iter().enumerate() {
            let abs = offset + i;
            debug_assert!(
                u32::try_from(abs).is_ok(),
                "row index exceeds u32::MAX during merge"
            );
            let abs_u32 = u32::try_from(abs).unwrap_or(u32::MAX);
            // Remap IDs: values from `other.strings` are not valid in `self.strings`.
            reassign_intern_ids(&other.strings, &mut self.strings, &mut row);
            SecondaryIndexBuilder {
                name_index: &mut self.name_index,
                kind_index: &mut self.kind_index,
                fql_kind_index: &mut self.fql_kind_index,
                stats: &mut self.stats,
                trigram_index: &mut self.trigram_index,
                strings: &self.strings,
            }
            .insert(&row, abs_u32);
            self.rows.push(row);
        }

        // Merge usage sites — remap path_id from other.strings.paths into self.strings.paths.
        for (name, sites) in other.usages {
            let remapped: Vec<UsageSite> = sites
                .into_iter()
                .map(|s| {
                    let path = other.strings.paths.get(s.path_id);
                    let path_id = self.strings.paths.intern(path);
                    UsageSite { path_id, ..s }
                })
                .collect();
            self.usages.entry(name).or_default().extend(remapped);
        }

        // Mention sites, remapped the same way and kept in their role bucket.
        for (role, by_name) in other.mentions {
            let target = self.mentions.entry(role).or_default();
            for (name, sites) in by_name {
                let remapped: Vec<UsageSite> = sites
                    .into_iter()
                    .map(|s| {
                        let path = other.strings.paths.get(s.path_id);
                        let path_id = self.strings.paths.intern(path);
                        UsageSite { path_id, ..s }
                    })
                    .collect();
                target.entry(name).or_default().extend(remapped);
            }
        }
    }

    /// Append a row and update the secondary indexes.
    ///
    /// The row must have pre-filled `name_id`, `node_kind_id`, `fql_kind_id`,
    /// `language_id`, and `path_id` — set by `table.strings.intern_row()` in
    /// `collect_nodes` before calling this method.
    pub fn push_row(&mut self, row: IndexRow) {
        let index = self.rows.len();
        debug_assert!(
            u32::try_from(index).is_ok(),
            "row index exceeds u32::MAX in push_row"
        );
        let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        SecondaryIndexBuilder {
            name_index: &mut self.name_index,
            kind_index: &mut self.kind_index,
            fql_kind_index: &mut self.fql_kind_index,
            stats: &mut self.stats,
            trigram_index: &mut self.trigram_index,
            strings: &self.strings,
        }
        .insert(&row, index_u32);
        self.rows.push(row);
    }

    /// Rebuild all secondary indexes and stats from `self.rows` in O(N).
    ///
    /// Used after cache load (when the pool is restored from `CachedIndex.strings`)
    /// and after [`purge_file`].  Clears all secondary indexes before rebuilding.
    pub fn rebuild_indexes_from_rows(&mut self) {
        self.name_index.clear();
        self.kind_index.clear();
        self.fql_kind_index.clear();
        self.trigram_index.clear();
        self.stats.by_fql_kind.clear();
        self.stats.by_language.clear();
        for (index, row) in self.rows.iter().enumerate() {
            let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
            SecondaryIndexBuilder {
                name_index: &mut self.name_index,
                kind_index: &mut self.kind_index,
                fql_kind_index: &mut self.fql_kind_index,
                stats: &mut self.stats,
                trigram_index: &mut self.trigram_index,
                strings: &self.strings,
            }
            .insert(row, index_u32);
        }
    }

    // ------------------------------------------------------------------
    // ------------------------------------------------------------------
    // Intern-pool accessors — resolve row IDs to string/path slices.
    // These are zero-copy; the returned references borrow from `self.strings`.
    // ------------------------------------------------------------------

    /// Resolve `row.name_id` to its symbol name.
    #[must_use]
    #[inline]
    pub fn name_of(&self, row: &IndexRow) -> &str {
        self.strings.names.get(row.name_id)
    }

    /// Look up a field value (by string key) in an interned `HashMap<u32, u32>`.
    #[must_use]
    #[inline]
    pub fn field_str<'a>(&'a self, fields: &HashMap<u32, u32>, key: &str) -> Option<&'a str> {
        self.strings.field_str(fields, key)
    }

    /// Convert an interned `HashMap<u32, u32>` back to a human-readable `HashMap<String, String>`.
    #[must_use]
    #[inline]
    pub fn resolve_fields(&self, fields: &HashMap<u32, u32>) -> HashMap<String, String> {
        self.strings.resolve_fields(fields)
    }

    /// Resolve `row.node_kind_id` to its raw tree-sitter node kind.
    #[must_use]
    #[inline]
    pub fn node_kind_of(&self, row: &IndexRow) -> &str {
        self.strings.node_kinds.get(row.node_kind_id)
    }

    /// Resolve `row.fql_kind_id` to its universal FQL kind string.
    #[must_use]
    #[inline]
    pub fn fql_kind_of(&self, row: &IndexRow) -> &str {
        self.strings.fql_kinds.get(row.fql_kind_id)
    }

    /// Resolve `row.language_id` to its language identifier string.
    #[must_use]
    #[inline]
    pub fn language_of(&self, row: &IndexRow) -> &str {
        self.strings.languages.get(row.language_id)
    }

    /// Resolve `row.path_id` to its source file path.
    #[must_use]
    #[inline]
    pub fn path_of(&self, row: &IndexRow) -> &std::path::Path {
        self.strings.paths.get(row.path_id)
    }

    /// Fill `IndexRow::usages_count` for every row from the `usages` map.
    ///
    /// Must be called after both `rows` and `usages` are fully populated.
    /// Skips rows where `usages_count` is already non-zero (idempotent on
    /// indexes built with a version that persists the field).
    pub fn populate_usage_counts(&mut self) {
        for i in 0..self.rows.len() {
            // Extract the bare name suffix (after last `::`) as an owned
            // String to release the immutable borrow on `self.strings`
            // before we look up `self.usages`.
            let usages_key = {
                let n = self.strings.names.get(self.rows[i].name_id);
                n.rsplit("::").next().unwrap_or(n).to_owned()
            };
            let count = self
                .usages
                .get(&usages_key)
                .map_or(0, |v| u32::try_from(v.len()).unwrap_or(u32::MAX));
            self.rows[i].usages_count = count;
        }
    }
    pub fn add_usage(&mut self, name: String, path: &Path, byte_range: Range<usize>, line: usize) {
        let path_id = self.strings.paths.intern(path);
        self.usages.entry(name).or_default().push(UsageSite {
            path_id,
            byte_range,
            line,
        });
    }

    /// Record one mention: `name` was written as a bare identifier token inside
    /// a text-bearing node whose kind maps to `role`.
    ///
    /// `line` is the token's own line, not the enclosing node's — a name in the
    /// middle of a long comment belongs to the line it is written on.
    pub fn add_mention(
        &mut self,
        role: &str,
        name: String,
        path: &Path,
        byte_range: Range<usize>,
        line: usize,
    ) {
        let path_id = self.strings.paths.intern(path);
        self.mentions
            .entry(role.to_owned())
            .or_default()
            .entry(name)
            .or_default()
            .push(UsageSite {
                path_id,
                byte_range,
                line,
            });
    }

    /// Look up all usage sites for a symbol name.
    #[must_use]
    pub fn find_usages(&self, name: &str) -> &[UsageSite] {
        self.usages.get(name).map_or(&[], Vec::as_slice)
    }

    /// Look up the primary definition row for a symbol by name.
    ///
    /// When multiple rows share a name, returns the last-indexed row
    /// (last-write-wins, matching v1 behaviour).
    #[must_use]
    pub fn find_def(&self, name: &str) -> Option<&IndexRow> {
        let id = self.strings.names.get_id(name)?;
        self.name_index
            .get(&id)?
            .last()
            .map(|&idx| &self.rows[idx as usize])
    }

    /// Return all definition rows for a given symbol name.
    ///
    /// Unlike [`find_def`] which returns only the last-indexed row,
    /// this returns every row matching the name — essential for
    /// multi-language workspaces where the same name may exist in
    /// different files/languages.
    #[must_use]
    pub fn find_all_defs(&self, name: &str) -> Vec<&IndexRow> {
        let Some(id) = self.strings.names.get_id(name) else {
            return Vec::new();
        };
        self.name_index.get(&id).map_or_else(Vec::new, |indices| {
            indices
                .iter()
                .map(|&idx| &self.rows[idx as usize])
                .collect()
        })
    }

    /// Return up to `max` symbol names that are similar to `query`.
    ///
    /// Uses case-insensitive prefix matching and substring matching to
    /// find plausible alternatives when a symbol lookup fails.
    #[must_use]
    pub fn suggest_similar(&self, query: &str, max: usize) -> Vec<&str> {
        let lower = query.to_ascii_lowercase();
        let mut results: Vec<&str> = self
            .strings
            .names
            .iter_str()
            .filter(|name| {
                let nl = name.to_ascii_lowercase();
                nl.starts_with(&lower) || lower.starts_with(&nl) || nl.contains(&lower)
            })
            .take(max)
            .collect();
        results.sort_unstable();
        results.truncate(max);
        results
    }

    /// Return an iterator over all rows matching a tree-sitter node kind.
    pub fn rows_by_kind(&self, kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.strings
            .node_kinds
            .get_id(kind)
            .and_then(|id| self.kind_index.get(&id))
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i as usize]))
    }

    /// Return an iterator over all rows matching a universal FQL kind.
    pub fn rows_by_fql_kind(&self, fql_kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.strings
            .fql_kinds
            .get_id(fql_kind)
            .and_then(|id| self.fql_kind_index.get(&id))
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i as usize]))
    }

    /// Return an iterator over all rows with an exact name match.
    ///
    /// O(1) lookup via `name_index`; suitable for wildcard-free `LIKE` and
    /// fully-anchored `MATCHES` predicates.
    pub fn rows_by_name(&self, name: &str) -> impl Iterator<Item = &IndexRow> {
        self.strings
            .names
            .get_id(name)
            .and_then(|id| self.name_index.get(&id))
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i as usize]))
    }

    /// Return candidate rows whose names contain `substr` according to the
    /// trigram index.  The result is a **superset** — callers must still
    /// verify the full predicate.  Returns `None` when `substr` is too short
    /// (< 3 bytes) to use trigrams.
    #[must_use]
    pub fn trigram_candidates(&self, substr: &str) -> Option<Vec<&IndexRow>> {
        let ids = self.trigram_index.candidates(substr)?;
        Some(ids.into_iter().map(|i| &self.rows[i]).collect())
    }

    // -------------------------------------------------------------------
    // Memory diagnostics
    // -------------------------------------------------------------------

    /// Compute a breakdown of approximate heap consumption (in bytes) for
    /// all major components of this `SymbolTable`.
    ///
    /// All figures are **estimates** using `std::mem::size_of` for fixed-size
    /// parts plus per-element heap allocations for `String`, `Vec`, and
    /// `HashMap`.  `HashMap` overhead uses 56 B/bucket as a conservative
    /// approximation for `std::collections::HashMap` on 64-bit platforms.
    #[must_use]
    pub fn mem_estimate(&self) -> MemEstimate {
        // --- rows: Vec<IndexRow> ---
        // Each IndexRow has fixed fields + one HashMap<u32,u32> (fields).
        // After the u32-key/value interning: each entry is 8 bytes + bucket overhead.
        let row_fixed = std::mem::size_of::<IndexRow>(); // byte_range, line, usages_count, ids
        let row_fields_heap: usize = self
            .rows
            .iter()
            .map(|r| {
                // 8 bytes per (u32,u32) entry + ~56 bytes/bucket overhead.
                r.fields.len() * 8 + r.fields.capacity() * 56
            })
            .sum();
        let rows_bytes = self.rows.capacity() * row_fixed + row_fields_heap;

        // --- usages: HashMap<String, Vec<UsageSite>> ---
        // UsageSite is now fully fixed-size (path_id: u32, byte_range, line) — no heap per site.
        let usage_site_fixed = std::mem::size_of::<UsageSite>();
        let usages_bytes: usize = self
            .usages
            .iter()
            .map(|(k, v)| {
                k.capacity() + v.capacity() * usage_site_fixed + 56 // bucket overhead
            })
            .sum::<usize>()
            + self.usages.capacity() * 56;

        // --- mentions: HashMap<role, HashMap<String, Vec<UsageSite>>> ---
        // Folded into the usage figures below rather than reported separately:
        // a mention is an occurrence site of the same shape, and `FIND usages`
        // returns both.
        let mentions_bytes: usize = self
            .mentions
            .iter()
            .map(|(role, by_name)| {
                role.capacity()
                    + by_name
                        .iter()
                        .map(|(k, v)| k.capacity() + v.capacity() * usage_site_fixed + 56)
                        .sum::<usize>()
                    + by_name.capacity() * 56
                    + 56
            })
            .sum::<usize>()
            + self.mentions.capacity() * 56;
        let mention_names: usize = self.mentions.values().map(HashMap::len).sum();
        let mention_sites: usize = self
            .mentions
            .values()
            .flat_map(HashMap::values)
            .map(Vec::len)
            .sum();

        // --- name_index: HashMap<u32, Vec<u32>> ---
        let name_index_bytes: usize = self
            .name_index
            .values()
            .map(|v| v.capacity() * 4 + 24 + 56)
            .sum::<usize>()
            + self.name_index.capacity() * 56;

        // --- kind_index ---
        let kind_index_bytes: usize = self
            .kind_index
            .values()
            .map(|v| v.capacity() * 4 + 24 + 56)
            .sum::<usize>()
            + self.kind_index.capacity() * 56;

        // --- fql_kind_index ---
        let fql_kind_index_bytes: usize = self
            .fql_kind_index
            .values()
            .map(|v| v.capacity() * 4 + 24 + 56)
            .sum::<usize>()
            + self.fql_kind_index.capacity() * 56;

        // --- trigram_index: HashMap<[u8;3], Vec<usize>> ---
        let trigram_bytes: usize = self
            .trigram_index
            .posting_iter()
            .map(|v| v.capacity() * 8 + 24 + 56)
            .sum::<usize>()
            + self.trigram_index.posting_len() * 56;

        // --- strings: ColumnarTable ---
        // StringPool: Vec<String> + HashMap<String,u32>
        let string_pool_bytes = |pool: &crate::ast::intern::StringPool| -> usize {
            pool.iter().map(|s| s.len() + 24).sum::<usize>() // Vec<String> heap
                + pool.len() * 56 // lookup HashMap buckets (key cloned)
                + pool.iter().map(String::len).sum::<usize>() // key copies in lookup
        };
        let path_pool_bytes: usize = {
            let p = &self.strings.paths;
            p.iter().map(|p| p.as_os_str().len() + 24).sum::<usize>()
                + p.len() * 56
                + p.iter().map(|p| p.as_os_str().len()).sum::<usize>()
        };
        let strings_bytes = string_pool_bytes(&self.strings.names)
            + string_pool_bytes(&self.strings.node_kinds)
            + string_pool_bytes(&self.strings.fql_kinds)
            + string_pool_bytes(&self.strings.languages)
            + path_pool_bytes
            + string_pool_bytes(&self.strings.field_keys)
            + string_pool_bytes(&self.strings.field_values);

        MemEstimate {
            rows_bytes,
            rows_count: self.rows.len(),
            usages_bytes: usages_bytes + mentions_bytes,
            usages_symbols: self.usages.len() + mention_names,
            usages_sites: self.usages.values().map(Vec::len).sum::<usize>() + mention_sites,
            name_index_bytes,
            kind_index_bytes,
            fql_kind_index_bytes,
            trigram_bytes,
            trigram_entries: self.trigram_index.posting_len(),
            strings_bytes,
            strings_names: self.strings.names.len(),
            strings_paths: self.strings.paths.len(),
        }
    }

    // -------------------------------------------------------------------
    // Incremental update
    // -------------------------------------------------------------------

    /// Remove all entries associated with `path` and rebuild secondary indexes.
    pub fn purge_file(&mut self, path: &Path) {
        let path_id = self.strings.paths.get_id(path);
        if let Some(pid) = path_id {
            self.rows.retain(|row| row.path_id != pid);
        }

        // Rebuild secondary indexes from scratch.
        self.rebuild_indexes_from_rows();

        if let Some(pid) = path_id {
            for sites in self.usages.values_mut() {
                sites.retain(|usage| usage.path_id != pid);
            }
            for by_name in self.mentions.values_mut() {
                for sites in by_name.values_mut() {
                    sites.retain(|mention| mention.path_id != pid);
                }
                by_name.retain(|_, sites| !sites.is_empty());
            }
        }
        self.usages.retain(|_, sites| !sites.is_empty());
        self.mentions.retain(|_, by_name| !by_name.is_empty());
    }
    /// # Errors
    /// Returns an error if parsing fails for any of the provided paths.
    pub fn reindex_files(
        &mut self,
        paths: &[PathBuf],
        lang_registry: &LanguageRegistry,
        workspace_root: Option<&Path>,
    ) -> Result<()> {
        // Run the per-file parse+enrich on the big-stack indexing pool: `index_file`
        // walks the AST recursively and a single deeply-nested edited file would
        // otherwise overflow rayon's default ~2 MiB stack. The full build already
        // does this (see `build_indexing_pool`); the incremental path needs it too.
        Self::with_indexing_pool(|| self.reindex_files_inner(paths, lang_registry, workspace_root))?
    }

    fn reindex_files_inner(
        &mut self,
        paths: &[PathBuf],
        lang_registry: &LanguageRegistry,
        workspace_root: Option<&Path>,
    ) -> Result<()> {
        let mut parser = tree_sitter::Parser::new();
        let enrichers = default_enrichers();

        for path in paths {
            let remapper = {
                let mut hints = Vec::new();
                if let Some(pid) = self.strings.paths.get_id(path) {
                    for row in self.rows.iter().filter(|row| row.path_id == pid) {
                        let Some(ordinal) = row.ordinal else {
                            continue;
                        };
                        let fields = self.resolve_fields(&row.fields);
                        hints.push(OrdinalHint {
                            name: self.name_of(row).to_string(),
                            fql_kind: self.fql_kind_of(row).to_string(),
                            parent_ordinal: row.parent_ordinal,
                            guard_group_id: fields.get("guard_group_id").cloned(),
                            guard_branch: fields.get("guard_branch").cloned(),
                            first_body_statement_fingerprint: fields
                                .get("first_body_statement_fingerprint")
                                .cloned(),
                            content_hash: fields.get("content_hash").cloned(),
                            ordinal,
                        });
                    }
                }
                OrdinalRemapper::from_previous(hints)
            };

            self.purge_file(path);
            if path.exists() {
                if let Some(lang) = lang_registry.language_for_path(path) {
                    parser
                        .set_language(&lang.tree_sitter_language())
                        .map_err(|e| ForgeError::TreeSitterLanguage(e.to_string()))?;
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: None,
                        ordinal_remapper: Some(remapper),
                        table: &mut *self,
                        workspace_root,
                    };
                    match index_file(&mut parser, &mut ctx, None) {
                        Ok(count) => {
                            debug!(path = %path.display(), rows = count, "reindexed");
                        }
                        Err(err) => {
                            warn!(path = %path.display(), error = %err, "reindex failed");
                        }
                    }
                } else {
                    debug!(path = %path.display(), "purged (unsupported language)");
                }
            } else {
                debug!(path = %path.display(), "purged (file deleted)");
            }
        }
        // Run post_pass for each enricher, scoped to the changed paths.
        // This makes incremental re-indexing O(P) instead of O(N) — on
        // Zephyr (2.7M symbols) it turns ~17s of CHANGE-time post_pass
        // overhead into milliseconds.
        let scope: std::collections::HashSet<std::path::PathBuf> = paths.iter().cloned().collect();
        for enricher in &enrichers {
            enricher.post_pass(self, Some(&scope));
        }
        Ok(())
    }
}

impl SymbolTable {
    /// Pass 1: collect macro definitions across all files in parallel, merged
    /// via tree reduction. Files without a macro expander contribute nothing.
    fn collect_macro_table(paths: &[PathBuf], lang_registry: &LanguageRegistry) -> MacroTable {
        paths
            .par_iter()
            .filter_map(|path| {
                let lang = lang_registry.language_for_path(path)?;
                let _ = lang.macro_expander()?;
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    return None;
                }
                match collect_macro_defs_for_file(&mut parser, path, lang.as_ref()) {
                    Ok(defs) if !defs.is_empty() => {
                        let mut local = MacroTable::new();
                        for def in defs {
                            local.insert(def);
                        }
                        Some(local)
                    }
                    _ => None,
                }
            })
            .reduce(MacroTable::new, |mut acc, local| {
                acc.merge_from(local);
                acc
            })
    }

    /// Parse and emit one file into the columnar segment store: segments are
    /// written inline during `index_file` (including per-file `post_pass`), so
    /// no merge / full-table `post_pass` / usage-count population is needed —
    /// the columnar engine never queries the in-memory `SymbolTable` after build.
    fn index_columnar_file(
        path: &Path,
        lang_registry: &LanguageRegistry,
        macro_table: &MacroTable,
        seg_ctx: Option<&SegmentBuildCtx>,
        workspace: &Workspace,
    ) {
        let Some(lang) = lang_registry.language_for_path(path) else {
            return;
        };
        let source = match crate::workspace::file_io::read_bytes(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(path = %path.display(), "skipping file: {e}");
                return;
            }
        };
        // Pre-parse reuse: hash the raw bytes; if this exact content already
        // has a valid segment on disk, register it for the overlay build and
        // skip the parse. Sound here because this path discards the per-file
        // table — the segment is the only output.
        if let Some(seg) = seg_ctx {
            let content_id = (seg.hash_fn)(&source);
            if (seg.reuse_fn)(path, &content_id) {
                debug!(path = %path.display(), "segment reuse: parse skipped");
                return;
            }
        }
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&lang.tree_sitter_language()).is_err() {
            warn!(path = %path.display(), "columnar fast-path: failed to set language");
            return;
        }
        let enrichers = default_enrichers();
        let mut file_table = Self::default();
        let mut ctx = IndexContext {
            path,
            language: lang.as_ref(),
            enrichers: &enrichers,
            macro_table: Some(macro_table),
            ordinal_remapper: None,
            table: &mut file_table,
            workspace_root: Some(workspace.root()),
        };
        match index_file_from_source(&mut parser, &mut ctx, seg_ctx, &source) {
            Ok(count) => {
                debug!(path = %path.display(), rows = count, "indexed (columnar fast-path)");
            }
            Err(e) => warn!(path = %path.display(), "skipping file: {e}"),
        }
        // file_table dropped here — no merge needed for columnar.
    }

    /// Parse a positive integer knob; zero, empty, or malformed falls back to
    /// the default so a bad environment value can never disable indexing.
    fn parse_size_knob(raw: Option<&str>, default: u64) -> u64 {
        raw.and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    }

    /// File size (bytes) at or above which a file is parsed on the bounded
    /// queue instead of the fully parallel pool. `FORGEQL_BIG_FILE_MB`
    /// overrides the default of 4 MB.
    fn big_file_threshold_bytes() -> u64 {
        Self::parse_size_knob(std::env::var("FORGEQL_BIG_FILE_MB").ok().as_deref(), 4)
            .saturating_mul(1024 * 1024)
    }

    /// Number of workers draining the big-file queue. `FORGEQL_BIG_FILE_SLOTS`
    /// overrides the default of 2.
    fn big_file_slots() -> usize {
        usize::try_from(Self::parse_size_knob(
            std::env::var("FORGEQL_BIG_FILE_SLOTS").ok().as_deref(),
            2,
        ))
        .unwrap_or(2)
    }

    /// One filesystem-metadata pass: split `paths` into (big, small) at
    /// `threshold` bytes. Big files are sorted largest-first so the most
    /// expensive parse starts earliest and the queue never ends on a straggler.
    fn partition_by_size(paths: &[PathBuf], threshold: u64) -> (Vec<&PathBuf>, Vec<&PathBuf>) {
        let mut big: Vec<(u64, &PathBuf)> = Vec::new();
        let mut small: Vec<&PathBuf> = Vec::new();
        for path in paths {
            let size = std::fs::metadata(path).map_or(0, |m| m.len());
            if size >= threshold {
                big.push((size, path));
            } else {
                small.push(path);
            }
        }
        big.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        (big.into_iter().map(|(_, p)| p).collect(), small)
    }

    /// Pass 2 (columnar): parse + emit per-file segments with size-aware
    /// admission.
    ///
    /// Peak indexing memory is dominated by parse trees, whose size is
    /// proportional to file size — running every file at full parallelism
    /// keeps one huge tree alive per CPU and can exhaust RAM on corpora with
    /// many large files. Small files keep full parallelism; files at or above
    /// the size threshold drain a dedicated largest-first queue with a bounded
    /// number of workers, so at most `slots` big parse trees exist at once.
    /// Both lanes run concurrently on the same pool via `rayon::join`.
    fn build_columnar_segments(
        paths: &[PathBuf],
        lang_registry: &LanguageRegistry,
        macro_table: &MacroTable,
        seg_ctx: Option<&SegmentBuildCtx>,
        workspace: &Workspace,
    ) {
        let (big, small) = Self::partition_by_size(paths, Self::big_file_threshold_bytes());
        let slots = Self::big_file_slots().min(big.len());
        if !big.is_empty() {
            info!(
                big_files = big.len(),
                slots, "size-aware indexing: large files on a bounded queue"
            );
        }
        rayon::join(
            || {
                let next = AtomicUsize::new(0);
                rayon::scope(|s| {
                    for _ in 0..slots {
                        s.spawn(|_| {
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some(path) = big.get(i) else { break };
                                Self::index_columnar_file(
                                    path,
                                    lang_registry,
                                    macro_table,
                                    seg_ctx,
                                    workspace,
                                );
                            }
                        });
                    }
                });
            },
            || {
                small.par_iter().for_each(|path| {
                    Self::index_columnar_file(path, lang_registry, macro_table, seg_ctx, workspace);
                });
            },
        );
    }

    /// Pass 2: parse + enrich each file in parallel into a per-file table, then
    /// merge via tree reduction so merges also spread across cores.
    fn parse_and_reduce(
        paths: &[PathBuf],
        lang_registry: &LanguageRegistry,
        macro_table: &MacroTable,
        seg_ctx: Option<&SegmentBuildCtx>,
        workspace: &Workspace,
    ) -> Self {
        paths
            .par_iter()
            .filter_map(|path| {
                let lang = lang_registry.language_for_path(path)?;
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    warn!(path = %path.display(), "failed to set tree-sitter language");
                    return None;
                }
                let enrichers = default_enrichers();
                let mut file_table = Self::default();
                {
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: Some(macro_table),
                        ordinal_remapper: None,
                        table: &mut file_table,
                        workspace_root: Some(workspace.root()),
                    };
                    match index_file(&mut parser, &mut ctx, seg_ctx) {
                        Ok(count) => {
                            debug!(
                                path = %workspace.relative(path).display(),
                                rows = count,
                                "indexed"
                            );
                        }
                        Err(err) => {
                            warn!(path = %path.display(), error = %err, "skipping file");
                            return None;
                        }
                    }
                }
                Some(file_table)
            })
            .reduce(Self::default, |mut acc, file_table| {
                acc.merge(file_table);
                acc
            })
    }
}
#[cfg(test)]
impl SymbolTable {
    /// Test helper: intern string fields and append a row.
    #[allow(clippy::too_many_arguments)]
    pub fn push_row_strings(
        &mut self,
        name: &str,
        node_kind: &str,
        fql_kind: &str,
        language: &str,
        path: &std::path::Path,
        byte_range: std::ops::Range<usize>,
        line: usize,
        fields: HashMap<String, String>,
    ) {
        let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = self
            .strings
            .intern_row(name, node_kind, fql_kind, language, path);
        let fields = self.strings.intern_fields(fields);
        self.push_row(IndexRow {
            name_id,
            node_kind_id,
            fql_kind_id,
            language_id,
            path_id,
            byte_range,
            line,
            usages_count: 0,
            ordinal: None,
            parent_ordinal: u32::MAX,
            rev: 0,
            fields,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_knob_falls_back_on_bad_input() {
        assert_eq!(SymbolTable::parse_size_knob(None, 4), 4);
        assert_eq!(SymbolTable::parse_size_knob(Some(""), 4), 4);
        assert_eq!(SymbolTable::parse_size_knob(Some("abc"), 4), 4);
        assert_eq!(SymbolTable::parse_size_knob(Some("0"), 4), 4);
        assert_eq!(SymbolTable::parse_size_knob(Some("-3"), 4), 4);
        assert_eq!(SymbolTable::parse_size_knob(Some(" 16 "), 4), 16);
    }

    #[test]
    fn partition_by_size_splits_and_sorts_largest_first() {
        let dir = std::env::temp_dir().join(format!("fql-partition-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, bytes: usize| {
            let p = dir.join(name);
            std::fs::write(&p, vec![b'x'; bytes]).unwrap();
            p
        };
        let paths = vec![
            mk("small_a.rs", 10),
            mk("big_mid.arxml", 2_000),
            mk("small_b.rs", 500),
            mk("big_top.arxml", 5_000),
        ];
        let (big, small) = SymbolTable::partition_by_size(&paths, 1_000);
        assert_eq!(
            big.iter()
                .map(|p| p.file_name().unwrap())
                .collect::<Vec<_>>(),
            ["big_top.arxml", "big_mid.arxml"]
        );
        assert_eq!(small.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn partition_by_size_missing_file_counts_as_small() {
        let paths = vec![PathBuf::from("/nonexistent/fql-partition-test-ghost.rs")];
        let (big, small) = SymbolTable::partition_by_size(&paths, 1);
        assert!(big.is_empty());
        assert_eq!(small.len(), 1);
    }
}
