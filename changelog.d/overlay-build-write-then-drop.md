- The overlay build no longer holds every blob until the file is written. Each
  blob now goes to disk as soon as the file's layout allows, and its buffer is
  freed there and then; the header and its table of contents sit at the front
  of the file but are written last, once every offset and length is known. The
  merged kind, trigram and enrichment row sets stay bitmaps until the moment
  they are written, which removes two full copies of that data at the build's
  highest point — one serialised buffer per bitmap, and one concatenation of
  all of them. The row table, eight bytes for every row in the commit, is
  written first and freed before any merge step runs; and the segment table,
  the per-file sizes, the file-only entries and the usages-count aggregate are
  built during the write rather than before it, so they allocate against a heap
  the earlier blobs have already left.

  The file is unchanged, byte for byte. Its checksum cannot show that on this
  project's corpora, because the same unmodified binary produces different
  checksums on consecutive runs of the same corpus. So the algorithm each part
  replaced is kept as a test oracle and the new one is compared against it byte
  for byte: the header, the table of contents and the padding between blobs;
  the enrichment blob; the four kind and trigram blobs; and the segment table
  with its string table. The remaining blobs are written straight out of a
  buffer with no layout work of their own. The padding cases are the two a
  streaming writer is most likely to get wrong: unaligned blob lengths, which
  push every later blob along, and a trailing run of empty blobs, which still
  carries the padding of the last blob that had content and so makes the file
  longer than its last content byte.

  Where an index blob is written before the payload it points into — the kind
  and trigram indexes before the bitmap region they share, the segment table
  before its string table — the offsets it stores are worked out from sizes
  rather than read off the bytes. A single byte of disagreement there would
  misplace every entry after it and the file would still parse, so the writer
  now refuses one whose payload does not write the predicted number of bytes,
  rather than producing a file that reads back wrong.

  No before-and-after peak figure is quoted here. The corpus probe this project
  measures index builds with produced no overlay on any corpus while the change
  was written — on unmodified code as well — so what the build no longer
  allocates is argued from the code rather than measured on a corpus.

  What still peaks together, and why: the enrichment bitmaps, the name FST with
  its postings, and the trigram bitmaps come from three passes that run
  concurrently, and the file's layout puts the trigram bitmaps before the name
  FST and the enrichment bitmaps after it. Writing each as it finished would
  reorder the file, so all three are still live when the first of them is
  written. Removing that overlap means running the three passes in sequence,
  which is a different trade and not this change.

- The trigram merge builds one partial map per worker thread rather than one
  per segment. Mapping every segment to its own map and reducing them pairwise
  left the number of maps alive at once following rayon's split tree instead of
  the size of the pool.

- The thread pool used for the parse and enrich passes of an index build is now
  built for each run and dropped when the run ends, instead of living for the
  life of the process. Its workers are given a 256 MiB stack so that deeply
  nested source cannot overflow them — that property is unchanged — but every
  stack page an enricher touched used to stay mapped for as long as the process
  ran. The pool is also sized at half the machine's cores by default, which
  `FORGEQL_INDEX_THREADS` overrides: how many per-file peaks can overlap is set
  by how many workers there are, and coinciding peaks have driven a 24 GB
  machine into a 2,824-second swap event. On a machine with memory to spare
  this trades some index-build wall clock for that headroom; raise the variable
  to get it back. The incremental reindex a mutation triggers takes a pool of
  one worker instead: it walks its handful of edited files sequentially, so it
  wants the stack and none of the parallelism, and spawning a full pool there
  would cost every mutation a set of threads with nothing to do. Failing to
  spawn the workers is now an error rather than a panic, which matters once a
  pool is built per run rather than once per process.

- One measurement that moved rather than disappeared: the enrichment step's
  log line no longer reports the serialised size of its blob, which it could
  only know by serialising it. The overlay write now reports the size of the
  whole file it produced instead.
