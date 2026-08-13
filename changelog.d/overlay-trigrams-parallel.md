- The overlay build computes the substring-search trigram bitmaps in a
  parallel pass over the per-file indexes instead of inside the serial
  name-index merge. The bitmaps produced are unchanged; the serial part of
  the build shrinks by the trigram work, which now spreads across cores.
