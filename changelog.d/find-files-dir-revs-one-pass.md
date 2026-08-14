- `FIND files` computes directory revs in one pass over the worktree instead
  of re-scanning the entire file list for every directory row. On a
  95,000-file corpus an unscoped `FIND files` previously spun for over 17
  minutes of CPU without answering; the listing is now one walk plus a lookup
  per row. The revs themselves are unchanged: a directory rev is still the
  same flat XOR of the path fingerprints of every file underneath it.
