- The overlay build's name-FST merge now runs in parallel. It was a sequential
  loop over every segment's name postings and the longest step in the build. It
  is now split across 256 disjoint first-byte shards, each merging its own key
  range; only the final FST insert stays serial, because `fst::MapBuilder`
  requires ascending keys. The usages-count FST merge got the same treatment.
  On a 3.06 M-row corpus (32,748 segments): name FST 6,359 → 4,931 ms,
  usages-count FST 830 → 416 ms, whole overlay build 8,890 → 7,096 ms, with the
  overlay byte-for-byte identical and peak resident set unchanged.
