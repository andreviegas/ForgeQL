- A query with a `WHERE` the index cannot answer used to build every candidate
  row and then throw the non-matching ones away. `FIND symbols WHERE fql_kind =
  'if' WHERE line >= 100 WHERE line <= 105` built a full row — its name, kind,
  language, path, node handle and enrichment map — for every `if` in the corpus
  in order to keep the few hundred on those lines.

  The residual `WHERE` is now tested against the segment's columns first, in
  place, and only the rows that pass are built. Answers are unchanged: same
  rows, same order, same totals, same paging.

  A predicate the columns cannot answer is not dropped — it is handed to the
  filter that runs after the rows are built, exactly as before. That covers a
  workspace usage count, which is only known once a row exists; a node handle,
  which is built as the row is; a field no column of that segment holds; and a
  regular expression, which is cheaper compiled once for a batch than once per
  row. A segment that stores an enrichment column named after a field a result
  row answers from a struct field of its own answers no fixed column early at
  all, because the two would disagree.

  Measured on a three-million-symbol corpus, this is a saving of time and not
  of memory, and the saving is confined to queries that ask for a large
  filtered answer: streaming twenty thousand name-ordered rows runs 1.7x
  faster, while queries capped at a small number of rows, and unfiltered scans
  with nothing to discard, are unchanged. Peak resident memory does not move on
  any of them, because the discarded rows were already built and released one
  file at a time — they cost processor time, never footprint. What a large
  answer holds in memory is the rows it returns, and a filter cannot reduce
  those.
