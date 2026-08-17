- Attaching to a commit that arrived from upstream (`REFRESH SOURCE`) no longer
  merges the whole corpus. The attach derives the chain manifest such a commit
  never had — nearest ancestor with an overlay on disk as the master, change set
  read off the two indexes (segment table against this parse's segment map,
  file-only listing against the same worktree walk a full build runs) — writes
  it beside the missing overlay, and serves the commit as a chain: the master
  overlay plus the changed files, exactly the rows a full build serves, `usages`
  included. Ancestry is checked on the commit graph; a sibling branch's overlay
  is never a base, and with no ancestor overlay in reach, or a change set at or
  past `FORGEQL_CHAIN_COMPACT_PATHS`, the full build runs as before.
- `usages` on a session with dirty rows — an uncommitted edit, a chained attach —
  is now the commit's own count. It was the master overlay's aggregate, which
  still counted a replaced file's old sites and none of its new ones. Each row
  is corrected by the sites the dirty overlay shadowed and added, from a table
  built once per dirty state and rebuilt when that state changes.
- A chain attach does not take a delta file it finds in the worktree for its
  own state when that cannot be so: a fresh checkout of a checkpoint tree
  carries the committing session's delta and none of its staging, and a
  worktree fast-forwarded past a chained base carries the older chain's seed.
  Either would leave the attach unseeded and the commit's changed files
  unserved; both are dropped before seeding, and every path they named is
  queued for re-index, which a live worktree drains from disk on reconnect.
- On reconnect, a file the delta loader had queued for re-index — a staged
  edit whose staging segment was gone — was handed to the re-index as a
  workspace-relative path while the git-diff paths beside it were absolute;
  read as given, it was looked for in the wrong directory, taken for a
  deletion, and its rows hidden instead of restored. Queued paths are now
  anchored to the worktree first, and deduplicate against the diff.
- Measured on Zephyr (3,062,139 rows, debug-profile binary, one `USE` per
  process, wall clock of the whole process): the branch head cold — full parse
  and full overlay build — 92 s; a fresh worktree on the branch head with its
  overlay on disk 17 s; a fresh worktree on a child commit one file away,
  served through a derived chain, 21–32 s (the spread is the number of other
  persisted sessions the process restores first). The chain wrote no overlay.
  Not measured in this change: the full build of that same child commit
  (incremental parse plus the whole merge) for a like-for-like attach
  comparison, release-profile numbers on Linux, and query latency on a chained
  session against a full one.
