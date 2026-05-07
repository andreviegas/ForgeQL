//! Unit tests for [`BackendSet`] (Phase 05.3 / 05.4).
//!
//! Four cases exercise the core invariants without any real I/O:
//! - `new_yields_legacy_only` — default engine works; `Columnar` request errors.
//! - `with_columnar_round_trip` — builder install + retrieve via `engine_for`.
//! - `engine_for_default_equals_legacy` — `Backend::Default` routes to legacy.
//! - `set_columnar_replaces` — second `set_columnar` overwrites the first.

use std::sync::Arc;

use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::ir::Backend;
use forgeql_core::storage::{BackendSet, LegacyMemoryStorage, StubColumnarStorage};

// -----------------------------------------------------------------------
// Helpers — tiny stub backends so we don't need a real LanguageRegistry.
// -----------------------------------------------------------------------

fn make_backend_set() -> BackendSet {
    let registry = Arc::new(LanguageRegistry::new(vec![]));
    BackendSet::new(LegacyMemoryStorage::new(registry))
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn new_yields_legacy_only() {
    let bs = make_backend_set();
    // Default engine is accessible (result unused deliberately).
    let _ = bs.default_engine();
    // Columnar request must error when none is installed.
    let result = bs.engine_for(&Backend::Columnar);
    assert!(result.is_err(), "expected Err for missing columnar backend");
    if let Err(e) = result {
        let msg = e.to_string();
        assert!(
            msg.contains("columnar backend is not enabled"),
            "unexpected error message: {msg}"
        );
    }
    assert!(!bs.has_columnar());
}

#[test]
fn with_columnar_round_trip() {
    let bs = make_backend_set().with_columnar(Box::new(StubColumnarStorage));
    assert!(bs.has_columnar());
    // engine_for(Columnar) must succeed after install.
    assert!(bs.engine_for(&Backend::Columnar).is_ok());
}

#[test]
fn engine_for_default_equals_legacy() {
    let bs = make_backend_set();
    // Both Default and Legacy should resolve without error.
    assert!(bs.engine_for(&Backend::Default).is_ok());
    assert!(bs.engine_for(&Backend::Legacy).is_ok());
}

#[test]
fn set_columnar_replaces() {
    let mut bs = make_backend_set();
    assert!(!bs.has_columnar());
    bs.set_columnar(Box::new(StubColumnarStorage));
    assert!(bs.has_columnar());
    // Install a second time — should not panic.
    bs.set_columnar(Box::new(StubColumnarStorage));
    assert!(bs.has_columnar());
    // Columnar engine still reachable.
    assert!(bs.engine_for(&Backend::Columnar).is_ok());
}
