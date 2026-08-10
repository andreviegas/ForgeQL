- An index that names a segment the disk no longer holds no longer serves rows
  through it. The readers are addressed by position, so a reader that would not
  open did not merely remove that file's rows: it shifted every later position,
  and rows were then served against a different file's reader — the right name
  and line under another file's path and content, and node handles addressing a
  file the query never named. An answer, not an error, and wrong. The state is
  reachable without anything exotic — an external cleaner, or a reclaim that
  removed a segment file while leaving the index that names it — though
  ForgeQL's own `VACUUM` cannot produce it, since it removes whole cache-version
  directories, index and segments together.
- What happens instead depends on whether the segment can be written again.
  Rebuilding an index by shadow-writing from a merged symbol table can write a
  segment that is absent, so where one is available the rebuild is allowed to
  run — and the open then checks that the segment is really there before
  answering from it. That check is what makes this safe rather than hopeful: a
  rebuild skips a segment whose first few bytes still look right even when
  opening it fails, a failed write leaves nothing behind, and the assembly step
  that ends every rebuild drops what it cannot read. So the index is repaired,
  or the open refuses; it is never quietly one file smaller.
- A rebuild that could only assemble the segments already on disk — which is
  what runs whenever the caller carries an inline set of them — cannot write a
  missing one back, and refuses without running. Having *a* rebuild available is
  not enough; it has to be one that can write the segment.
- A refusal names the index, the file, where the open looked for it and why it
  would not open. A readable index file is never deleted on the strength of a
  missing segment, in either direction — removing one cannot be undone, and the
  repairing rebuild replaces it atomically without needing it gone. An index
  file that is itself unreadable is still removed and rebuilt, exactly as before.
- A refused open caches nothing, so putting the missing segment back by hand is
  enough for the very next open to succeed, without a restart.
- Two boundaries this does not cross. A session whose commit is in this state
  still answers completely and does not report the refusal: the columnar open
  declines, the refusal is logged, and the session falls back to its full
  in-memory index. And assembling an index from the segments on disk still drops
  one it cannot read wherever that assembly is reached other than through an
  open — the open is guarded by the check above, nothing else is.
