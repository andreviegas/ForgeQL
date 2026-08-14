- Verifying `IF REV` on a bulk `NODES FOUND` mutation no longer re-walks the
  worktree once per member. Whole-path members are answered from their
  recorded paths — one walk, built only when the set holds a directory, is
  shared by every directory member — so the gate on a directory-heavy set
  costs about one listing where it previously cost members × workspace
  (minutes at 95,000-file scale, the same cliff the arming path had). A
  member deleted out from under an armed set now reads as a set change and
  is refused with the usual re-run-the-FIND recovery, where it previously
  surfaced a raw not-found error. Symbol-handle members still resolve through
  the index, and the per-file content fingerprint is unchanged.
