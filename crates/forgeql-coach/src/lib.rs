//! `forgeql-coach` — adaptive onboarding coach for `ForgeQL`.
//!
//! A temporary bridge that feeds an agent short, just-in-time hints about the
//! `ForgeQL` protocol as it works, re-teaching on evidence of forgetting. The
//! coach is a decoupled add-on: it depends on `forgeql-core` and is injected
//! into the engine by product entry points through the core-owned `Coach`
//! trait. The engine never depends on this crate.
//!
//! This is the dark skeleton — it observes every command and persists
//! per-learner state to a cookie on disk, but emits no hints yet. Setting
//! `FORGEQL_COACH_DEBUG` surfaces a one-line diagnostic, used to prove the
//! observe/persist/deliver wiring end-to-end on a live binary.

use std::collections::HashMap;
use std::path::PathBuf;

use forgeql_core::coach_api::{Coach, CommandEvent, ErrKind, Hint, Outcome, Verb};
use serde::{Deserialize, Serialize};

/// Build a coach from the environment, or `None` when disabled.
///
/// Enabled by default. `FORGEQL_COACH=0|off|false|no` disables it, leaving the
/// engine's coach slot empty and the hot path untouched. When enabled,
/// `data_dir` is the engine's data directory; cookies live under
/// `<data_dir>/coach/`. `FORGEQL_COACH_DEBUG` additionally turns on the
/// diagnostic line.
#[must_use]
pub fn from_env(data_dir: PathBuf) -> Option<Box<dyn Coach>> {
    if let Ok(raw) = std::env::var("FORGEQL_COACH") {
        let v = raw.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "0" | "off" | "false" | "no") {
            return None;
        }
    }
    let debug = std::env::var("FORGEQL_COACH_DEBUG")
        .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false" | "no" | "off"));
    Some(Box::new(ForgeQLCoach::new(data_dir, debug)))
}

/// Per-learner state, persisted as one small JSON cookie per branch.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LearnerState {
    /// Total commands observed for this learner.
    commands_seen: u64,
    /// `cmd_index` of the most recent observed command.
    last_cmd_index: u64,
    /// Total failed commands observed.
    errors_seen: u64,
    /// Commands that mutate the workspace (mode-detection input).
    mutation_ops: u64,
    /// Read-only commands (mode-detection input).
    read_ops: u64,
    /// Per-concept recency: concept id -> `cmd_index` last exercised. The
    /// confidence-decay curve that reads this lands in a later iteration;
    /// today it is recorded but not yet scored.
    concept_last_seen: HashMap<String, u64>,
}

impl LearnerState {
    /// Fold one observed command into the state.
    fn record(&mut self, ev: &CommandEvent<'_>) {
        self.commands_seen = self.commands_seen.saturating_add(1);
        self.last_cmd_index = ev.cmd_index;
        if matches!(ev.outcome, Outcome::Err(_)) {
            self.errors_seen = self.errors_seen.saturating_add(1);
        }
        if is_mutation(ev.verb) {
            self.mutation_ops = self.mutation_ops.saturating_add(1);
        } else {
            self.read_ops = self.read_ops.saturating_add(1);
        }
        let _ = self
            .concept_last_seen
            .insert(concept_id(ev.verb).to_owned(), ev.cmd_index);
    }
}

/// Whether a verb writes to the workspace.
const fn is_mutation(verb: Verb) -> bool {
    matches!(
        verb,
        Verb::Change | Verb::Insert | Verb::Delete | Verb::Move | Verb::Copy | Verb::Commit
    )
}

/// A coarse concept id for a verb — the unit the curriculum tracks.
const fn concept_id(verb: Verb) -> &'static str {
    match verb {
        Verb::Use => "connect",
        Verb::Find => "locate",
        Verb::Show => "read",
        Verb::Change | Verb::Insert | Verb::Delete | Verb::Move | Verb::Copy => "mutate",
        Verb::Begin | Verb::Commit | Verb::Rollback | Verb::Verify => "transact",
        Verb::Job => "gate",
        Verb::Undo => "undo",
        Verb::Other => "other",
    }
}

/// The dark coach: observes, persists, and (only in debug mode) emits a
/// diagnostic. Real teaching lands in later iterations.
struct ForgeQLCoach {
    /// Cookie directory: `<data_dir>/coach`.
    dir: PathBuf,
    /// When set, `observe` returns a one-line diagnostic proving the wiring.
    debug: bool,
    /// In-memory cache of loaded states, keyed by cookie key.
    cache: HashMap<String, LearnerState>,
}

impl ForgeQLCoach {
    fn new(data_dir: PathBuf, debug: bool) -> Self {
        let mut dir = data_dir;
        dir.push("coach");
        Self {
            dir,
            debug,
            cache: HashMap::new(),
        }
    }

    /// Cookie file path for a learner key.
    fn cookie_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitize(key)))
    }

    /// Load a learner's state — from cache, then disk, else default.
    fn load(&mut self, key: &str) -> LearnerState {
        if let Some(state) = self.cache.remove(key) {
            return state;
        }
        std::fs::read_to_string(self.cookie_path(key))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Persist a learner's state to disk (best effort) and re-cache it.
    fn store(&mut self, key: &str, state: LearnerState) {
        if let Ok(raw) = serde_json::to_string(&state) {
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.cookie_path(key), raw);
        }
        let _ = self.cache.insert(key.to_owned(), state);
    }
}

impl Coach for ForgeQLCoach {
    fn observe(&mut self, ev: &CommandEvent<'_>) -> Option<Hint> {
        let key = format!("{}@{}", ev.coords.source, ev.coords.budget_branch());
        let mut state = self.load(&key);
        state.record(ev);
        // Reactive teaching is the primary signal: a failure (or a capped read)
        // is concrete evidence of a protocol gap, so the corrective hint rides
        // the very response that carries it. The debug diagnostic is a fallback
        // for the remaining commands, used to prove the wiring on a live binary.
        let hint = reactive_hint(ev).or_else(|| self.debug.then(|| debug_hint(ev, &state, &key)));
        self.store(&key, state);
        hint
    }
}

/// A short corrective hint for a node handle that no longer resolves.
const NODE_NOT_FOUND: &str = concat!(
    "That handle no longer resolves — the node was deleted, moved, or its ordinal was ",
    "remapped by an earlier edit.\n",
    "Re-locate it before retrying: FIND symbols WHERE name = '...' (or SHOW outline OF '<file>') ",
    "returns the current node_id and rev.",
);

/// A short corrective hint for an `IF REV` fingerprint mismatch.
const REV_MISMATCH: &str = concat!(
    "IF REV mismatch — the node changed since you read it.\n",
    "The error payload carries its current rev, line range, and source; re-target with that rev: ",
    "CHANGE NODE '<id>' IF REV '<current-rev>' WITH '...'.\n",
    "If the change is unexpected, re-read the node first: SHOW NODE '<id>'.",
);

/// A short corrective hint for a bulk `FOUND` verb with no armed set.
const NO_FOUND_SET: &str = concat!(
    "A NODES FOUND verb needs an armed set.\n",
    "Run the selecting FIND first (FIND symbols/usages/files WHERE ...), then re-issue the FOUND ",
    "command in the same session — its response carries the master rev to quote in IF REV.",
);

/// A short corrective hint for a `FOUND` verb over a truncated arming FIND.
const FOUND_TRUNCATED: &str = concat!(
    "The arming FIND was truncated, so no master rev was issued — a FOUND mutation would touch ",
    "rows you never saw.\n",
    "Re-run the FIND with a LIMIT that covers the whole result (or tighter WHERE filters), then ",
    "repeat the FOUND command.",
);

/// A short corrective hint for a `FOUND` verb missing its mandatory `IF REV`.
const FOUND_REFUSED: &str = concat!(
    "A NODES FOUND verb edits every member at once, so it requires IF REV '<master-rev>'.\n",
    "Re-run the arming FIND — its response carries the master rev — then quote it: ",
    "CHANGE NODES FOUND IF REV '<master-rev>' MATCHING 'a' WITH 'b'.",
);

/// The reactive curriculum: map a command outcome to a short recovery hint.
///
/// Pure static data keyed on the coach-facing error taxonomy — no source is
/// inspected. A hint fires only for a self-healing failure; a capped read is
/// left to the `show_more` footer, and a clean success returns `None`.
fn reactive_hint(ev: &CommandEvent<'_>) -> Option<Hint> {
    let text = match &ev.outcome {
        Outcome::Err(ErrKind::ParseError { attempted }) => return Some(parse_hint(attempted)),
        Outcome::Err(ErrKind::RevMismatch) => REV_MISMATCH,
        Outcome::Err(ErrKind::NodeNotFound) => NODE_NOT_FOUND,
        Outcome::Err(ErrKind::NoFoundSet) => NO_FOUND_SET,
        Outcome::Err(ErrKind::FoundTruncated) => FOUND_TRUNCATED,
        Outcome::Err(ErrKind::FoundRefused) => FOUND_REFUSED,
        // A single capped read is already taught by the `show_more` footer that
        // rides the same response, so the coach cedes that topic to it. The
        // repeated-capping case the footer cannot see (capping again and again
        // without ever paging) is a later, stateful detector. BudgetLow has no
        // producer yet; its hint lands with the budget observer.
        Outcome::Ok { .. }
        | Outcome::Err(ErrKind::OutputCapped | ErrKind::BudgetLow | ErrKind::Other) => return None,
    };
    Some(Hint {
        text: text.to_owned(),
    })
}

/// Nearest-verb correction for a statement that did not parse.
fn parse_hint(attempted: &str) -> Hint {
    const VERBS: [&str; 14] = [
        "USE", "FIND", "SHOW", "CHANGE", "INSERT", "DELETE", "MOVE", "COPY", "BEGIN", "COMMIT",
        "ROLLBACK", "VERIFY", "UNDO", "JOB",
    ];
    let first = attempted
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let lead = if first.is_empty() {
        "Empty statement.".to_owned()
    } else if VERBS.contains(&first.as_str()) {
        format!(
            "'{first}' is a valid verb, but the statement did not parse — check clause order \
             (IN -> EXCLUDE -> WHERE -> GROUP BY -> HAVING -> ORDER BY -> OFFSET -> LIMIT) and quoting."
        )
    } else {
        format!("'{first}' is not a ForgeQL verb.")
    };
    Hint {
        text: format!(
            "{lead}\n\
             Statements start with a verb: USE / FIND / SHOW / CHANGE / INSERT / DELETE / MOVE / COPY / BEGIN / COMMIT.\n\
             Connect with USE source.branch AS 'alias'; locate with FIND symbols WHERE name LIKE '...'; read with SHOW NODE '<id>'."
        ),
    }
}

/// The debug-mode wiring diagnostic, emitted only when no reactive hint fires.
fn debug_hint(ev: &CommandEvent<'_>, state: &LearnerState, key: &str) -> Hint {
    Hint {
        text: format!(
            "[coach:debug] cmd #{} verb={:?} outcome={} seen={} errors={} cookie={}",
            ev.cmd_index,
            ev.verb,
            outcome_label(&ev.outcome),
            state.commands_seen,
            state.errors_seen,
            key,
        ),
    }
}
/// Short label for an outcome — used only by the debug diagnostic.
fn outcome_label(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok { capped, truncated } => {
            format!("ok(capped={capped},truncated={truncated})")
        }
        Outcome::Err(kind) => format!("err({kind:?})"),
    }
}

/// Make a learner key safe for use as a file name.
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '@' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
