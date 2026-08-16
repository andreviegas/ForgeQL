- Collecting macro definitions no longer keeps a `PathBuf` per definition and
  three copies of every macro name. Both are interned and the records carry
  4-byte ids. On Zephyr's 254,732 definitions the table's counted heap falls
  from 86 MiB to 41 MiB and the phase's anonymous memory from ~500 MiB to
  ~430 MiB. The phase matters out of proportion to its size because almost
  none of its memory is reclaimable.
- Indexed output is unchanged — a cold index of Zephyr and of VLC produces a
  byte-identical overlay before and after — so the enrichment cache version is
  not bumped for this change.
