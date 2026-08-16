- Indexing the same file twice now produces the same segment bytes. The
  enrichment columns were walked in hash order, and because a string column's
  values are interned into the segment's string pool as the column is walked,
  every run assigned different ids — 3,237 of 3,240 segments differed between
  two cold indexes of one C corpus; it is now 34. Answers are unchanged: the
  overlay checksum is identical across the fix.
- The enrichment version moves to 69, so cached segments and overlays are
  re-keyed and every workspace re-indexes once on upgrade.
