- Known defect, now reproduced and written down: an `ORDER BY <field> LIMIT k`
  can return fewer than `k` rows even when far more than `k` distinct rows match.
  While a scan runs, a top-K trim sheds rows down to `k * 2` whenever the working
  set passes `k * 4`, and duplicate rows are only collapsed afterwards. So where
  enough rows agreeing on `name`, `fql_kind`, path and line sort ahead of the
  distinct ones, the retained window can be filled by rows that later become one,
  and distinct rows that belonged in the answer were already discarded. The page
  comes back short without saying so.
- This is stated wherever the ordered form is recommended — the refusal text,
  `doc/syntax.md` and each of the agent guides — because that form is the remedy
  offered for a scan too large to materialise, and recommending it without naming
  its hole is the failure that advice is supposed to prevent. The reproduction is
  an ignored test, `crates/forgeql-core/tests/topk_trim_before_dedupe.rs`, which
  builds the rows directly so the counts are exact; the control beside it asserts
  the same index answers with more distinct rows than the page asks for, so the
  case cannot quietly stop reproducing. Nothing changes in what a query returns:
  the defect is unchanged, only no longer undocumented.
