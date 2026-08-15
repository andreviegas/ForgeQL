- `WHERE`/`HAVING` predicates using `MATCHES`/`NOT MATCHES` now reject an
  uncompilable regex pattern before any row is read, naming the field, the
  operator, and the underlying regex error. Previously an invalid pattern was
  silently treated as "matches nothing" for `MATCHES`, and — worse —
  "matches everything" for `NOT MATCHES`, so a broken pattern on the negated
  form returned a plausible-looking, unfiltered result instead of an obviously
  empty one.
