- A segment now stores its string pool's ids in the order of the bytes they
  name, and `WHERE <field> = '<value>'` resolves the value to an id by binary
  searching that index through the file's mapping. Until now the first
  predicate to touch a segment built an owned hash map of every string in it
  and kept it for as long as the index stayed open, so what a session held grew
  with the number of segments its predicates reached rather than with the
  answer. One `WHERE naming = 'snake_case'` reaches 24,056 of the 32,748
  segments of a three-million-symbol tree: the maps that query used to build
  held 5,721,758 strings and 221 MB of string bytes, and are not built at all
  now. What replaced them was counted on the same query — 197,965 comparisons
  against already-mapped bytes for the whole of it, 8.2 per segment.

  Those maps were per-session heap, which is the kind that multiplies: several
  agents on one corpus each paid for their own. That, rather than the size of
  any single one, is what the change is for.

  Answers are unchanged: the search runs over a sorted permutation of the same
  pool, so it finds the same id, and the same absence where the segment does
  not hold the value. No wall-clock claim is made — the query is bounded by
  what it reads, and the comparison count above is the only cost measured.

  This changes what a segment file holds, so the index generation moves and
  every corpus re-indexes once. A segment written by an older build carries no
  such index and is refused rather than read; none can be reached, since
  segments are cached under a directory named for the index generation.
