//! Token-based authentication for the `ForgeQL` MCP endpoint.
//!
//! This is a deliberately small, file-backed shim. Real token issuance
//! (`OAuth` or a managed credential store) is out of scope for now; the
//! `TokenStore` below is the single lookup seam so it can be swapped for a
//! real backend later without touching the HTTP handler. For today, bearer
//! tokens are read from a JSON file at startup and mapped to a `Principal`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Authorisation role attached to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Full access, including source-management commands (CREATE/REFRESH SOURCE).
    Admin,
    /// Read and query access only; source-management commands are rejected.
    Normal,
}

impl Role {
    /// Parse a role name; unknown names fall back to `Normal`.
    fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "admin" => Self::Admin,
            _ => Self::Normal,
        }
    }
}

/// The authenticated identity for a single request.
#[derive(Debug, Clone)]
pub(crate) struct Principal {
    /// User id forwarded to the engine (session ownership / audit).
    pub(crate) user: String,
    /// Authorisation role.
    pub(crate) role: Role,
}

impl Principal {
    /// The identity for an unauthenticated or unknown-token request.
    #[must_use]
    pub(crate) fn anonymous() -> Self {
        Self {
            user: "anonymous".to_string(),
            role: Role::Normal,
        }
    }

    /// True if this principal may run source-management commands.
    #[must_use]
    pub(crate) fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// Maps bearer tokens to principals.
///
/// Backed by a JSON file today; `resolve` is intentionally the only lookup
/// path so a future real backend can replace it wholesale.
#[derive(Debug, Default, Clone)]
pub(crate) struct TokenStore {
    by_token: HashMap<String, Principal>,
}

impl TokenStore {
    /// An empty store: every request resolves to anonymous.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Load tokens from a JSON file shaped as:
    /// `{ "tokens": [ { "token": "...", "user": "admin", "role": "admin" } ] }`.
    pub(crate) fn load_from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading auth file {}", path.display()))?;
        let doc: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parsing auth file {} as JSON", path.display()))?;
        let mut by_token = HashMap::new();
        if let Some(entries) = doc.get("tokens").and_then(Value::as_array) {
            for entry in entries {
                let Some(token) = entry.get("token").and_then(Value::as_str) else {
                    continue;
                };
                if token.is_empty() {
                    continue;
                }
                let user = entry
                    .get("user")
                    .and_then(Value::as_str)
                    .unwrap_or("admin")
                    .to_string();
                let role = entry
                    .get("role")
                    .and_then(Value::as_str)
                    .map_or(Role::Normal, Role::from_name);
                let _ = by_token.insert(token.to_string(), Principal { user, role });
            }
        }
        Ok(Self { by_token })
    }

    /// Resolve a bearer token to a principal. `None` or an unknown token yields
    /// the anonymous principal.
    #[must_use]
    pub(crate) fn resolve(&self, token: Option<&str>) -> Principal {
        token
            .and_then(|t| self.by_token.get(t))
            .cloned()
            .unwrap_or_else(Principal::anonymous)
    }

    /// Number of configured tokens (for startup logging).
    #[must_use]
    pub(crate) fn token_count(&self) -> usize {
        self.by_token.len()
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "test code")]
    use super::*;

    #[test]
    fn anonymous_is_normal_role() {
        let p = Principal::anonymous();
        assert_eq!(p.user, "anonymous");
        assert!(!p.is_admin());
    }

    #[test]
    fn empty_store_resolves_anonymous() {
        let store = TokenStore::empty();
        assert!(!store.resolve(Some("whatever")).is_admin());
        assert_eq!(store.token_count(), 0);
    }

    #[test]
    fn known_admin_token_resolves_admin() {
        let store =
            parse(r#"{ "tokens": [ { "token": "sekret", "user": "root", "role": "admin" } ] }"#);
        let p = store.resolve(Some("sekret"));
        assert!(p.is_admin());
        assert_eq!(p.user, "root");
    }

    #[test]
    fn unknown_or_missing_token_is_anonymous() {
        let store =
            parse(r#"{ "tokens": [ { "token": "sekret", "user": "root", "role": "admin" } ] }"#);
        assert!(!store.resolve(Some("nope")).is_admin());
        assert!(!store.resolve(None).is_admin());
    }

    #[test]
    fn missing_role_defaults_to_normal() {
        let store = parse(r#"{ "tokens": [ { "token": "t", "user": "u" } ] }"#);
        assert!(!store.resolve(Some("t")).is_admin());
    }

    fn parse(json: &str) -> TokenStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("forgeql-auth-test-{}-{n}.json", std::process::id()));
        std::fs::write(&path, json).unwrap();
        let store = TokenStore::load_from_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        store
    }
}
