- `FIND usages` now cuts its page before it builds the rows. Its `LIMIT`
  counts whole files, not rows, and until now the backend was asked for every
  site, built a result row for each, and the caller selected file groups out of
  that list — so a hot identifier with tens of thousands of sites paid about
  1.6 KB per site to deliver twenty files. The file count, the `OFFSET` in
  files and the site ceiling now travel to the backend as their own bound, the
  residual `WHERE` and the `ORDER BY` run over the sites, and a result row is
  built only for the sites inside the page. Answers are unchanged: the site
  view answers every clause field exactly as the row built from it does, which
  a test in `crates/forgeql-core/src/storage/mod.rs` holds it to, and the
  `crates/forgeql-core/tests/usages_file_groups.rs` suite pins the page itself.
  `total` is still every site that matched under the default page; under
  `GROUP BY` it counts groups rather than sites, and an explicit `LIMIT` clips
  it, both as before this change.

  What did **not** move: the site list itself is still held whole and still
  bounded by the row budget, because a cut that selects whole files out of a
  computed answer cannot bound what is searched. There is no `bench_mem` class
  for `FIND usages`, so the memory claim here is the count of rows built — from
  every matching site to the sites the page renders — and not a measured peak.

- The zero-symbols hint (`FIND symbols WHERE name = 'x'` returning nothing, on
  a name the workspace uses) asked for the site list only to take its length.
  It now asks for a page of no files at all: every site is still read and
  counted, none is built.

- Two claims about the result budget were wrong and are corrected in the
  budget documentation itself, in the refusal an agent reads when it hits the
  budget, and in every agent document that repeated them. `FIND files` was described as needing no bound because it "pages at
  the standard 20-row `FIND` default"; it does build a row for every workspace
  entry before any clause runs, and what makes that safe is that the row is a
  file entry and the count is the number of files rather than the number of
  symbols. Its default page is also a constant of its own rather than the
  configurable `find_limit` every other verb defaults to, so the two agree only
  while nobody retunes one.
