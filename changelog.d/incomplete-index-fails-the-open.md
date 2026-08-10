- An index that names a segment the disk no longer holds now fails to open,
  naming the missing file and both repairs, instead of opening without it. The
  readers are addressed by position, so a reader that would not open did not
  merely remove that file's rows: it shifted every later position, and rows were
  then served against a different file's reader — the right name and line under
  another file's path and content, and node handles addressing a file the query
  never named. An answer, not an error, and wrong. The state is reachable
  without anything exotic — an external cleaner, or a reclaim that removed a
  segment file while leaving the index that names it — though ForgeQL's own
  `VACUUM` cannot produce it, since it removes whole cache-version directories,
  index and segments together.
- An index file that is itself unreadable is still removed and rebuilt, exactly
  as before. Only the case where the index reads cleanly and a segment under it
  is gone refuses. Removing a readable index is destructive and cannot be
  undone, and a rebuild is not a dependable repair to route into unasked:
  rebuilding from a merged symbol table does regenerate a segment that is
  absent, but rebuilding from the segments already on disk drops one it cannot
  read and writes a smaller index that never says it is smaller. So the choice
  is named and left to the operator rather than taken on their behalf.
- Refusing rather than rebuilding is what keeps the state recoverable: the index
  file survives, nothing is cached from a refused open, and restoring the
  missing segment is enough for the very next open to succeed. Had the refusal
  been treated as a corrupt index, the file would have been deleted first and
  the same repair would have recovered nothing.
- Two boundaries this does not cross. A session whose commit is in this state
  still answers completely and does not report the refusal: the columnar open
  declines, the refusal is logged, and the session falls back to its full
  in-memory index. And a rebuild reached any other way still drops a segment it
  cannot read and writes a smaller index without saying so.
