- Every indexing phase now logs how much memory it is holding, beside the time
  it already logged. A build that ends at 29 GiB used to say only which phase
  was slow, so attributing the peak to a phase meant guessing, and a report of
  "the reindex needed 25 GB" could not be acted on without reproducing it under
  a profiler. Each `TIMING` line now carries `anon`, `file` and `peak`.

  The split matters more than the total. `anon` is heap and thread stacks —
  memory the process owns, that nothing else can reclaim, and that an
  out-of-memory kill is decided on. `file` is page cache behind the mapped
  segments: shared between every session that mapped the same segment, dropped
  by the kernel under pressure, and counted once per mapping rather than once
  per page, so it overstates. `top`, `ps` and every process viewer report the
  two added together, which is why a large reading there says nothing about
  whether a build is in danger. Measured on the Linux kernel, an index peaking
  at 29.2 GiB was 15.5 GiB of the first kind and 10.2 GiB of the second.

  Two phases that had no line at all now have one, both single-threaded and
  both landing after the overlay build reports its total — which made them read
  as time nothing accounted for. Opening the overlay and its segments is timed
  separately from building them, because a cold index opens every segment twice
  and only the first open was visible; and freeing the build-time table is
  timed, because returning one heap allocation per macro definition is
  single-threaded work that looks like a hang on a corpus with six million of
  them.
