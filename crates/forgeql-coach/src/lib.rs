//! `forgeql-coach` — adaptive onboarding coach for `ForgeQL`.
//!
//! A temporary bridge that feeds an agent short, just-in-time hints about the
//! `ForgeQL` protocol as it works, re-teaching on evidence of forgetting. The
//! coach is a decoupled add-on: it depends on `forgeql-core` and is injected
//! into the engine by product entry points through the core-owned `Coach`
//! trait. The engine never depends on this crate.
//!
//! Two kinds of hint. A **reactive** hint corrects a concrete failure — a
//! rejected mutation, a parse error — and rides the very response that carried
//! it; it is the primary signal. A **proactive** hint teaches the next protocol
//! skill the learner has not yet shown, ordered by a static curriculum and
//! biased by whether the session is reading or mutating. Proactive hints are
//! rare: they fire below a per-skill recency threshold, on a cooldown, and fall
//! silent entirely once the learner is fluent (dormancy). Two behavioral
//! detectors sit between the two — fragmented `SHOW LINES` reading and reads
//! that keep hitting the line cap. `FORGEQL_COACH=0` disables the coach;
//! `FORGEQL_COACH_DEBUG` surfaces a diagnostic line.

use std::collections::HashMap;
use std::path::PathBuf;

use forgeql_core::coach_api::{Clause, Coach, CommandEvent, ErrKind, Hint, Outcome, Verb};
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

// ---- Tuning ---------------------------------------------------------------

/// A skill stays "known" for this many commands after its last correct use;
/// past that it decays back below threshold and may be re-taught.
const DECAY_WINDOW: u64 = 40;
/// Minimum commands between two proactive hints — keeps them rare.
const PROACTIVE_COOLDOWN: u64 = 4;
/// Do not re-teach the same skill within this many commands.
const TEACH_COOLDOWN: u64 = 25;
/// Consecutive capped reads before the repeated-capping nudge fires.
const CAP_STREAK_LIMIT: u32 = 3;
/// Adjacent `SHOW LINES` reads on one file before the fragmentation nudge.
const FRAGMENT_LIMIT: usize = 3;
/// Two `SHOW LINES` ranges count as adjacent when the gap is at most this.
const ADJACENT_GAP: u32 = 20;

// ---- Learner state --------------------------------------------------------

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
    /// Skill id -> `cmd_index` of its last correct use.
    demonstrated: HashMap<String, u64>,
    /// Skill id -> `cmd_index` of the last error that collapsed it.
    collapsed: HashMap<String, u64>,
    /// `cmd_index` of the last proactive hint (global cooldown).
    last_proactive: u64,
    /// Skill id -> `cmd_index` it was last taught (per-skill cooldown).
    taught: HashMap<String, u64>,
    /// Consecutive capped reads not yet broken by anything else.
    cap_streak: u32,
    /// Recent `SHOW LINES` reads (file fingerprint, start, end).
    recent_reads: Vec<(u64, u32, u32)>,
}

impl LearnerState {
    /// Fold one command's counters and skill mastery into the state. Called
    /// AFTER hint selection, so proactive selection sees the learner as they
    /// were BEFORE this command.
    fn record(&mut self, ev: &CommandEvent<'_>) {
        self.commands_seen = self.commands_seen.saturating_add(1);
        let now = self.commands_seen;
        self.last_cmd_index = ev.cmd_index;
        let failed = matches!(ev.outcome, Outcome::Err(_));
        if failed {
            self.errors_seen = self.errors_seen.saturating_add(1);
        }
        if is_mutation(ev.verb) {
            self.mutation_ops = self.mutation_ops.saturating_add(1);
        } else {
            self.read_ops = self.read_ops.saturating_add(1);
        }
        if !failed {
            // Any observed command comes from a live session — the initial USE
            // is not always observable, so connect is treated as satisfied the
            // moment the coach sees the learner run anything at all.
            let _ = self.demonstrated.insert("connect".to_owned(), now);
        }
        for skill in exercised(ev) {
            if failed {
                // The error implicates the skill this command reached for.
                let _ = self.collapsed.insert(skill.to_owned(), now);
            } else {
                let _ = self.demonstrated.insert(skill.to_owned(), now);
            }
        }
    }

    /// The session's leaning: read-only until its first mutation, then mutation.
    const fn mode(&self) -> Mode {
        if self.mutation_ops == 0 {
            Mode::Research
        } else {
            Mode::Mutation
        }
    }

    /// Whether a skill is currently known: used recently, and not collapsed by
    /// a more recent error.
    fn known(&self, skill: &str, now: u64) -> bool {
        let Some(&seen) = self.demonstrated.get(skill) else {
            return false;
        };
        if self.collapsed.get(skill).is_some_and(|&err| err >= seen) {
            return false;
        }
        now.saturating_sub(seen) < DECAY_WINDOW
    }

    /// Fold a read into the fragmentation tracker; returns a nudge when several
    /// adjacent `SHOW LINES` reads on one file pile up. Only `SHOW LINES`
    /// carries a `read_span`, so nothing else advances this.
    fn note_read(&mut self, ev: &CommandEvent<'_>) -> Option<Hint> {
        let span = ev.read_span?;
        self.recent_reads.push((span.file, span.start, span.end));
        if self.recent_reads.len() > FRAGMENT_LIMIT.saturating_mul(2) {
            let _ = self.recent_reads.remove(0);
        }
        let n = self.recent_reads.len();
        if n < FRAGMENT_LIMIT {
            return None;
        }
        let window = &self.recent_reads[n - FRAGMENT_LIMIT..];
        let file = window[0].0;
        let same_file = window.iter().all(|r| r.0 == file);
        let adjacent = window
            .windows(2)
            .all(|w| w[1].1 <= w[0].2.saturating_add(ADJACENT_GAP));
        if same_file && adjacent {
            self.recent_reads.clear();
            Some(Hint {
                text: FRAGMENTED_READS.to_owned(),
            })
        } else {
            None
        }
    }

    /// Fold a read outcome into the capping tracker; returns a nudge only when
    /// reads are capped again and again without anything breaking the streak.
    fn note_cap(&mut self, ev: &CommandEvent<'_>) -> Option<Hint> {
        if !matches!(ev.outcome, Outcome::Ok { capped: true, .. }) {
            self.cap_streak = 0;
            return None;
        }
        self.cap_streak = self.cap_streak.saturating_add(1);
        if self.cap_streak >= CAP_STREAK_LIMIT {
            self.cap_streak = 0;
            Some(Hint {
                text: REPEATED_CAPPING.to_owned(),
            })
        } else {
            None
        }
    }

    /// The rare proactive nudge: the lowest-tier in-mode skill the learner has
    /// not shown and whose prerequisites they have. Returns the skill id (for
    /// cooldown bookkeeping) with its hint. `None` when cooling down or when
    /// everything in-mode is known — the dormant, fluent-agent case.
    fn proactive(&self, now: u64) -> Option<(&'static str, Hint)> {
        if self.last_proactive != 0 && now.saturating_sub(self.last_proactive) < PROACTIVE_COOLDOWN
        {
            return None;
        }
        let mode = self.mode();
        let mut candidates: Vec<&Concept> = CURRICULUM
            .iter()
            .filter(|c| mode.allows(c.mode))
            .filter(|c| !self.known(c.id, now))
            .filter(|c| c.prereqs.iter().all(|p| self.known(p, now)))
            .filter(|c| {
                self.taught
                    .get(c.id)
                    .is_none_or(|&t| now.saturating_sub(t) >= TEACH_COOLDOWN)
            })
            .collect();
        candidates.sort_by_key(|c| mode.priority(c));
        let concept = candidates.first()?;
        Some((
            concept.id,
            Hint {
                text: concept.hint.to_owned(),
            },
        ))
    }
}

/// Whether a verb writes to the workspace.
const fn is_mutation(verb: Verb) -> bool {
    matches!(
        verb,
        Verb::Change | Verb::Insert | Verb::Delete | Verb::Move | Verb::Copy | Verb::Commit
    )
}

/// The protocol skills a single command demonstrates, keyed off its verb and
/// the clauses it reached for — the only things the coach observes.
fn exercised(ev: &CommandEvent<'_>) -> Vec<&'static str> {
    let has = |clause: Clause| ev.clauses.contains(&clause);
    match ev.verb {
        Verb::Use => vec!["connect"],
        Verb::Find => {
            let mut skills = vec!["locate"];
            if has(Clause::Where) || has(Clause::In) || has(Clause::Limit) {
                skills.push("filter");
            }
            if has(Clause::GroupBy) {
                skills.push("enrich");
            }
            skills
        }
        Verb::Show => {
            let mut skills = vec!["read"];
            if has(Clause::Depth) {
                skills.push("depth");
            }
            skills
        }
        Verb::Change | Verb::Insert | Verb::Delete | Verb::Move | Verb::Copy => {
            let mut skills = vec!["mutate"];
            if has(Clause::IfRev) {
                skills.push("ifrev");
            }
            skills
        }
        Verb::Begin | Verb::Commit | Verb::Rollback | Verb::Verify => vec!["transact"],
        Verb::Job | Verb::Undo | Verb::Other => vec![],
    }
}

// ---- Curriculum -----------------------------------------------------------

/// When a skill is relevant. `Mutation` skills are suppressed while a session is
/// only reading, and float to the front while it is mutating; `Any` skills are
/// eligible in every mode.
#[derive(Clone, Copy)]
enum ConceptMode {
    Any,
    Mutation,
}

/// One proactive teaching unit: a skill, its place in the progression, when it
/// applies, what it depends on, and the (<= 2 line) hint that teaches it.
struct Concept {
    id: &'static str,
    tier: u8,
    mode: ConceptMode,
    prereqs: &'static [&'static str],
    hint: &'static str,
}

/// The static progression: connect -> locate/filter -> read/depth -> enrich ->
/// mutate -> ifrev -> transact. Ordered by tier; mode decides which pool
/// applies and, while mutating, floats the mutation skills to the front.
const CURRICULUM: &[Concept] = &[
    Concept {
        id: "connect",
        tier: 0,
        mode: ConceptMode::Any,
        prereqs: &[],
        hint: "Connected. Locate code with FIND symbols WHERE name LIKE '...' (each row carries a file, a line, and a node handle); read it with SHOW NODE '<id>'.",
    },
    Concept {
        id: "locate",
        tier: 1,
        mode: ConceptMode::Any,
        prereqs: &["connect"],
        hint: "Find before you read: FIND symbols WHERE name LIKE '...' returns each match's file, line, and a stable node handle to read or edit by.",
    },
    Concept {
        id: "filter",
        tier: 2,
        mode: ConceptMode::Any,
        prereqs: &["locate"],
        hint: "Narrow in the query, don't read then grep: stack WHERE fql_kind = 'function', IN 'src/**', and LIMIT before you read.",
    },
    Concept {
        id: "read",
        tier: 3,
        mode: ConceptMode::Any,
        prereqs: &["locate"],
        hint: "Read a located node by handle: SHOW NODE '<id>' returns its full span; SHOW body OF 'name' DEPTH 1 gives a control-flow skeleton.",
    },
    Concept {
        id: "depth",
        tier: 4,
        mode: ConceptMode::Any,
        prereqs: &["read"],
        hint: "Control read cost with DEPTH: 0 = signature + metadata, 1 = control-flow skeleton, 99 = full source (add LIMIT).",
    },
    Concept {
        id: "enrich",
        tier: 5,
        mode: ConceptMode::Any,
        prereqs: &["filter"],
        hint: "Let the index answer instead of grepping: WHERE is_magic = 'true', has_doc = 'false', usages = 0; GROUP BY file for hotspots.",
    },
    Concept {
        id: "mutate",
        tier: 6,
        mode: ConceptMode::Mutation,
        prereqs: &["read"],
        hint: "Edit by node handle — CHANGE / INSERT / DELETE NODE '<id>'. Every mutation returns a boundary diff; read it and fix any seam yourself.",
    },
    Concept {
        id: "ifrev",
        tier: 7,
        mode: ConceptMode::Mutation,
        prereqs: &["mutate"],
        hint: "Every edit to an existing node needs IF REV '<rev>'. The rev rides beside the handle on each FIND/SHOW row and changes after every edit.",
    },
    Concept {
        id: "transact",
        tier: 8,
        mode: ConceptMode::Mutation,
        prereqs: &["mutate"],
        hint: "Group related edits: BEGIN TRANSACTION '...' ... VERIFY build '...' ... COMMIT MESSAGE '...' — atomic, one UNDO step, gated before commit.",
    },
];

/// The session's reading-vs-mutating leaning, which biases the curriculum.
#[derive(Clone, Copy)]
enum Mode {
    Research,
    Mutation,
}

impl Mode {
    /// Whether a skill of the given relevance is taught in this mode.
    const fn allows(self, concept: ConceptMode) -> bool {
        match self {
            Self::Research => !matches!(concept, ConceptMode::Mutation),
            Self::Mutation => true,
        }
    }

    /// Ordering key: while mutating, mutation skills sort ahead of everything —
    /// the IF REV contract before enrichment trivia — otherwise it is tier order.
    fn priority(self, concept: &Concept) -> (u8, u8) {
        let mutation_first =
            matches!(self, Self::Mutation) && matches!(concept.mode, ConceptMode::Mutation);
        (u8::from(!mutation_first), concept.tier)
    }
}

// ---- Reactive & behavioral hint text --------------------------------------

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

/// The repeated-read nudge: several fragmented `SHOW LINES` reads on one file.
const FRAGMENTED_READS: &str = concat!(
    "Several adjacent SHOW LINES on one file — that fragments a read across many calls.\n",
    "Read a whole function in one go with SHOW body OF 'name' DEPTH 99, or address it by handle: ",
    "SHOW NODE '<id>'.",
);

/// The repeated-capping nudge: reads that keep hitting the inline line cap.
const REPEATED_CAPPING: &str = concat!(
    "Reads keep hitting the line cap.\n",
    "SHOW NODE '<id>' returns a whole node uncapped; SHOW MORE HEAD n | TAIL n pages a buffered ",
    "result; or narrow with a tighter DEPTH / WHERE / LIMIT.",
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

/// The debug-mode wiring diagnostic, emitted only when no other hint fires.
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

// ---- Coach ----------------------------------------------------------------

/// The coach: observes every command, updates the learner's on-disk cookie, and
/// returns at most one hint — reactive, behavioral, or proactive, in that order.
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
        let now = state.commands_seen.saturating_add(1);

        // Priority: a concrete failure outranks a behavioral nudge, which
        // outranks the rare proactive lesson. The streak trackers are advanced
        // for their side effects even when a higher-priority hint wins.
        let reactive = reactive_hint(ev);
        let fragmentation = state.note_read(ev);
        let repeated_cap = state.note_cap(ev);

        // Proactive selection reads mastery as of BEFORE this command; record()
        // below then folds this command in, so the next call reflects it.
        let mut taught_skill = None;
        let proactive = if reactive.is_none() && fragmentation.is_none() && repeated_cap.is_none() {
            state.proactive(now).map(|(id, hint)| {
                taught_skill = Some(id);
                hint
            })
        } else {
            None
        };

        state.record(ev);
        if let Some(id) = taught_skill {
            state.last_proactive = now;
            let _ = state.taught.insert(id.to_owned(), now);
        }

        let hint = reactive
            .or(fragmentation)
            .or(repeated_cap)
            .or(proactive)
            .or_else(|| self.debug.then(|| debug_hint(ev, &state, &key)));

        self.store(&key, state);
        hint
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
