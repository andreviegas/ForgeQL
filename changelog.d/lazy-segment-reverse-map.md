- Opening a segment no longer builds a lookup table nothing is going to read.
  Each segment carried a reverse map from string to id, built eagerly when the
  segment was opened, with an owned copy of every string in its pool as keys —
  bytes the memory-mapped file already holds. Its only callers are query
  prefilters, so an index build constructed one per segment and read none of
  them. It is now built the first time a prefilter asks that segment a
  question, and not before.

  Measured on the Linux kernel, 80,426 segments: the phase that opens them all
  for the overlay build went from 17.8 seconds to 1.4. The overlay it produces
  is byte-identical to the one built before the change, which is the property
  worth keeping if this is ever made eager again.

  This does not reduce the memory that phase holds — its 7.5 GiB of growth was
  unchanged by the removal, so the allocation belongs to something else opened
  at the same time.
