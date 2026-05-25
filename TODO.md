# ForgeQL Refactoring Roadmap

Tracking all structural improvements identified on 2026-05-25.
One commit per step. Each step must pass `VERIFY build 'test-all-before-commit'` before commit.

---

## Phase 1 — Parameter Clustering (Struct Grouping)

Eliminate functions with 6+ parameters by grouping related arguments into
purpose-built structs. Do these first — they make the file-split step (Phase 2)
cleaner because moved functions carry smaller signatures.

### P1-A — `ShowRequest<'_>` struct  [ ]
**Files:** `crates/forgeql-core/src/ast/show/body.rs`, `callees.rs`, `show.rs` (signature), `crates/forgeql-core/src/engine/exec_show.rs`

The following 7 parameters are repeated verbatim across `show_body` (9 params),
`show_callees` (7 params), `show_signature` (7 params), and `show_members` (7 params):

```
cached: &CachedParse,
path: &Path,
byte_range_start: usize,
hint_line: Option<usize>,
workspace: &Workspace,
symbol: &str,
lang_registry: &LanguageRegistry,
```

**Action:**
1. Create `crates/forgeql-core/src/ast/show/request.rs` with `pub struct ShowRequest<'a>` holding those 7 fields.
2. Update all four `show_*` functions to accept `req: &ShowRequest<'_>` + variant-specific extras only:
   - `show_body(req, depth, enrichment)`
   - `show_callees(req)`
   - `show_signature(req)`
   - `show_members(req)`
3. Update `exec_show.rs` to build one `ShowRequest` per call site and pass it down.

---

### P1-B — `IndexContext<'_>` struct  [ ]
**Files:** `crates/forgeql-core/src/ast/index.rs`

`collect_nodes` (8 params) and `index_file` (7 params) share:

```
path: &Path,
language: &dyn LanguageSupport,
enrichers: &[Box<dyn NodeEnricher>],
macro_table: Option<&MacroTable>,
table: &mut SymbolTable,
source: &[u8],
```

**Action:**
1. Create `struct IndexContext<'a>` in `ast/index.rs` (or a new `ast/index/context.rs`).
2. Refactor `collect_nodes` (8 → 2 params) and `index_file` (7 → 3 params).

---

### P1-C — `SecondaryIndexBuilder` struct  [ ]
**Files:** `crates/forgeql-core/src/ast/index.rs`

`index_row_into_secondaries` receives 8 params, 5 of which are separate mutable
structures that belong together:

```
name_index:     &mut HashMap<u32, Vec<u32>>,
kind_index:     &mut HashMap<u32, Vec<u32>>,
fql_kind_index: &mut HashMap<u32, Vec<u32>>,
stats:          &mut IndexStats,
trigram_index:  &mut TrigramIndex,
```

**Action:**
1. Create `struct SecondaryIndexBuilder` owning those 5 fields.
2. Replace the free function with a method: `builder.insert(row: &IndexRow, idx: u32)`.
3. Update all call sites in `build()` / `collect_nodes()`.

---

### P1-D — `EscapeLocals<'_>` + `EscapeAccumulator` structs  [ ]
**Files:** `crates/forgeql-core/src/ast/enrich/escape.rs`

`check_expr_escape` (9 params) mixes two unrelated groups:

```
// read-only inputs
local_names:   &HashSet<&str>,
array_locals:  &HashSet<String>,
static_locals: &HashSet<String>,
alias_map:     &HashMap<String, String>,

// mutable accumulators (outputs being built)
escaping:   &mut Vec<String>,
best_tier:  &mut u8,
kinds_seen: &mut HashSet<&str>,
```

**Action:**
1. Create `struct EscapeLocals<'a>` for the 4 read-only fields.
2. Create `struct EscapeAccumulator` for the 3 mutable fields.
3. Signature becomes: `fn check_expr_escape(node, ctx, locals: &EscapeLocals<'_>, acc: &mut EscapeAccumulator)`.

---

### P1-E — Reuse `SessionCoords` in `Session::new`  [ ]
**Files:** `crates/forgeql-core/src/session/mod.rs`, `crates/forgeql-core/src/engine/exec_source.rs`

`Session::new` takes `id`, `user_id`, `source_name`, `branch` as 4 separate strings,
but `SessionCoords` (already used in `use_source`) holds exactly those four fields.

**Action:**
1. Add a `Session::from_coords(coords: SessionCoords, worktree_path: PathBuf, lang_registry: &Arc<LanguageRegistry>) -> Self` constructor.
2. Update `use_source` to call `Session::from_coords` instead of `Session::new`.
3. Keep `Session::new` as a thin wrapper or deprecate it.

---

### P1-F — Replace `field_to_kinds_for_config` match with a lookup map  [ ]
**Files:** `crates/forgeql-core/src/storage/legacy/prefilter.rs`

`field_to_kinds_for_config` is a 214-line function with 72 hardcoded string literals
mapping enrichment field names to `LanguageConfig` method calls. It is a lookup
table disguised as code.

**Action:**
1. Build a `HashMap<&'static str, fn(&LanguageConfig) -> Vec<String>>` once (lazy_static
   or `OnceLock`) mapping each field name to the appropriate config accessor closure.
2. Replace the 214-line match with a 3-line map lookup.
3. The `field_to_kinds` aggregator above it stays unchanged.

---

## Phase 2 — File Splitting (Module Decomposition)

Break monolith files into module folders. Each item below is one commit.
Do Phase 1 first so moved functions carry cleaner signatures.

### P2-A — Split `exec_show.rs` into per-variant helpers  [ ]
**Files:** `crates/forgeql-core/src/engine/exec_show.rs` (383 lines)

The `exec_show` function (383 lines, 43 string literals) is a single `match op { … }`
where each arm is 15–30 lines of resolve + read + format logic.

**Action:**
1. Extract each match arm into a private `fn exec_show_<variant>(&self, …)` method.
2. Result: `exec_show` itself shrinks to ~30 lines (pure dispatcher).
3. Optionally move each helper to a subfile:
   `engine/exec_show/body.rs`, `outline.rs`, `callees.rs`, `lines.rs`, `files.rs`.

---

### P2-B — Split `build_and_persist` into named steps  [ ]
**Files:** `crates/forgeql-core/src/storage/columnar/overlay_builder.rs` (486 lines, 44 branches)

The function already has numbered comment steps (// 1. // 2. // 2.5. // 3. …).
Each step is a self-contained `&self → Result<T>` operation.

**Action:**
1. Extract each numbered step into a private method:
   - `fn open_segments_parallel(&self) -> Result<Vec<(PathBuf, String, SegmentReader)>>`
   - `fn collect_file_only_entries(&self, indexed: &HashSet<PathBuf>) -> Vec<FileOnlyEntry>`
   - `fn compute_row_offsets(segs: &[…]) -> Result<Vec<u32>>`
   - `fn build_global_row_table(segs, offsets) -> Vec<RowPtr>`
   - … (one per numbered comment block)
2. `build_and_persist` becomes a ~40-line orchestrator calling these helpers in order.
3. Branch count drops from 44 to ~4 in the orchestrator.

---

### P2-C — Split `columnar_storage.rs` into a module folder  [ ]
**Files:** `crates/forgeql-core/src/storage/columnar/columnar_storage.rs` (100 KB)

**Target layout:**
```
storage/columnar/columnar_storage/
    mod.rs           ← struct definition + StorageEngine impl wiring (~100 lines)
    query.rs         ← find_symbols pipeline, stages 1–5 (~300 lines)
    fast_paths.rs    ← fast_group_by_*, order_by_name_fast_path (~150 lines)
    commit.rs        ← commit_dirty*, flush_delta, drop_stored_index (~250 lines)
    resolve.rs       ← resolve_impl, find_usages (~280 lines)
```

Further split `find_symbols` (268 lines) into private helpers:
`prefilter_global`, `group_by_segment`, `materialize_all`, `apply_clauses`.

---

### P2-D — Split `lang.rs` into per-language submodules  [ ]
**Files:** `crates/forgeql-core/src/ast/lang.rs` (81 KB, 3 language `config()` functions)

**Target layout:**
```
ast/lang/
    mod.rs    ← LanguageConfig, LanguageRegistry, LanguageSupport trait, FQL_* constants
    cpp.rs    ← CppLanguageInline::config()  (was ~line 1672)
    rust.rs   ← RustLanguage::config()       (was ~line 1823)
    python.rs ← PythonLanguage::config()     (was ~line 1947)
```

Alternative (longer-term): move static grammar data to embedded TOML/JSON
files loaded at registry init — adding a new language becomes a data change,
not a code change.

---

### P2-E — Split `ast/index.rs` into a module folder  [ ]
**Files:** `crates/forgeql-core/src/ast/index.rs` (77 KB)

`collect_nodes` (255 lines, 33 branches) and `build` (164 lines) are the main offenders.

**Target layout:**
```
ast/index/
    mod.rs        ← SymbolTable struct, public API
    build.rs      ← build(), index_file(), collect_nodes()
    secondaries.rs ← SecondaryIndexBuilder (from P1-C), index_row_into_secondaries()
    query.rs      ← find_rows(), row lookups
```

---

### P2-F — Move test data out of test code  [ ]
**Files:**
- `crates/forgeql/tests/parity_find.rs` — `corpus()` 616 lines, 717 string literals
- `crates/forgeql/tests/zephyr_golden.rs` — `golden_values()` 321 lines, 57 string literals
- `crates/forgeql-core/tests/sms_integration.rs` — `sms_combinatorial()` 203 lines

**Action:**
1. Convert each inline data function into an external file:
   `tests/test-data/parity_corpus.toml`, `zephyr_golden.toml`, `sms_cases.toml`.
2. Replace the inline function body with `include_str!` + TOML deserialization.
3. Reduces test LOC by ~1 100 lines; improves incremental compile times.

---

## Phase 3 — Enforce Limits (Prevent Regression)

### P3-A — Add Clippy size thresholds to `.clippy.toml`  [ ]

```toml
too-many-lines-threshold = 60
cognitive-complexity-threshold = 25
too-many-arguments-threshold = 5
```

These are warnings, not errors — they create a feedback loop that stops
accumulation without breaking CI on existing violations during the migration.
Tighten to errors once Phase 1 and Phase 2 are complete.

---

## Completion Checklist

| Step | Description | Status |
|------|-------------|--------|
| P1-A | `ShowRequest<'_>` struct | [ ] |
| P1-B | `IndexContext<'_>` struct | [ ] |
| P1-C | `SecondaryIndexBuilder` struct | [ ] |
| P1-D | `EscapeLocals` + `EscapeAccumulator` | [ ] |
| P1-E | `Session::from_coords` | [ ] |
| P1-F | `field_to_kinds` lookup map | [ ] |
| P2-A | Split `exec_show.rs` | [ ] |
| P2-B | Split `build_and_persist` | [ ] |
| P2-C | Split `columnar_storage.rs` | [ ] |
| P2-D | Split `lang.rs` | [ ] |
| P2-E | Split `ast/index.rs` | [ ] |
| P2-F | Move test data to files | [ ] |
| P3-A | Clippy size thresholds | [ ] |