# `forgeql-core::storage` — Storage abstraction layer

This module defines the two core traits that decouple ForgeQL's query engine
from any concrete storage backend or SCM system.

---

## `StorageEngine`

**File:** `mod.rs`

The `StorageEngine` trait is the MySQL-handler–style boundary between the
`exec_*` dispatch layer and every storage backend.  All symbol queries,
SHOW resolutions, index lifecycle operations, and cache persistence go
through this interface.

```rust
pub trait StorageEngine: Send + Sync {
    fn backend_name(&self) -> &'static str;

    // Read-only queries (called by exec_find / exec_show)
    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>>;
    fn find_usages(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>>;

    // Symbol resolution for SHOW commands
    fn resolve_symbol(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Option<SymbolLocation>>;
    fn resolve_type_symbol(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Option<SymbolLocation>>;
    fn resolve_body_symbol(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Option<SymbolLocation>>;

    // Outline + callees helpers (SHOW outline / SHOW callees)
    fn show_outline_for_file(&self, workspace: &Workspace, file: &str) -> Result<serde_json::Value>;
    fn locate_definition(&self, name: &str) -> Option<PathBuf>;

    // Aggregate
    fn index_stats(&self) -> Option<&IndexStats>;

    // Lifecycle
    fn build(&mut self, workspace: &Workspace) -> Result<()>;
    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()>;
    fn purge_file(&mut self, path: &Path);
    fn persist_to_cache(&self, worktree_path: &Path, commit_hash: &str, source_name: &str) -> Result<()>;
    fn load_from_cache(&mut self, worktree_path: &Path, head_oid: &str, source_name: &str) -> bool;
    fn drop_stored_index(&mut self);
    fn has_index(&self) -> bool;

    // Downcast hatch for legacy code
    fn as_legacy_table(&self) -> Option<&SymbolTable> { None }
    fn as_legacy_table_mut(&mut self) -> Option<&mut SymbolTable> { None }
}
```

`SymbolLocation` carries everything a SHOW command needs: `path`, `byte_range`,
`line`, `node_kind`, and `enrichment` fields extracted from the index row.
The `exec_show` layer passes `SymbolLocation` to the existing `show::*` functions
(which read source bytes from disk and re-parse the AST). Storage only resolves
*where* a symbol lives; the re-parse is the caller's responsibility.

### Implementations

| Struct | Backend | Status |
|---|---|---|
| `LegacyMemoryStorage` (`legacy.rs`) | In-memory `SymbolTable` | Current default (Phase 01+) |
| `StubColumnarStorage` (`stub.rs`) | Returns empty for everything | Trait-shape validator only |
| *(planned)* `ColumnarStorage` | On-disk columnar engine | Phase 03+ |

---

## `SourceProvider`

**File:** `source_provider.rs`

The `SourceProvider` trait abstracts over the SCM system used to identify and
retrieve source content.  It pairs with two supporting traits:

- **`ContentId`** — a content-addressable identifier for a single blob
  (e.g. a 20-byte SHA-1 for git).
- **`SnapshotId`** — identifies a point-in-time snapshot of the whole tree
  (e.g. a git commit OID).

```rust
pub trait SourceProvider: Send + Sync {
    type Content: ContentId;
    type Snapshot: SnapshotId;

    fn provider_id(&self) -> &'static str;
    fn hash_content(&self, bytes: &[u8]) -> Self::Content;
    fn walk_snapshot(&self, snap: &Self::Snapshot)
        -> Result<Box<dyn Iterator<Item = Result<(PathBuf, Self::Content)>> + Send>>;
    fn read_content(&self, id: &Self::Content) -> Result<Vec<u8>>;
    fn current_snapshot(&self, worktree: &Path) -> Result<Self::Snapshot>;
    fn changed_paths(&self, from: &Self::Snapshot, to: &Self::Snapshot)
        -> Result<Vec<PathBuf>> { /* default: full walk diff */ }
}
```

`LegacyMemoryStorage` does **not** use `SourceProvider` — it reads files from
disk during indexing, as before.  The trait exists in Phase 01 so that Phase 03
(columnar engine) can write against the abstraction from day one.

### Implementations

| Struct | Backend | Status |
|---|---|---|
| `GitSha1Provider` (`git_sha1_provider.rs`) | `git2` + SHA-1 blob hashing | Phase 01+ |
| `MockProvider` (`mock_provider.rs`) | In-memory map, FNV-1a hashing | Unit-test helper |

---

## Extending the abstraction

Before adding a method to either trait, check whether `StubColumnarStorage`
or `MockProvider` can implement it with a trivial body.  If they cannot, the
method is too tightly coupled to the legacy model — iterate the trait shape
before merging.

The Phase 01 gate query:

```sql
FIND usages OF 'SymbolTable' IN 'crates/forgeql-core/src/engine/**'
-- must return 0 results

---

## Backend routing — `USING 'backend'` clause

All read-only ForgeQL commands (`FIND symbols`, `FIND usages`, `FIND files`,
`SHOW body`, `SHOW outline`, etc.) accept an optional `USING 'backend'` clause
between the command target and any filtering `clauses`:

```sql
FIND symbols USING 'legacy'  WHERE name LIKE 'get%'
SHOW body OF 'myFn' USING 'columnar'
SHOW LINES 1-20 OF 'src/lib.rs' USING 'legacy'
```

### `Backend` enum (`ir.rs`)

| Variant | Wire name | Meaning |
|---|---|---|
| `Default` | *(omitted)* | Same routing as `Legacy` in the current release |
| `Legacy` | `"legacy"` | Existing `LegacyMemoryStorage` in-memory index |
| `Columnar` | `"columnar"` | `Session::columnar_engine` slot (Phase 03+) |

`Backend::from_clause("unknown")` returns `ForgeError::DslParse` — unknown
names are rejected at parse time, not at query execution time.

### `Session::engine_for`

```rust
pub fn engine_for(&self, backend: &Backend) -> Result<&dyn StorageEngine> {
    match backend {
        Backend::Default | Backend::Legacy => Ok(self.engine.as_ref()),
        Backend::Columnar => self.columnar_engine.as_deref()
            .ok_or_else(|| anyhow::anyhow!("columnar backend is not enabled..."))
    }
}
```

`exec_find` and `exec_show` call `require_workspace_and_engine_for(session_id, backend)`
which calls `session.engine_for(backend)` — all backend routing passes through
this single chokepoint.

### Restricting mutations

Grammar rule `change_stmt` (and `copy_stmt`, `move_stmt`, transaction rules) do
**not** include `using_clause?`.  Passing `USING` on a mutation is a parse
error — storage backends are read-only selectors, not write destinations.
