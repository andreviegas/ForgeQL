- `Overlay`'s enrichment-field lookups (equality, existence, and the
  numeric `>=`/`<=` prefilters) now binary-search the on-disk index
  directly instead of first copying every distinct `field=value` key into
  an owned `String` at session open. The on-disk array was already sorted
  and already binary-searched by key, so the copy reproduced work the file
  format had already done; removing it drops one heap allocation per
  distinct enrichment key from every session's private memory. No on-disk
  format change.

  Measured on a 3M-row corpus: resident private memory right after opening
  a session did not move outside rounding (2,516 MB before and after, at
  megabyte-granularity sampling). The eliminated allocations are real, but
  they are small next to a multi-gigabyte baseline whose bulk is per-row
  segment decode, not this index -- a few hundred distinct keys at roughly
  sixty bytes each does not register against 2.5 GB. The query class
  measured for timing does not exercise this code path (it reads the
  unrelated kind-bitmap index instead), so no timing claim is made here.

- Also corrected the file's own module doc, which had drifted three format
  revisions out of date: it still described a 9-blob, 600-byte table of
  contents, and still gave the header's schema version as 3. The format in
  use has 13 named blobs behind an 856-byte header + TOC region, and a
  schema version of 15 -- a value the reader refuses to open a file without.
  The doc now lists all thirteen blobs, including the four it had never
  mentioned.
