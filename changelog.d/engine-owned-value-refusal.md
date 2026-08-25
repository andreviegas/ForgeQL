- A `WHERE` or `HAVING` value the engine itself could never produce is now
  **refused** instead of answered with a silent zero. `FIND symbols WHERE
  fql_kind = 'impl'` returned nothing on every corpus — Rust `impl` blocks are
  indexed as `class`, so no row anywhere could match — and `SHOW outline …
  WHERE fql_kind = 'impl'` printed a header and no rows at all, which reads as
  "this file has none of those". Both now fail with the accepted kinds named.
  The same holds for `role` on `FIND usages`, whose values (`code`, `comment`,
  `string`, `config`, `doc`, `text`) are minted by the pass that finds the site.

- **The boundary is the other half of the change.** This applies only where the
  ENGINE owns the set of values a field can take. Where the CORPUS owns it, a
  value it never stored still answers empty and the empty answer is correct:
  `guard_kind = 'ifdef'` on a repository holding no such guard returns nothing,
  not an error. Exactly two fields declare an engine-owned value set, and a test
  fails if a third is added without that being a deliberate decision.

- What moved, per operator. `=` and `!=` are checked — `!=` because it excludes
  nothing and returns everything, which is the harder wrong answer to notice.
  Patterns are untouched, because a pattern names no value: `fql_kind LIKE
  '%_block'` still answers, 2,660 rows on this repository. `ORDER BY` and
  `GROUP BY` name a field rather than a value and are unaffected.

- `fql_kind = ''` — the kind a row carries when nothing maps its grammar node —
  is **accepted**, because `GROUP BY fql_kind` publishes those rows under that
  name and refusing the equality would close a shape the grouping had just
  opened. It is not yet ANSWERED: the equality returns no rows where the
  grouping counts 2,169 on this repository, because the kind lookup resolves a
  predicate value through the segment string pool and the empty kind is not a
  pooled value. That is now recorded as an open defect rather than left
  implicit, since the rest of this change tells you an empty answer on
  `fql_kind` is a fact about the code.

- The check runs once per operation, ahead of dispatch, over that operation's
  own clauses. It therefore reaches every verb that carries a clause and both
  storage backends rather than landing in one verb, and a verb added later
  inherits it without being named anywhere.

- The kind vocabulary a language plugin may map onto is now declared in one
  place, and a test reads every language configuration in the repository —
  found by walking the crate directories rather than from a list of them — and
  fails if one maps to a kind the declaration does not carry. A kind missing
  from that list would refuse a legitimate query, which is worse than the
  silence this replaces.
