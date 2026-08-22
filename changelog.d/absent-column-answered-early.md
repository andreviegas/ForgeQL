- A `WHERE` on a field a segment has no column for is now answered from that
  absence instead of waiting for the rows to be built. It used to be treated as
  unanswerable, which was over-cautious in a way that cost the whole query: the
  row that segment would build carries the field in neither its struct nor its
  enrichment map, so both readers resolve it to nothing and agree, but one
  segment declining was enough to take the page off row views for every segment
  that could answer. On this repository's index a `WHERE lines > 150` selects
  411 segments and 103 of them carry no `lines` column; on a
  three-million-symbol corpus it selects 32,748 and 21,983 of them do not. Every
  one of those queries built its whole answer before it could deliver twenty
  rows, and now cuts the page from views.

  **Answers do not change anywhere** — only where the predicate is answered.
  The early filter and the filter over built rows are the same function reading
  the same absence, so the operator table is inherited rather than restated:
  every operator that consults the field is false, `!=` and `NOT LIKE` included,
  because each arm is `is_some_and` and a missing value fails an operator rather
  than passing it. A segment with no column therefore contributes nothing, which
  is exactly what it contributed before. `NOT MATCHES` is the one shape held
  back, and not for cost: on a value that resolves to nothing the batch filter
  keeps the row while a per-row evaluation would drop it, so every regex still
  waits for the rows that batch was compiled for.

  **What was not measured, and why.** No `bench_ab` class exercises this. Each
  of its enrichment-equality classes is pre-narrowed by the posting index to
  segments that post the value, so no candidate can be missing the column
  (`naming_eq` selects 24,056 segments with none absent); the remaining classes
  name struct-backed fields or use a regex. The shape that moves is an
  enrichment predicate the postings cannot pre-narrow — a range such as
  `lines > 150` — and the harness has no class for it. The route change itself
  was measured, by counting the declining segments through the query path on
  both corpora, and it is pinned in `fast_paths/tests.rs`; the time it buys is
  not claimed here because nothing on this box measured it.
