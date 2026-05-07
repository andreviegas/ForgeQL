use anyhow::Result;

use crate::ir::Backend;

use super::{LegacyMemoryStorage, StorageEngine};

/// Owns one or more storage backends for a single session.
///
/// Encapsulates the decision of which backend serves a given [`Backend`]
/// selector. Today this is `legacy` always-present + `columnar` optional;
/// Phase 09 will flip the default and eventually drop `legacy`.
///
/// Having a single `BackendSet` field on [`Session`] instead of two
/// `Box<dyn StorageEngine>` fields makes Phase 09 legacy-retirement a
/// one-struct change — only `BackendSet` needs updating, and all callers
/// continue using `engine()` / `engine_for()` unchanged.
///
/// [`Session`]: crate::session::Session
pub struct BackendSet {
    legacy: LegacyMemoryStorage,
    columnar: Option<Box<dyn StorageEngine>>,
}

impl BackendSet {
    /// Create a `BackendSet` with only the legacy backend installed.
    #[must_use]
    pub fn new(legacy: LegacyMemoryStorage) -> Self {
        Self {
            legacy,
            columnar: None,
        }
    }

    /// Builder-style helper: install a columnar backend at construction time.
    #[must_use]
    pub fn with_columnar(mut self, columnar: Box<dyn StorageEngine>) -> Self {
        self.columnar = Some(columnar);
        self
    }

    /// Install (or replace) the columnar backend after construction.
    pub fn set_columnar(&mut self, columnar: Box<dyn StorageEngine>) {
        self.columnar = Some(columnar);
    }

    /// Returns `true` if a columnar backend is installed.
    #[must_use]
    pub fn has_columnar(&self) -> bool {
        self.columnar.is_some()
    }

    /// The default backend used when no `USING` clause is present.
    ///
    /// Phase 05.3: returns the legacy engine.
    /// Phase 09: returns columnar when configured as the default.
    #[must_use]
    pub fn default_engine(&self) -> &dyn StorageEngine {
        &self.legacy
    }

    /// Mutable access to the default backend (used for reindex / persist).
    pub fn default_engine_mut(&mut self) -> &mut dyn StorageEngine {
        &mut self.legacy
    }

    /// Backend-aware lookup.
    ///
    /// - [`Backend::Default`] / [`Backend::Legacy`] → the legacy engine.
    /// - [`Backend::Columnar`] → the columnar engine, if installed.
    ///
    /// # Errors
    /// Returns `Err` when [`Backend::Columnar`] is requested but no columnar
    /// engine has been installed (i.e. `columnar.shadow_write` is not set in
    /// `.forgeql.yaml`).
    pub fn engine_for(&self, backend: &Backend) -> Result<&dyn StorageEngine> {
        match backend {
            Backend::Default | Backend::Legacy => Ok(&self.legacy),
            Backend::Columnar => self.columnar.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "columnar backend is not enabled for this session; \
                     enable columnar.shadow_write in .forgeql.yaml"
                )
            }),
        }
    }

    /// Return a reference to the legacy backend.
    ///
    /// Returns `None` in Phase 09+ when the default is no longer legacy.
    #[must_use]
    pub const fn legacy_storage(&self) -> Option<&LegacyMemoryStorage> {
        Some(&self.legacy)
    }

    /// Return a mutable reference to the legacy backend.
    ///
    /// Returns `None` in Phase 09+ when the default is no longer legacy.
    pub const fn legacy_storage_mut(&mut self) -> Option<&mut LegacyMemoryStorage> {
        Some(&mut self.legacy)
    }
}
