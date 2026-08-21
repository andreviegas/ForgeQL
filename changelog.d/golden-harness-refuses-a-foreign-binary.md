- **The golden harness refuses to run when it was built somewhere other than
  where it is running.** The directory it reads its suites from is fixed when
  the harness is compiled, and several checkouts on one machine may share a
  build directory — so a harness built in one tree could be handed to another,
  read the first tree's suites, and report them as the second tree's result. A
  pass then said nothing about the tree under test, and an unexpected count of
  cases was the only hint anything was wrong. The harness now compares where it
  was built against where it is running, and stops with both paths named. It
  asks that before it looks for a corpus, so a foreign harness with no corpus is
  refused rather than reporting a skip.

  What this does not cover, and still needs the count read with care: the engine
  binary the harness drives is addressed by a path into that same shared build
  directory, so a concurrent build in another tree can replace it between
  compiling and running; and the segment cache is keyed by the enrichment
  version, so a change to index output without a version bump is still answered
  from pre-change segments.
