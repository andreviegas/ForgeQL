- `forgeql-server` now runs on jemalloc, as the `forgeql` binary already did.
  The daemon had no allocator override and so ran on glibc malloc, which parks
  memory freed by a large index build in per-thread arenas and never trims it
  back to the operating system — the resident size of a long-lived server
  stayed at its post-build high water mark. With jemalloc's background decay
  thread the freed pages are returned. Not enabled on Windows, where jemalloc
  does not build under MinGW. Not measured on the daemon itself here: the
  effect is the one the `forgeql` binary was given the same allocator for
  (recovering RSS after zephyr-scale index frees), now applied to the process
  that actually stays up.
