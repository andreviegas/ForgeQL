/// Request context — identity and permissions for every plugin call.
///
/// DESIGN: This struct is threaded through every `Query`, `Transform`, and
/// `Verifier` trait method from day one. In Phases 1-4 the permission is always
/// `Permission::Admin` and implementations do not check it. In Phase E the
/// `Permission` enum gains a `Scoped` variant and `check_permission()` is
/// injected into each method body — a **local** change, not a signature refactor.
use anyhow::{bail, Result};

// -----------------------------------------------------------------------
// Permission
// -----------------------------------------------------------------------

/// Permission level for the request.
///
/// `Admin` = full access, no checks performed. Used in all phases before E.
/// Phase E will add: `Scoped { role: String, grants: Vec<Grant> }`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Permission {
    /// Full unrestricted access — no permission checks are performed.
    Admin,
    // Phase E will add:
    // Scoped { role: String, grants: Vec<Grant> }
}

// -----------------------------------------------------------------------
// RequestContext
// -----------------------------------------------------------------------

/// Carries session identity and permissions for a single plugin invocation.
///
/// Every `trait Query`, `Transform`, and `Verifier` method receives `&RequestContext`
/// as its first argument. This ensures that adding access control in Phase E
/// requires no trait signature changes — only a `check_permission()` call inside
/// each method body.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Unique ID for this server session.
    pub session_id: String,
    /// The user who owns the session.
    pub user_id: String,
    /// What operations this context is allowed to perform.
    pub permission: Permission,
}

impl RequestContext {
    /// Create a fully-privileged context for the local/CLI case.
    ///
    /// This is the only constructor used in Phases 1-4. Sessions introduced
    /// in Phase B will populate real `session_id` and `user_id` values.
    #[must_use]
    pub fn admin() -> Self {
        Self {
            session_id: "local".to_string(),
            user_id: "local".to_string(),
            permission: Permission::Admin,
        }
    }

    /// Create a context with a specific user identity (still Admin permission).
    #[must_use]
    pub fn with_user(user_id: impl Into<String>) -> Self {
        Self {
            session_id: uuid_like(),
            user_id: user_id.into(),
            permission: Permission::Admin,
        }
    }

    /// Check whether this context permits the given operation on a path.
    ///
    /// In Phases 1-4, `Admin` always succeeds. Phase E will implement
    /// path-scoped GRANT/DENY logic here without changing the call sites.
    ///
    /// # Errors
    /// Returns `Err` if the permission level denies the requested operation.
    /// In Phases 1-4 this never fails (always `Admin`).
    #[allow(clippy::missing_const_for_fn)]
    pub fn check_permission(&self, _operation: &str, _path: &str) -> Result<()> {
        match &self.permission {
            Permission::Admin => Ok(()),
            // Phase E: Scoped { grants, .. } => { check grants against operation + path }
            #[allow(unreachable_patterns)]
            _ => bail!("permission denied"),
        }
    }
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

/// Generate a simple pseudo-unique ID without pulling in a uuid crate.
/// Phase B will replace this with a proper session registry.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("s-{t:08x}")
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_context_always_permits() {
        let ctx = RequestContext::admin();
        assert!(ctx.check_permission("transform", "src/foo.cpp").is_ok());
        assert!(ctx.check_permission("commit", "src/foo.cpp").is_ok());
    }

    #[test]
    fn with_user_sets_user_id() {
        let ctx = RequestContext::with_user("alice");
        assert_eq!(ctx.user_id, "alice");
        assert_eq!(ctx.permission, Permission::Admin);
    }

    #[test]
    fn admin_context_has_local_defaults() {
        let ctx = RequestContext::admin();
        assert_eq!(ctx.user_id, "local");
        assert_eq!(ctx.session_id, "local");
    }
}
