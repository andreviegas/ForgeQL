- A bounded `FIND symbols` no longer builds a result row for every row it
  matches. A query carrying a `LIMIT` — written, or the 20-row default the
  engine now hands it — with no `GROUP BY` and no `HAVING` travels as 48-byte
  row views read straight from the segment columns: filtered, collapsed on
  `(name, fql_kind, line)`, ranked and cut to the page, with a result row built
  only for the `LIMIT + OFFSET` that survive. Every segment is still read,
  tested and counted, and `total` is still the number of rows that matched.

  Measured on a 3.06-million-symbol corpus, against the same index in the same
  run (release optimisation with debug assertions):

  | query | before | after |
  |---|---|---|
  | `FIND symbols IN 'drivers/**' LIMIT 5000` | 4,317 ms, 1,365 MB private heap | 411 ms, 142 MB |
  | `FIND symbols WHERE fql_kind = 'if'` | 463 ms, 134 MB | 55 ms, 133 MB |

  A session costs 131 MB once its index is open, so the first query's own heap
  fell from 1,234 MB to 11 MB.

  A second harness, ten distinct queries per class against the same corpus, put
  the time change at 5.34x and 5.61x on two runs of a query filtering by an
  enrichment field (1,947–2,002 ms per query before, 347–375 ms after), and
  below its own 200 ms resolution floor on a query filtering by a core field
  (762–820 ms before).

  **What did not move.** `FIND symbols WHERE name LIKE 'k_%' ORDER BY name ASC
  LIMIT 20000` and a bare `FIND symbols` are answered by the name index and
  never built the rows this avoids: 1.01x and unchanged. A three-session
  measurement on one index is unchanged, sharing intact. And nothing moves for a
  query no row view can page — a `GROUP BY`, a `HAVING`, a regular-expression
  predicate, an ordering or a predicate naming `usages`, `node_id` or `count`,
  or an index holding two segments built from one source path. Those still build
  every matching row, and the same 2 GiB result budget still refuses them.

  Two shapes that were outside every bound are now inside one. A `LIMIT` above
  1,000 and a `LIMIT` beside an `OFFSET` could neither arm the running trim over
  built rows nor be bounded by anything else, so both materialised the whole
  answer to deliver a page; both now carry `LIMIT + OFFSET` row views instead.

  The budget is still 2 GiB and `FORGEQL_FIND_MAX_ROWS` still overrides it in
  rows, but it now bounds two things with two defaults: the rows a scan carries
  (about 44.7 million, at 48 bytes each) and the rows it builds for delivery
  (about 1.34 million, at about 1,600). Each refusal says which one it is.

- A row view reads a field the way the result row it stands for reads it, and
  now does so for a field whose name a segment's own enrichment column shares.
  It used to report such a field absent, which disqualified the entire segment
  from every route that ranks or keys rows before building them. That was not a
  corner case, and not an accident of one enricher: a file's enrichment columns
  are, among other things, every tree-sitter grammar field of every node the
  indexer emitted, and `name` is a grammar field on essentially every definition
  node. So essentially every code segment shadowed it — 308 of the 411 segments
  of this repository's own index, each excluded from the cheaper route for every
  query. Only `usages`, `node_id` and `count` remain unreadable from a view,
  because a result row fills those in from outside its own columns.
