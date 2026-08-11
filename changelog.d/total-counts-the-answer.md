- The `total` a `FIND symbols` response carries is now the number of rows that
  matched, not the number the page holds. It used to be taken after `OFFSET`
  and `LIMIT` had already cut the page, so under any explicit `LIMIT` the two
  were equal and every first page read as a whole answer:
  `ORDER BY lines DESC LIMIT 2` over seven matching rows answered `total: 2`
  and now answers `total: 7`. The running top-K trim, which discards rows on
  rank while the segments are still being read, now reports how many it dropped
  so those rows are counted too.
- An `ORDER BY … LIMIT k` page can no longer come back holding fewer than `k`
  rows because duplicates collapsed after the page was chosen. Rows agreeing on
  name, kind, path and line are now collapsed per segment before anything sheds
  on rank — both in the running trim and in the bounded choice a segment makes
  over its own row IDs before building any of them — so a group of duplicates
  sorting to the front of the window no longer costs the distinct rows that
  belonged in the answer. Where two segments were built from the same source
  path a duplicate pair can span both, which a per-segment collapse cannot see;
  there the trim is not armed at all rather than shed on an incomplete one.
- Two places still answer with the size of the page. A bare `LIMIT` with no
  `ORDER BY` stops opening segments at `limit + 1`, and `ORDER BY name` with a
  small `LIMIT` streams that many rows out of the name index and stops; neither
  counts what it did not read. Both are pinned as failing cases in the golden
  suite rather than left to be discovered.
