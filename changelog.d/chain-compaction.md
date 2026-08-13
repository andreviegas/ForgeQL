- A commit chain that has grown past a configurable size
  (`FORGEQL_CHAIN_COMPACT_PATHS`, default 512 changed-or-removed paths) is
  compacted on attach: its merged index is assembled once from the master
  index and the recorded changes, written where every later attach finds it,
  and the superseded manifest is removed. Below the threshold an attach
  keeps seeding the changes directly, which costs milliseconds; the
  compaction is the one full merge, paid once per chain instead of never
  bounding it.
