- A new golden suite (`open_defects`) pins five known-open defects as
  expected failures: `-1` and context-dependent `0` literals stamped
  `is_magic` against the documented exemption list; the `'false'` value of
  most boolean enrichment fields answering zero rows while `has_doc` and
  `is_magic` answer both values; `SHOW NODE … LINES A-B` failing to parse
  while the error advertises the clause; an unknown `fql_kind` value
  matching nothing silently instead of being refused; and local variables
  reporting the corpus-wide count of their name as `usages`. Each case
  asserts the correct behaviour, runs without failing the gate while the
  defect stands, and fails the gate on a PASS — so the commit that fixes a
  defect must promote its case in the same change. Engine behaviour is
  unchanged. Found while authoring the suite and fixed with it: the golden
  runner evaluated `error: true` / `error_contains` only on the steps of a
  multi-step case — on a single-query case the keys were accepted and
  silently never read, so such a case could pass with no assertion evaluated.
  A single-query case now honours them with the same contract as a step; the
  unknown-kind case above exercises that path, so a regression re-ignoring
  the keys flips it to an unexpected PASS and fails the gate.
