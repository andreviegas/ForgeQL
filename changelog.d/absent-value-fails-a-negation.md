- **`NOT MATCHES` no longer returns rows that have no such field at all.** A
  row that does not carry the field fails every predicate naming it — `!=`,
  `NOT LIKE` and `NOT MATCHES` alike — because a value that is missing is not a
  value that differs. `NOT MATCHES` was the exception: two filters answer a
  `WHERE`, and the one that compiles a regex once for a whole batch treated a
  missing value as a successful non-match and kept the row, while every other
  operator dropped it. So the two spellings of one question answered
  differently. On the frozen snapshot this repository pins its golden suite
  against, `WHERE num_format NOT MATCHES 'hex'` returned 59,601 rows — nodes of
  every kind, most of which have no `num_format` — where
  `WHERE num_format NOT LIKE '%hex%'` returned the 3,585 numbers that are not
  hexadecimal. Both now answer 3,585.

  **This changes answers.** A `NOT MATCHES` predicate over a field only some
  rows carry returns fewer rows than it did: only the rows that carry the field
  and do not match. If you were relying on it to mean "everything except the
  matches", spell that intent as two queries, or name a field every row
  carries. Unchanged: a **pattern** operator handed something it cannot use
  still passes before any field is read — `NOT LIKE` or `NOT MATCHES` with a
  non-string value, or `NOT MATCHES` with a regex that does not compile. `!=`
  is not in that set and never was.

  The gate that holds every regex back to the batch filter was load-bearing for
  this reason and is now about cost alone — a pattern compiled once per batch
  rather than once per row.
