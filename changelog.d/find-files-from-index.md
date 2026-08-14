- `FIND files` answers every query shape from the index's own file list —
  the union of indexed segments, non-indexed file entries and the session's
  uncommitted adds, with deletions masked — instead of walking the worktree
  and stat-ing every file on each call. Only the extension-filtered shape
  used the stored list before; the guard dated from when older overlays
  carried source files only, a reason that no longer exists. Directory rows
  are now derived from that same list: one row per directory that has files
  beneath it, `size` the total bytes of those files (previously the same
  directory could appear twice, once with `size` meaning child count and
  once meaning summed bytes). An empty directory created in-session is
  addressable by the handle its creation returned but is not listed. The
  filesystem walk survives only for storage backends that expose no file
  list. On a 95,000-file corpus an unscoped `FIND files` previously burned
  tens of minutes of CPU; the listing itself is now free of filesystem work,
  and what remains is the per-row rev stamping of the rows actually
  returned.
