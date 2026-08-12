- Indexing no longer holds the macro table through the whole overlay build. It
  is collected in a first pass, feeds macro expansion while each file is
  indexed, and after that has exactly one reader left — the writer of the
  legacy on-disk cache, which the columnar path never calls. It was kept
  anyway, so a build held one heap allocation per macro definition, unread,
  until the session dropped its legacy index at the very end. It is now
  released as soon as the pass that needs it has finished. Incremental reindex
  is unaffected: it takes no macro table and never has.

  Measured on the Linux kernel — 6,119,906 macro definitions, 80,510 files,
  29,864,281 rows — the table is 6.5 GiB, and it was resident for the five
  minutes of overlay assembly that never looks at it, with every other phase's
  memory stacked on top.

  Together with segments no longer building a lookup table that only queries
  read, a cold index of that corpus peaks at **19.6 GiB where it used to peak
  at 30.2 GiB**, a 35% reduction, and produces a byte-identical overlay.
