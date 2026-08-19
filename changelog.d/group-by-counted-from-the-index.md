- `FIND symbols … GROUP BY <field>` is now counted from the index when the field
  is one the segments post per value — `naming`, `scope`, `has_doc`,
  `comment_style`, `cast_safety`, `guard_kind`, `key_path` and the rest of that
  set. The answer is a handful of numbers and the numbers are already stored:
  the overlay's `field=value` key table is binary searched to the field's range
  and each key's bitmap cardinality is that value's group, so no result row is
  built for any of the rows counted. `GROUP BY fql_kind` and `GROUP BY file`
  were already served this way; this extends it to the enrichment fields.

  On a 3-million-symbol corpus `FIND symbols GROUP BY naming ORDER BY count
  DESC` used to materialise past the 2 GiB result budget and be **refused** by
  it; it now returns its eight groups. The groups and their sizes are the
  scan's: the stored bitmaps are drawn from each segment's canonical rows,
  which is the same collapse the scan's dedupe pass performs, and a row carries
  at most one value of a field, so the cardinalities are the collapsed counts.
  The rows a field says nothing about are still one group keyed by the empty
  string, sized by subtracting the values from the stored canonical total —
  on that corpus the eight `naming` groups add up to 2,962,193, which is what a
  bare `FIND symbols` reports as its total, and each value's group matches what
  `WHERE naming = '<value>'` reports on its own.

  One thing the two routes do not share is the order of a page the clause has
  not decided. A counted group row is named by its own value; a scanned one is
  the first row of the group and is named by that row. So with no `ORDER BY`, or
  where an `ORDER BY count` ties, the same groups with the same counts come back
  in a different sequence — and under the 20-row default page that changes which
  of a wide field's groups you see. Never `total`, and never a count. Write
  `ORDER BY count DESC` and the field, not the page, decides. A counted group
  row also carries the value and the count and nothing else — no path, no line,
  no `node_id`, no `rev` — where a scanned one is the first row of its group and
  carries all four. The CSV group layout renders `key,count` either way, so the
  difference shows only in `format=JSON`.

  The counted path hands the query back to the scan wherever the stored counts
  would not be the collapsed ones, and each of these is a refusal rather than an
  approximation. The first is the one you will meet: a `WHERE`. A stored
  cardinality counts a value over whole segments and cannot be narrowed to the
  subset a predicate selects, so `WHERE fql_kind = 'function' GROUP BY naming`
  scans exactly as it did, as `GROUP BY fql_kind` beside any `WHERE` does. Note
  that `GROUP BY file` is not in that company — it groups by segment, so it
  admits a `WHERE` built from `fql_kind = '<value>'` and counts the
  intersection. That one predicate is the whole admitted set, and it took a
  second change to make it so: writing this down turned up that the path
  counted whatever a tier proposed without testing one of them, having cleared
  the residual `WHERE` first, so a `name` predicate could count high at any
  literal length. The entry on the two older counted groupings has the numbers
  and the repair. `fql_kind =` is admitted because its postings are intersected
  with each segment's canonical rows at build, which no name tier is. Extending
  any `WHERE` admission to the enrichment table has to clear that same bar —
  per predicate tier, that the candidate set is exact — which is why this
  change takes none. `IN` and `EXCLUDE` are welcome everywhere: a segment is one
  source path, so a glob selects whole segments and the counts narrow with it.
  All three counted groupings also want a session holding no uncommitted edits,
  an index with no two segments built from one source path, and a `HAVING` or
  `ORDER BY` naming anything but `count` or the grouped field, since a counted
  group row carries those two and nothing else. The rest are the
  enrichment table's own: a field the overlay pruned for carrying more distinct
  values than its budget allows; and a selected segment that stores the column and
  posted none of its values, which happens past the per-field posting budget and
  leaves its rows carrying values no key counts. `IN`/`EXCLUDE` are applied by
  intersecting with the rows of the segments whose path passes, and only those
  segments are asked about their postings. A `GROUP BY` on a field stored only
  as a column — `num_format`, `lines`, `guard`, `error_scope` — is unchanged and
  still scans.

  **What it costs.** One stored bitmap is decoded per distinct value of the
  field, so the cost tracks the field's cardinality rather than the corpus. For
  the narrow fields that is a handful — `naming` has seven values. Five fields
  are budgeted wide (`key_path`, `guard_defines`, `guard_mentions`,
  `guard_negates`, `guard_group_id`) and can carry thousands: `GROUP BY
  key_path` over this repository's own 84,762-symbol index decodes 1,085 of
  them and takes about 0.8 s against a 0.1 s warm-session baseline, where
  `GROUP BY naming` on the same session is not separable from the baseline at
  all. On a 3-million-symbol corpus those five decline before that ever
  matters — some segment there exceeds its per-field posting budget, so the
  partial-posting gate hands them to the scan — and the counted route in
  practice serves the narrow fields.

  **Measurement.** No fixed bench class covers a `GROUP BY`, so this is not a
  `bench_mem` or `bench_ab` figure. What is measured is a pair of probe runs of
  the same query through the same freshly built (unoptimised) binary on a
  3,062,139-symbol Zephyr checkout, timed by the probe job's own elapsed against
  a `USE`-only run of the same shape: the `USE`-only baseline read 7.2 s and
  9.6 s on two runs, the counted grouping 8.1 s and 8.2 s — inside that
  baseline's own spread — and the same grouping forced through the scan (one
  `WHERE` every row satisfies) 18.5 s and 17.2 s before being refused. So the
  counted route costs less than the harness's run-to-run noise, and the scan
  route costs roughly ten seconds and then answers nothing. Absolute figures are
  from a debug build on a box that was also running another agent's benchmark
  for part of the window; the refusal, not the seconds, is the result.
