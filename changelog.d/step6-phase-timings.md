- The overlay build's `TIMING step6` line now reports where the time goes:
  `prep_ms` / `merge_ms` / `merge_cpu_ms` / `serial_ms` / `finalise_ms`, and
  `TIMING step6.5` the same. `merge_cpu_ms` is CPU summed across shards, so
  `merge_cpu_ms / merge_ms` is the parallel speedup the merge actually gets —
  the number that was missing when deciding whether more parallelism helps.
  On a 3.06 M-row corpus the name merge reaches 4-6x while the usages-count
  merge reaches 12-15x on identical shards, and the serial FST build, which
  cannot be parallelised at all, is 69% of the step.
- The name merge no longer allocates per posting: it borrows the mapped
  postings instead of copying them, and looks up an existing map entry before
  allocating a key. That cuts merge CPU from 5.6-5.8 s to 2.2-2.8 s on the same
  corpus. Wall clock for the step is unchanged (5,725 -> 5,724 ms) because the
  merge is not the step's dominant term there; the overlay is byte-identical.
