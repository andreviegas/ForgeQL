- A bare `FIND symbols LIMIT k` — no WHERE, no ORDER BY — is now served by the
  ascending name stream instead of materialising every row in the corpus. Its
  default ordering starts with `name`, so the streamed page is exactly the
  page the full scan sorted its way to. On a 3.05 M-symbol corpus the bare
  form measured about 7.4 s per query while the explicit
  `ORDER BY name ASC LIMIT k` form measured near zero; the bare form now
  takes the same path.
- The name-stream fast paths now report an honest `total`. They used to
  report the size of the page they streamed — `total: 5` on a corpus of any
  size — which was pinned as an expected failure in the golden suite and is
  now enforced: the whole-corpus total is the sum of the per-segment
  deduplicated row counts stored when the overlay is built, and a
  kind-filtered stream's total is its bitmap's cardinality, so neither walks
  the index it avoided reading. On the one index shape where those stored
  counts cannot speak for the answer — two segments built from one source
  path — every stream form, the bare one included, declines and the full
  scan answers: slower there, never a wrong count.
