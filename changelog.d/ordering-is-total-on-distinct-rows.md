- The ordering that decides every `FIND symbols` page now ends its tie-break in
  `fql_kind`, making it `(name, line, path, fql_kind)` — the same four fields
  as the duplicate-collapse row identity, so two rows an answer tells apart
  never compare equal. Until now, rows equal on the ordering field and on
  name, line and path while differing in `fql_kind` were ordered arbitrarily:
  which of them a `LIMIT k` page held could change between runs or with `k`.
  That caveat is gone from the guarantee rather than restated. Two visible
  consequences: a page containing one of two such previously-tied rows may now
  deterministically hold the other one, and the in-memory backend collapses
  duplicate rows on the same identity the columnar backend uses (`fql_kind`
  rather than the language-level node kind), so the two backends agree on what
  a duplicate is. A segment that stores an enrichment column named `fql_kind`,
  shadowing the built-in field, now builds its rows before ranking them
  instead of ranking from its columns — slower for that segment, never a
  different page.
