//! Coach contract — the entire coupling surface between the engine and an
//! optional onboarding coach.
//!
//! Zero logic lives here: only the event the engine hands out after every
//! command, the outcome taxonomy the engine fills in, and the trait an
//! external coach implements. `forgeql-core` gains no new dependency for this.
//! The concrete coach lives in the separate `forgeql-coach` crate, which
//! depends on core — never the reverse. The engine holds
//! `Option<Box<dyn Coach>>`, injected by product entry points after
//! construction; library embedders and the test suites leave it `None` and
//! stay deterministic.

use crate::session::SessionCoords;

/// One executed command, exactly as the engine observed it. Handed to the
/// coach on both the success and the failure path.
pub struct CommandEvent<'a> {
    /// Session identity (source, branch, alias, user). The coach keys learner
    /// state off `coords.budget_branch()`.
    pub coords: &'a SessionCoords,
    /// The command's top-level verb.
    pub verb: Verb,
    /// The clauses the command carried, presence-only.
    pub clauses: Vec<Clause>,
    /// How the command resolved.
    pub outcome: Outcome,
    /// Monotonic per-engine command counter at the time of this event.
    pub cmd_index: u64,
}

/// Top-level command verb — a coarse, language-agnostic classification of the
/// engine's internal op enum, deliberately smaller so the coach never depends
/// on `ForgeQLIR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Use,
    Find,
    Show,
    Change,
    Insert,
    Delete,
    Move,
    Copy,
    Begin,
    Commit,
    Rollback,
    Verify,
    Job,
    Undo,
    Other,
}

/// A clause that rode the command. Presence-only: the coach reasons about
/// which clauses an agent is (or is not) reaching for, not their values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clause {
    Where,
    Having,
    In,
    Exclude,
    OrderBy,
    GroupBy,
    Limit,
    Offset,
    Depth,
    IfRev,
}

/// How a command resolved.
pub enum Outcome {
    /// Succeeded. `capped` is set when the output hit the implicit line cap;
    /// `truncated` when a set was returned only in part (no master rev armed).
    Ok { capped: bool, truncated: bool },
    /// Failed, classified into the coach-facing error taxonomy.
    Err(ErrKind),
}

/// Coach-facing error taxonomy. The engine classifies its type-erased error
/// into one of these before handing the event to the coach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrKind {
    /// A DSL parse failure. `attempted` is the raw statement text.
    ParseError { attempted: String },
    /// A session-dependent command issued with no live session.
    NoSession,
    /// `IF REV` fingerprint did not match the node's current state.
    RevMismatch,
    /// A handle no longer resolves to a node.
    NodeNotFound,
    /// Output hit the implicit line cap.
    OutputCapped,
    /// A `NODES FOUND` verb refused because the arming FIND had no LIMIT.
    FoundRefusedNoLimit,
    /// Session line budget is running low.
    BudgetLow,
    /// Anything the classifier could not place more specifically.
    Other,
}

/// A hint the coach chose to emit. Multi-line is expected (the coach crate
/// enforces its own line budgets); the engine delivers the text verbatim on
/// the outbound response.
pub struct Hint {
    /// The hint body, delivered verbatim.
    pub text: String,
}

/// The coach seam. One method, called once per executed command.
pub trait Coach: Send {
    /// Observe one executed command; optionally return a hint to deliver.
    fn observe(&mut self, ev: &CommandEvent<'_>) -> Option<Hint>;
}
