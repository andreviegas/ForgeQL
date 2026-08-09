- `WHERE file MATCHES ...` and `WHERE file NOT MATCHES ...` silently ignored the
  `file`/`path` alias and every other alias the field table declares. Every other
  operator (`=`, `LIKE`, `>`, `HAVING`, `ORDER BY`, `GROUP BY`) resolved an alias
  to its canonical field before reading a row; the regex operators read the
  written field name as-is. An alias that named no struct field and no
  enrichment map entry then read as absent on every row, so `MATCHES` matched
  nothing and `NOT MATCHES` matched everything — both silently, with no error.

  The regex operators now resolve the field the same way every other operator
  does, once, before compiling the pattern. `WHERE file MATCHES` now answers
  identically to `WHERE path MATCHES`.
