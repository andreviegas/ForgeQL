- **Opening a segment no longer copies its table of contents onto the heap.**
  A reader parsed every blob name of every segment into a `String`-keyed map
  and kept that map for as long as the index stayed open, together with an
  owned copy of each enrichment column name it carried. On a 3-million-row
  workspace of 32,748 segments that was several kilobytes per segment, held
  for the life of the open commit. The table of contents is now read in place
  and dropped when the open finishes — what a reader keeps is the byte ranges
  of the blobs it will actually read — and the names that repeat across every
  segment, the enrichment column names and the occurrence role names, are
  interned, so a workspace holds one copy of each instead of one per segment.

  Measured on a 3.06-million-row corpus (32,748 segments), against the release
  before it: the memory a cold index build adds when it opens the segments
  fell from 299 MiB to 8 MiB, the private memory a session holds once its
  index is open from 825 MiB to 437 MiB, and the peak of the whole cold index
  from 3,143 MB to 2,824 MB, at the same wall time. A second or third session
  on the same commit still costs 3 MB each — the sharing between sessions is
  unchanged. Whole-corpus scans came out faster rather than slower, because
  deciding whether a segment shadows one of the fields a result row answers
  from its own struct is now a bit test resolved at open instead of a walk of
  that segment's column names on every row.

  Together with the posting lists that stopped being decoded at open in the
  previous release, opening every segment of that corpus costs 8 MiB where it
  cost about 560. The two are what a session's at-rest cost was made of, so a
  commit's index is now very nearly its mapping.

  A reader no longer keeps the path of the file it was opened from, so where a
  diagnostic used to name a segment by its path inside the reader it now names
  it by its content id; the overlay build, which knows the path, still uses it.

  What did not move: the index format is unchanged byte for byte — the overlay
  a cold index produces has the same checksum the unchanged code produced — so
  nothing has to be reindexed.
