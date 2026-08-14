- `COPY NODES FOUND TO 'dir/'` and `MOVE NODES FOUND TO 'dir/'` now refuse a
  set in which two members share a basename, naming both sources and the
  destination they collide on. Previously both members targeted the same new
  file and the second silently appended to the first — two unrelated files
  welded into one, reported as success. With every destination claimed exactly
  once, the reported `edit_count` again equals the number of destination files
  a copy creates. A refused statement writes nothing: destinations are now
  validated for every member before any directory is created.
