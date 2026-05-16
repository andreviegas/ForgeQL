//! Authentication stub — the single source of truth for user identity.
//!
//! # Today
//! All production contexts resolve to `"anonymous"`.  The test harness uses
//! `"fql_tester"` so test sessions are distinguishable in logs and the session
//! map from real (future) sessions.
//!
//! # Future — `forgeql-server`
//! Replace [`auth`] with a real resolver that extracts `user_id` from the
//! connection's bearer token (JWT) or API key.  Every call site already
//! receives a `user_id: &str` variable — only this function body changes.
//!
//! ```text
//! // Today
//! pub fn auth(_context: AuthContext) -> &'static str { "anonymous" }
//!
//! // Future (async, returns Result)
//! pub async fn auth(token: &BearerToken) -> Result<String, AuthError> {
//!     jwt::validate(token).map(|claims| claims.sub)
//! }
//! ```

/// Authentication context — identifies which entry point is resolving the
/// current user identity.
///
/// Each variant maps to one birth point for `user_id` in the system:
/// - [`AuthContext::Mcp`]     — an MCP tool call received over HTTP/SSE
/// - [`AuthContext::Cli`]     — a command arriving through the CLI/pipe runner
/// - [`AuthContext::Session`] — a session being restored at engine startup
/// - [`AuthContext::Tester`]  — a test exercising the engine directly
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthContext {
    /// MCP tool call (HTTP/SSE transport).
    Mcp,
    /// CLI / pipe-mode command.
    Cli,
    /// Engine startup — restoring a session from disk.
    Session,
    /// In-process test harness (`#[cfg(feature = "test-helpers")]`).
    Tester,
}

/// Resolve the current user identity for the given authentication context.
///
/// # Today
/// Returns `"anonymous"` for all production contexts and `"fql_tester"` for
/// the test harness.  The string `"anonymous"` **only appears here** — all
/// other code receives it as the `user_id` variable.
///
/// # Future
/// Replace this function with a token validator.  Call sites remain unchanged:
/// they already use a `user_id` variable, never a hard-coded literal.
#[must_use]
pub const fn auth(context: AuthContext) -> &'static str {
    match context {
        AuthContext::Tester => "fql_tester",
        AuthContext::Mcp | AuthContext::Cli | AuthContext::Session => "anonymous",
    }
}
