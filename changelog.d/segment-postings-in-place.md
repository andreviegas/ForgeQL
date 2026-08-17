- Opening a segment no longer decodes its Roaring posting lists into the heap.
  A segment's `postings_fql_kind`, per-field enrichment postings and
  `name_prefix` index used to be deserialised — every bitmap of every blob —
  into per-segment hash maps at open, once by the overlay build and again by
  each session that opened the result, and held for as long as the segments
  were. They are now addressed in place over the segment's mmap: `open` checks
  the entry tables without decoding, and a lookup deserialises only the one
  bitmap it returns. Measured on a cold index of a 3.06M-row, 32,748-segment
  corpus (`overlay_probe`, one run each way, ci profile): the anonymous memory
  the segment-open phase adds fell from about 550 MiB to about 340 MiB, the
  resident anonymous memory of a session after `USE` from about 1.1 GiB to
  938 MiB, peak process RSS from 3,399 MB to 3,127 MB, and the shared open
  from 5.4 s to 4.3 s; the overlay is byte-identical (same md5). On the frozen
  3M-symbol corpus (`bench_mem`, ci profile) `USE` alone went from 622 MB to
  380 MB of private heap at peak (6.9 s to 6.4 s), `kind_scan` — a whole-corpus
  `WHERE fql_kind = 'if'` that now decodes one kind bitmap per segment instead
  of a hash lookup — from 852 MB to 611 MB at 834 ms to 808 ms (1.03x, inside
  the harness's ±5% floor), `full_scan` from 2,365 MB to 2,118 MB, and three
  sessions on one commit still share (3 MB per extra session, unchanged).
  `bench_ab`, two runs on a shared box: `naming_eq` 2,036 → 2,014 and
  2,119 → 1,983 ms/q (1.01x, 1.07x), `usages_split` 871 → 922 and 843 → 760
  ms/q (0.94x, 1.11x — a class that reads none of the changed structures, so
  that spread is the box's run-to-run noise), `name_like` under the 200 ms
  floor both sides. What
  did **not** move: the remaining ~340 MiB of the open phase is the readers'
  own tables (TOC, column names, string-pool bounds), which are still heap;
  the query paths that read per-segment postings — the enrichment prefilter,
  the kind prefilter, the short-prefix `LIKE` index and the absence proof —
  now walk the blob's entry table and decode one bitmap per touched segment
  instead of a hash lookup; `kind_scan` and `naming_eq` above exercise the
  first two, whether `name_like` reaches the short-prefix index depends on
  its fixed patterns (1–2 leading characters do, longer ones take the
  trigram tier), and the absence proof has no class.
  A bitmap whose bytes will not decode is reported by the lookup that reaches
  it, not at open: the prefilters stop narrowing (the residual filter still
  decides), the absence proof declines to prove, and nothing is dropped.
