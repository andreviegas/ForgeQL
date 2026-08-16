- The enrichment-bitmap phase of the overlay build now walks segment columns as
  the 32-bit value ids they already are, resolving each distinct value to text
  once per segment instead of rebuilding a string per field per row. On a
  29,864,281-row corpus the phase drops from 78,156 ms to 31,929 ms, the whole
  overlay build from 111,630 ms to 80,393 ms, and the peak of a cold index from
  19.48 GiB to 18.88 GiB. Output is unchanged: the overlay checksum is identical
  before and after at 29.9M, 3.06M and 589k rows.
- Because the enrichment version changes, cached segments and overlays are
  re-keyed and every workspace re-indexes once on upgrade.
