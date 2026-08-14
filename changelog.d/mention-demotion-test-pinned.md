- The regression test guarding symbol resolution against uncommitted edits now
  builds its fixture from an edit that really produces a reference row — a Rust
  struct literal in a session-created file — and asserts that row exists in the
  index before asserting anything about resolution. The previous fixture edit, a
  bodyless C++ enum reference, indexes no row at all, so the test kept passing
  even with the mention demotion it guards removed. Behaviour of the engine is
  unchanged; only the test's evidence is.
