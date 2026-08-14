- A directory's revision fingerprint now has one definition instead of three.
  A listing stamps a directory row's `rev`, a bulk `IF REV` gate re-derives it,
  and a bare directory handle resolves to it — three copies of the same fold,
  each of which had to agree to the bit or every mutation on a directory would
  be refused with a mismatch that re-running the `FIND` could never clear. They
  now call one function, and unit tests pin both what a directory rev means and
  that the handle path really routes through it. Behaviour is unchanged; the
  previous copies agreed, but nothing was checking that they still did — the
  end-to-end test that used to compare two of them stopped covering the third
  when the gate switched to the one-walk derivation.
