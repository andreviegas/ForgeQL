- Four enrichment fields an enricher really writes — `error_scope`,
  `expansion_depth`, `expanded_reads` and `expansion_failure_reason` — are
  recognised by the unknown-field refusal, which previously called them
  unknown. Because that refusal decides typo-or-real from a fixed list, a
  field the list omitted was rejected as a misspelling on any corpus holding
  no row for it, and `doc/syntax.md` offers `WHERE error_scope = 'root'` as a
  worked example. A query on one of these now reaches the ordinary
  no-candidate message instead of being refused as nonsense.
- A new golden suite, `broken_query_refused`, pins the refusals themselves the
  way an agent meets them: by writing a statement and reading the answer. It
  covers an uncompilable `MATCHES` and `NOT MATCHES` pattern, the same in
  `HAVING`, and an unknown `WHERE` field, and each refusal travels with a
  control proving the query answers once the broken part is removed — so a
  refusal can never be mistaken for an empty corpus, and the validation can
  never regress into refusing a working query. The pattern check previously
  had unit tests only; those construct the clause value and call the checking
  function directly, so all three stayed green when the single line wiring the
  check into the engine was deleted, and the protection could have been lost
  without any test noticing. The new suite fails in exactly that case. The
  filter test module now states the rule that separates the two levels.
