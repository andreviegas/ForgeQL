//! What the engine reports to the onboarding coach.
//!
//! No coach output reaches the result, the control flow, or a mutation. Every
//! entry point here is handed an outcome that has already been decided —
//! `observe_command` takes `&Result<ForgeQLResult>`, not an opportunity to
//! change it — and its job is to describe that outcome as a `CommandEvent`.
//! What comes back is an optional hint the caller pairs with the result for
//! display: the coach can change what the user is told, never what the engine
//! did.
//!
//! One asymmetry is worth knowing about, because it predates this module:
//! `observe_parse_error` bumps `commands_served` only when a coach is
//! installed, so that counter reads differently depending on whether one is.
//!
//! The coach is also entirely optional: `ForgeQLEngine::new` never builds one,
//! so library embedders and the test suites stay coach-free and deterministic.

use anyhow::Result;

use crate::{
    coach_api::{Clause, Coach, CommandEvent, ErrKind, Outcome, ReadSpan, Verb},
    engine::ForgeQLEngine,
    error::{ForgeError, RejectionKind},
    ir::{Clauses, ForgeQLIR},
    result::ForgeQLResult,
    session::SessionCoords,
};

impl ForgeQLEngine {
    /// Inject an onboarding coach. Product entry points call this after
    /// construction; `ForgeQLEngine::new` never builds one, so library
    /// embedders and the test suites stay coach-free and deterministic.
    pub fn set_coach(&mut self, coach: Box<dyn Coach>) {
        self.coach = Some(coach);
    }

    /// Hand the just-executed command to the coach — on both the success and
    /// the failure path — returning any hint it produces so the caller can pair
    /// it with the result.
    pub(super) fn observe_command(
        &mut self,
        coords: Option<&SessionCoords>,
        op: &ForgeQLIR,
        dispatched: &Result<ForgeQLResult>,
    ) -> Option<String> {
        let coords = coords?;
        // The inline line cap is applied at the render boundary, after this
        // point, so `output_capped` recomputes it here from the line count and
        // the session's configured cap — the coach must see capping at observe
        // time, on every transport, not only where a footer is later attached.
        let cap = self.session_inline_cap(&coords.map_key());
        let outcome = match dispatched {
            Ok(result) => Outcome::Ok {
                capped: result.output_capped(cap),
                truncated: result.output_truncated(),
            },
            Err(err) => Outcome::Err(Self::classify_rejection(err)),
        };
        let ev = CommandEvent {
            coords,
            verb: Self::verb_of(op),
            clauses: Self::clauses_of(op),
            outcome,
            cmd_index: self.commands_served,
            read_span: Self::read_span_of(op),
        };
        self.coach
            .as_mut()
            .and_then(|c| c.observe(&ev))
            .map(|hint| hint.text)
    }

    /// Observe a statement that failed to parse before it could be executed.
    ///
    /// Parse errors never reach [`Self::execute`] — the transport parses first
    /// and rejects — so this lets the coach still see them (the primary teaching
    /// signal) and return a corrective hint the transport attaches to the error
    /// response. A no-op without a coach or without session coords.
    pub fn observe_parse_error(
        &mut self,
        coords: Option<&SessionCoords>,
        attempted: &str,
    ) -> Option<String> {
        let coords = coords?;
        if self.coach.is_some() {
            self.commands_served += 1;
            let ev = CommandEvent {
                coords,
                verb: Verb::Other,
                clauses: Vec::new(),
                outcome: Outcome::Err(ErrKind::ParseError {
                    attempted: attempted.to_owned(),
                }),
                cmd_index: self.commands_served,
                read_span: None,
            };
            self.coach
                .as_mut()
                .and_then(|c| c.observe(&ev))
                .map(|hint| hint.text)
        } else {
            None
        }
    }

    /// Map an op to its coarse coach verb.
    const fn verb_of(op: &ForgeQLIR) -> Verb {
        match op {
            ForgeQLIR::UseSource { .. } => Verb::Use,
            ForgeQLIR::FindSymbols { .. }
            | ForgeQLIR::FindUsages { .. }
            | ForgeQLIR::FindFiles { .. }
            | ForgeQLIR::FindNode { .. } => Verb::Find,
            ForgeQLIR::ShowSources
            | ForgeQLIR::ShowBranches
            | ForgeQLIR::ShowVersion
            | ForgeQLIR::ShowStats { .. }
            | ForgeQLIR::ShowNode { .. }
            | ForgeQLIR::ShowContext { .. }
            | ForgeQLIR::ShowSignature { .. }
            | ForgeQLIR::ShowOutline { .. }
            | ForgeQLIR::ShowMembers { .. }
            | ForgeQLIR::ShowBody { .. }
            | ForgeQLIR::ShowCallees { .. }
            | ForgeQLIR::ShowLines { .. }
            | ForgeQLIR::ShowMore { .. }
            | ForgeQLIR::ShowDiff { .. } => Verb::Show,
            ForgeQLIR::ChangeNode { .. }
            | ForgeQLIR::ChangeNodeMatching { .. }
            | ForgeQLIR::ChangeNodesFound { .. }
            | ForgeQLIR::ChangeContent { .. } => Verb::Change,
            ForgeQLIR::InsertNode { .. } | ForgeQLIR::InsertNodeFor { .. } => Verb::Insert,
            ForgeQLIR::DeleteNode { .. } | ForgeQLIR::DeleteNodesFound { .. } => Verb::Delete,
            ForgeQLIR::MoveNode { .. }
            | ForgeQLIR::MoveNodeTo { .. }
            | ForgeQLIR::MoveNodesFoundTo { .. }
            | ForgeQLIR::MoveLines { .. } => Verb::Move,
            ForgeQLIR::CopyNodeTo { .. }
            | ForgeQLIR::CopyNodesFoundTo { .. }
            | ForgeQLIR::CopyLines { .. } => Verb::Copy,
            ForgeQLIR::BeginTransaction { .. } => Verb::Begin,
            ForgeQLIR::Commit { .. } => Verb::Commit,
            ForgeQLIR::Rollback { .. } => Verb::Rollback,
            ForgeQLIR::VerifyBuild { .. } => Verb::Verify,
            ForgeQLIR::JobStart { .. } | ForgeQLIR::JobStatus { .. } | ForgeQLIR::JobList => {
                Verb::Job
            }
            ForgeQLIR::Undo { .. } => Verb::Undo,
            _ => Verb::Other,
        }
    }

    /// Presence of read-verb clauses (WHERE, IN, LIMIT, DEPTH, …). Mutation
    /// clauses are added when the curriculum begins consuming them.
    fn clauses_of(op: &ForgeQLIR) -> Vec<Clause> {
        let (ForgeQLIR::FindSymbols { clauses, .. }
        | ForgeQLIR::FindUsages { clauses, .. }
        | ForgeQLIR::FindFiles { clauses, .. }
        | ForgeQLIR::ShowNode { clauses, .. }
        | ForgeQLIR::ShowContext { clauses, .. }
        | ForgeQLIR::ShowSignature { clauses, .. }
        | ForgeQLIR::ShowOutline { clauses, .. }
        | ForgeQLIR::ShowMembers { clauses, .. }
        | ForgeQLIR::ShowBody { clauses, .. }
        | ForgeQLIR::ShowCallees { clauses, .. }
        | ForgeQLIR::ShowLines { clauses, .. }
        | ForgeQLIR::ShowMore { clauses, .. }) = op
        else {
            return Vec::new();
        };
        Self::clauses_present(clauses)
    }

    /// The read span for a raw `SHOW LINES` command, so the coach can detect
    /// fragmented reading. `None` for every other op: the coach then sees no read
    /// target and infers nothing about what was read.
    fn read_span_of(op: &ForgeQLIR) -> Option<ReadSpan> {
        // FNV-1a: small and restart-stable, so a file's fingerprint survives the
        // server restart the coach cookie persists across.
        const fn fnv1a(bytes: &[u8]) -> u64 {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            let mut i = 0;
            while i < bytes.len() {
                hash ^= bytes[i] as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                i += 1;
            }
            hash
        }
        let ForgeQLIR::ShowLines {
            file,
            start_line,
            end_line,
            ..
        } = op
        else {
            return None;
        };
        Some(ReadSpan {
            file: fnv1a(file.as_bytes()),
            start: u32::try_from(*start_line).unwrap_or(u32::MAX),
            end: u32::try_from(*end_line).unwrap_or(u32::MAX),
        })
    }

    /// Translate a populated `Clauses` into presence flags for the coach.
    fn clauses_present(c: &Clauses) -> Vec<Clause> {
        let mut v = Vec::new();
        if !c.where_predicates.is_empty() {
            v.push(Clause::Where);
        }
        if !c.having_predicates.is_empty() {
            v.push(Clause::Having);
        }
        if c.in_glob.is_some() {
            v.push(Clause::In);
        }
        if !c.exclude_globs.is_empty() {
            v.push(Clause::Exclude);
        }
        if c.order_by.is_some() {
            v.push(Clause::OrderBy);
        }
        if c.group_by.is_some() {
            v.push(Clause::GroupBy);
        }
        if c.limit.is_some() {
            v.push(Clause::Limit);
        }
        if c.offset.is_some() {
            v.push(Clause::Offset);
        }
        if c.depth.is_some() {
            v.push(Clause::Depth);
        }
        v
    }

    /// Classify a type-erased engine error into the coach taxonomy. Parse
    /// failures never reach here — the transport rejects them before `execute`.
    fn classify_rejection(err: &anyhow::Error) -> ErrKind {
        match err.downcast_ref::<ForgeError>() {
            Some(ForgeError::Rejection { kind, .. }) => match kind {
                RejectionKind::RevMismatch => ErrKind::RevMismatch,
                RejectionKind::NodeNotFound => ErrKind::NodeNotFound,
                RejectionKind::NoFoundSet => ErrKind::NoFoundSet,
                RejectionKind::FoundTruncated => ErrKind::FoundTruncated,
                RejectionKind::FoundRefused => ErrKind::FoundRefused,
                // NoSession is a precondition/handshake denial, out of coach scope.
                RejectionKind::NoSession => ErrKind::Other,
            },
            Some(ForgeError::DslParse(attempted)) => ErrKind::ParseError {
                attempted: attempted.clone(),
            },
            _ => ErrKind::Other,
        }
    }
}
