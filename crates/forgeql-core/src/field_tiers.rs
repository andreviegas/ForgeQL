//! One declaration per queryable field.
//!
//! A field's serving path used to be implied by which of six unrelated const
//! lists happened to name it and which `match` arm in `prefilter_global`
//! happened to match it. Nothing declared the relationship, so nothing could
//! check it, and a disagreement between two of those places was not a compile
//! error but a wrong answer at corpus scale.
//!
//! [`FIELD_TIERS`] is that declaration. Each row records where the value is
//! stored, which structure serves each operator class, whether that structure
//! decides the answer or only proposes candidates, what it cannot see and
//! which mechanism covers that, the budgets bounding it, and the measurement
//! (or the bench class that would produce one).
//!
//! # Scope
//!
//! The table covers every field name `FIND symbols` validation accepts
//! ([`crate::filter::CORE_WHERE_FIELDS`]) together with every field carrying a
//! per-segment or workspace serving structure
//! ([`POSTING_ENRICHMENT_FIELDS`](crate::storage::columnar::POSTING_ENRICHMENT_FIELDS)
//! and `ZONEMAP_NUMERIC_FIELDS`). Language-declared enrichment fields are not
//! enumerable at compile time — the set depends on which language plugins are
//! registered — so they are covered by the single [`CATCH_ALL_FIELD`] row,
//! which states their serving path exactly; the table is total over the
//! universe because of that row, not in spite of it.
//!
//! A field name that belongs to another row type (`FIND files`, `SHOW members`,
//! `SHOW` line rows) appears here only where `CORE_WHERE_FIELDS` names it, and
//! its row describes what that name does on a **symbol** row — which for all
//! of them is [`Tier::Refused`], with [`FieldTier::elsewhere`] naming the verb
//! that does answer it. That family was the defect this table was built to
//! surface: each of those names was accepted and answered a confident zero.
//!
//! # This table is declarative
//!
//! Nothing reads it at query time. The tests in `tests/field_tier_table.rs`
//! assert that it agrees with the const lists and the behaviour it describes,
//! so a disagreement is a test failure rather than a silent wrong answer.

/// Where a field's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Stored per row in the columnar row layout, one slot per row.
    CoreColumn,
    /// Stored in the per-row enrichment extras map, present only on rows an
    /// enricher wrote it to.
    EnrichmentColumn,
    /// Not stored per row: computed workspace-wide and attached at
    /// materialisation (the usage-count aggregate).
    Aggregate,
    /// Not stored per row: taken from the owning segment, which is one file.
    SegmentPath,
    /// Not stored: read out of the source file when the row is materialised.
    MaterialisedText,
    /// A column of another row type — `FIND files`, `FIND globals`, a `SHOW`
    /// line row. A symbol row does not carry it.
    OtherRowType,
    /// Accepted by a validation list and stored nowhere at all.
    Absent,
}

/// One operator class a `WHERE` predicate can fall into.
///
/// Negations are their own classes because the engine treats them separately:
/// `NOT LIKE`/`NOT MATCHES` share the enrichment pattern arm with their
/// positive forms but have no arm at all for `name`, and `!=` has no arm
/// anywhere. Folding them together would make this table lie about `name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `LIKE`
    Like,
    /// `MATCHES`
    Matches,
    /// `NOT LIKE`
    NotLike,
    /// `NOT MATCHES`
    NotMatches,
    /// `<`, `<=`, `>`, `>=`
    Ord,
}

/// Every operator class. A row must account for each exactly once.
pub const ALL_OPS: &[OpClass] = &[
    OpClass::Eq,
    OpClass::Ne,
    OpClass::Like,
    OpClass::Matches,
    OpClass::NotLike,
    OpClass::NotMatches,
    OpClass::Ord,
];

/// The structure that answers a predicate on a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The workspace-global name FST — an exact key lookup, or a regex
    /// automaton intersection over its keys.
    NameFst,
    /// The per-segment short-literal name prefix index.
    NamePrefix,
    /// The trigram index over names.
    Trigram,
    /// The overlay's per-kind row bitmap.
    KindBitmap,
    /// The overlay's `field=value` key set, one key read.
    KeyBitmap,
    /// The overlay's `field=value` key set, walked: the pattern is tested
    /// against each distinct **value** and the matching bitmaps are unioned,
    /// so the cost is one test per distinct value rather than one per row.
    ValueUniverse,
    /// The overlay's numeric enrichment index, range-scanned.
    NumericIndex,
    /// A stored per-row column, compared one slot per row without
    /// materialising a result row for any of them.
    StoredColumn,
    /// Per-segment min/max, used to skip whole segments.
    ZoneMap,
    /// No index: every row is read and handed to the row-level filter. Slow
    /// and complete — the honest default, not a failure.
    Scan,
    /// Validation refuses the predicate rather than answering it.
    ///
    /// A field is refused when no row of the queried shape can carry a value
    /// for it: either nothing in the index stores it (`node_kind`) or the name
    /// belongs to a different row shape entirely (`size`, `declaration`).
    /// Either way the only answer available is a false absence, so the query
    /// errors and [`FieldTier::elsewhere`] names where the field IS answered.
    Refused,
}

/// What follows a tier, and in which of the two possible relationships.
///
/// One field used to carry both, spelled `Option<Tier>`, and the two mean
/// opposite things about whether the first tier's empty answer is an absence.
/// `KeyBitmap → Scan` proposes candidates the scan then decides, so the bitmap
/// alone proves nothing; `NameFst → Trigram` is a substitute for a predicate
/// the FST declined, so the pair is only as exact as the substitute. Written
/// as one `Option`, both read the same and neither could be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    /// Nothing follows. This tier's answer is the answer.
    Nothing,
    /// The tier proposes candidates and this structure decides each one. The
    /// final rows are right either way; what the declared exactness is about
    /// is the tier's OWN empty answer, which a filter cannot rescue — there is
    /// nothing to filter — so a filter leaves that claim exactly as it was.
    Filters(Tier),
    /// The tier may decline the predicate outright; this answers instead. The
    /// pair is only as exact as whichever of the two is weaker.
    Fallback(Tier),
}
/// Whether a tier's answer is the answer, or only a set of candidates.
///
/// This is the field that decides whether an empty result may be reported as
/// an absence. Getting it wrong is not a slow query, it is a wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exactness {
    /// Every row was decided against its own stored value, so the result is
    /// the answer set for the committed rows and an empty one concludes an
    /// absence.
    Exact,
    /// Candidates only. The row-level filter decides, and an empty result
    /// concludes nothing whatsoever.
    Superset,
    /// Candidates only, but an empty result may conclude an absence once the
    /// named per-segment proof has run and passed.
    SupersetProvenAbsent(&'static str),
    /// No tier runs, so the question does not arise.
    NotApplicable,
}

impl Tier {
    /// What this structure alone can conclude from an empty answer.
    ///
    /// The dividing line is whether the structure looked at rows or at keys.
    /// A scan and a stored column compare each row against its own value, so
    /// an empty result is the answer. Everything assembled from keys — a
    /// posting list, a trigram set, a zone map — saw only what was keyed, and
    /// what was not keyed is not what is not there.
    #[must_use]
    pub const fn intrinsic_exactness(self) -> Exactness {
        match self {
            Self::Refused => Exactness::NotApplicable,
            // Two ways to be exact. A scan and a stored column decide every
            // row against that row's own value; a name FST and a kind bitmap
            // are lookups over a key set complete by construction, because
            // every indexed row contributed its name and its kind.
            Self::Scan | Self::StoredColumn | Self::NameFst | Self::KindBitmap => Exactness::Exact,
            // Assembled from keys that a budget, an empty value or a dirty
            // session can leave out.
            Self::NamePrefix
            | Self::Trigram
            | Self::KeyBitmap
            | Self::ValueUniverse
            | Self::NumericIndex
            | Self::ZoneMap => Exactness::Superset,
        }
    }
}

impl Exactness {
    /// The one exactness a `(tier, then)` pair may declare.
    ///
    /// This is a function, not a permission list, and that is the point. The
    /// matrix it replaced allowed `NameFst` under both `Exact` and `Superset`
    /// and so could not catch either being wrong — `name` is served by the FST
    /// for `=` and by the FST-or-trigram pair for a regex, and only one of
    /// those may conclude an absence.
    ///
    /// A `KeyBitmap` may declare [`Exactness::SupersetProvenAbsent`] wherever
    /// this returns [`Exactness::Superset`]: the proof is an additional check,
    /// not a different tier.
    #[must_use]
    pub const fn of(tier: Tier, then: Then) -> Self {
        match then {
            // Nothing else runs, or what runs decides each candidate the tier
            // proposed — either way the claim being made is the tier's own.
            Then::Nothing | Then::Filters(_) => tier.intrinsic_exactness(),
            // The follower may answer the predicate instead, so the pair is
            // only as strong as whichever of the two is weaker.
            Then::Fallback(other) => {
                match (tier.intrinsic_exactness(), other.intrinsic_exactness()) {
                    (Self::Exact, Self::Exact) => Self::Exact,
                    _ => Self::Superset,
                }
            }
        }
    }
}

/// Something a tier cannot see, and the mechanism that covers it.
///
/// A tier with an unnamed gap is the finding: every one of this campaign's
/// defects was a gap nobody had written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Gap {
    /// A file whose distinct-value count for the field exceeded its per-file
    /// posting budget wrote no postings at all, so no key names its rows.
    OverBudgetFile,
    /// The whole field is dropped from the overlay once its workspace-wide
    /// distinct-value count passes its bucket limit — no keys at all, rather
    /// than some.
    OverBudgetWorkspace,
    /// A row whose value for the field is the empty string is keyed nowhere.
    EmptyValue,
    /// Rows of files edited inside the session are not in the committed
    /// overlay the tier reads.
    DirtySession,
    /// A segment whose stored column does not account for its rows
    /// one-for-one cannot be read positionally.
    ShortColumn,
    /// The value does not fit the `i64` a numeric predicate is parsed into.
    AboveI64,
}

impl Gap {
    /// The mechanism that covers this gap, or the plain statement that none
    /// does.
    #[must_use]
    pub const fn fallback(self) -> &'static str {
        match self {
            Self::OverBudgetFile => {
                "fast_paths::rows_missing_field_postings adds the file's rows back"
            }
            Self::OverBudgetWorkspace => "the tier returns None and the complete row scan runs",
            Self::EmptyValue => {
                "the pattern tier stands aside when the pattern accepts the empty string"
            }
            Self::DirtySession => "the dirty overlay is materialised separately and unioned in",
            Self::ShortColumn => "the column is not read and the complete row scan runs",
            Self::AboveI64 => "none: no predicate can match such a row, on any tier or on the scan",
        }
    }
}

/// A cost measurement, or the reason there is not one.
///
/// The A/B harness reports a 200 ms floor that is pipe overhead rather than
/// query time, so `ms_per_query: 200` means "at or below the floor".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measured {
    /// Never measured, and no bench class exists to measure it with. A new
    /// tier arriving in this state is what P7 exists to catch.
    Unmeasured,
    /// A bench class exists; the number is outstanding.
    Pending {
        /// The `bench_ab` class that will produce the number.
        bench_class: &'static str,
    },
    /// Measured on the reference corpus.
    At {
        /// Milliseconds per query, at or below a 200 ms harness floor.
        ms_per_query: u32,
        /// Symbols in the corpus the number was taken on.
        corpus_symbols: u32,
        /// ISO date of the measurement.
        on: &'static str,
        /// The `bench_ab` class that produced it, so re-measuring is one
        /// command rather than an archaeology exercise.
        bench_class: &'static str,
    },
}

/// Which structure serves which operator classes on one field, how exactly,
/// and at what measured cost.
#[derive(Debug, Clone, Copy)]
pub struct Serving {
    /// The operator classes this entry covers. Across a field's entries these
    /// partition [`ALL_OPS`] — every class named exactly once.
    pub ops: &'static [OpClass],
    /// The structure that answers them.
    pub tier: Tier,
    /// What follows the primary structure, and how — see [`Then`]. This is
    /// half of what fixes `exactness`: a filter leaves the primary's own
    /// exactness intact, a fallback drags the pair down to the weaker of the
    /// two, and `Nothing` leaves the primary alone with the answer.
    pub then: Then,
    /// Whether the pair's answer is the answer — determined, not chosen: see
    /// [`Exactness::of`].
    pub exactness: Exactness,
    /// The cost of asking, on the reference corpus.
    pub measured: Measured,
}

/// The distinct-value budgets bounding a field's keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Distinct values one file may carry before it writes no postings at
    /// all. Mirrors `segment_builder::posting_budget`.
    pub per_file: usize,
    /// Distinct values the workspace may carry before the field is dropped
    /// from the overlay. Mirrors `segment_builder::overlay_budget`.
    pub per_workspace: usize,
}

/// One queryable field: where it lives, what serves it, what that cannot see.
#[derive(Debug, Clone, Copy)]
pub struct FieldTier {
    /// The canonical field name as an agent writes it.
    pub field: &'static str,
    /// Other names the DSL accepts for the same field.
    pub aliases: &'static [&'static str],
    /// The stored column behind the field when its name differs — the zone
    /// maps are keyed by column, and `usages` is stored as `usages_count`.
    pub column: Option<&'static str>,
    /// Where the value comes from.
    pub source: Source,
    /// Serving per operator class.
    pub serving: &'static [Serving],
    /// What the serving structures cannot see. Empty is a claim, not an
    /// omission: it says this field's tiers see everything.
    pub gaps: &'static [Gap],
    /// The distinct-value budgets, for fields whose keys have any.
    pub budget: Option<Budget>,
    /// Where the name IS answered, for a field this row refuses.
    ///
    /// A refusal that only says no leaves the agent to guess; every refused
    /// row either names the verb or clause that does answer the name, or says
    /// `None` because nothing does. The refusal messages are built from this,
    /// so the table is what an agent is told, not a second description of it.
    pub elsewhere: Option<&'static str>,
    /// Whether `GROUP BY` is what populates the value.
    ///
    /// `count` is the only such field: it is written onto a row by the
    /// grouping pass, so `HAVING` and `ORDER BY` — which run after it — read a
    /// real number, while a `WHERE`, which runs before it, reads nothing on
    /// every row and matches none of them. One name, answerable in two clauses
    /// and refused in the third.
    pub post_group: bool,
}

impl FieldTier {
    /// Whether `name` addresses this field, canonically or by alias.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        self.field == name || self.aliases.contains(&name)
    }

    /// Whether every operator class on this field is refused as a predicate
    /// before the grouping pass, and as a grouping key.
    ///
    /// True for `count` too — the serving column describes a predicate, and
    /// before grouping `count` is empty on every row. See
    /// [`is_refused_everywhere`](FieldTier::is_refused_everywhere) for the
    /// stronger claim.
    #[must_use]
    pub fn is_refused(&self) -> bool {
        self.serving.iter().all(|s| s.tier == Tier::Refused)
    }

    /// Whether this field is refused in every clause, `HAVING` and `ORDER BY`
    /// included.
    ///
    /// [`is_refused`](FieldTier::is_refused) alone is not that claim: it is
    /// true for `count`, which a `WHERE` cannot answer and a `HAVING` can.
    #[must_use]
    pub fn is_refused_everywhere(&self) -> bool {
        self.is_refused() && !self.post_group
    }

    /// The error an agent sees for a refused field, in the clause that named
    /// it and on the verb it was written against.
    ///
    /// `written` is the spelling the agent used, which may be an alias: an
    /// error that silently renames the field the agent typed is an error about
    /// a different query.
    #[must_use]
    pub fn refusal(&self, written: &str, clause: &str, verb: &str) -> String {
        let Some(elsewhere) = self.elsewhere else {
            return format!(
                "{clause} {written} cannot be answered on {verb}: {written} is an \
                 internal storage column and no clause can name it."
            );
        };
        let why = match self.source {
            Source::Absent => {
                "no indexed row stores it at all, so the query could only report absence"
            }
            Source::Aggregate => {
                "no row carries it until GROUP BY writes it, and WHERE runs before that"
            }
            _ => "no row of this result carries it, so the query could only report absence",
        };
        format!("{clause} {written} cannot be answered on {verb}: {why}. Use {elsewhere}.")
    }
}

/// Every operator class answered by the complete row scan — no index, and no
/// gap either, since a scan reads every row.
const SCAN_ALL: &[Serving] = &[Serving {
    ops: ALL_OPS,
    tier: Tier::Scan,
    then: Then::Nothing,
    exactness: Exactness::Exact,
    measured: Measured::Unmeasured,
}];

/// Every operator class refused at validation.
const REFUSED_ALL: &[Serving] = &[Serving {
    ops: ALL_OPS,
    tier: Tier::Refused,
    then: Then::Nothing,
    exactness: Exactness::NotApplicable,
    measured: Measured::Unmeasured,
}];

/// `=` on an enrichment field reads the one `field=value` key. An empty
/// bitmap is candidates-none, not answer-none, until the per-segment proof
/// says the value is carried by no segment at all.
const ENRICH_EQ: Serving = Serving {
    ops: &[OpClass::Eq],
    tier: Tier::KeyBitmap,
    then: Then::Filters(Tier::Scan),
    exactness: Exactness::SupersetProvenAbsent("fast_paths::no_segment_carries_enrichment_value"),
    measured: Measured::Unmeasured,
};

/// The pattern operators walk the same keys and test each distinct value.
const ENRICH_PATTERN: Serving = Serving {
    ops: &[
        OpClass::Like,
        OpClass::Matches,
        OpClass::NotLike,
        OpClass::NotMatches,
    ],
    tier: Tier::ValueUniverse,
    then: Then::Filters(Tier::Scan),
    exactness: Exactness::Superset,
    measured: Measured::Unmeasured,
};

/// Range operators read the overlay's numeric enrichment index.
const ENRICH_ORD: Serving = Serving {
    ops: &[OpClass::Ord],
    tier: Tier::NumericIndex,
    then: Then::Filters(Tier::Scan),
    exactness: Exactness::Superset,
    measured: Measured::Unmeasured,
};

/// `!=` has no arm anywhere in `prefilter_global`; it scans.
const ENRICH_NE: Serving = Serving {
    ops: &[OpClass::Ne],
    tier: Tier::Scan,
    then: Then::Nothing,
    exactness: Exactness::Exact,
    measured: Measured::Unmeasured,
};

/// The serving shape shared by every enrichment field.
const ENRICHMENT_SERVING: &[Serving] = &[ENRICH_EQ, ENRICH_PATTERN, ENRICH_ORD, ENRICH_NE];

/// What an enrichment field's keys cannot see when they come from the segment
/// posting lists.
const POSTED_GAPS: &[Gap] = &[
    Gap::OverBudgetFile,
    Gap::OverBudgetWorkspace,
    Gap::EmptyValue,
    Gap::DirtySession,
];

/// What they cannot see when they come from the complete row walk instead.
/// The per-file gap is absent by construction: a row walk visits every row.
const ROW_WALK_GAPS: &[Gap] = &[Gap::OverBudgetWorkspace, Gap::EmptyValue, Gap::DirtySession];

/// A posted enrichment field with no measurement of its own.
const fn posted(field: &'static str, per_file: usize, per_workspace: usize) -> FieldTier {
    FieldTier {
        field,
        aliases: &[],
        column: None,
        source: Source::EnrichmentColumn,
        serving: ENRICHMENT_SERVING,
        gaps: POSTED_GAPS,
        budget: Some(Budget {
            per_file,
            per_workspace,
        }),
        elsewhere: None,
        post_group: false,
    }
}

/// `name` is the only field with a different structure per operator class.
const NAME_SERVING: &[Serving] = &[
    Serving {
        ops: &[OpClass::Eq],
        tier: Tier::NameFst,
        then: Then::Nothing,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
    Serving {
        ops: &[OpClass::Like],
        tier: Tier::NamePrefix,
        then: Then::Fallback(Tier::Trigram),
        exactness: Exactness::Superset,
        measured: Measured::Unmeasured,
    },
    Serving {
        ops: &[OpClass::Matches],
        tier: Tier::NameFst,
        then: Then::Fallback(Tier::Trigram),
        exactness: Exactness::Superset,
        measured: Measured::At {
            ms_per_query: 200,
            corpus_symbols: 3_062_139,
            on: "2026-08-07",
            bench_class: "name_matches",
        },
    },
    Serving {
        ops: &[
            OpClass::Ne,
            OpClass::NotLike,
            OpClass::NotMatches,
            OpClass::Ord,
        ],
        tier: Tier::Scan,
        then: Then::Nothing,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
];

/// `fql_kind` has a bitmap for `=` and nothing else.
const FQL_KIND_SERVING: &[Serving] = &[
    Serving {
        ops: &[OpClass::Eq],
        tier: Tier::KindBitmap,
        then: Then::Nothing,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
    Serving {
        ops: &[
            OpClass::Ne,
            OpClass::Like,
            OpClass::Matches,
            OpClass::NotLike,
            OpClass::NotMatches,
            OpClass::Ord,
        ],
        tier: Tier::Scan,
        then: Then::Nothing,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
];

/// `language` is compared against its own stored column, one integer per row,
/// which is why it is the one field whose tier may conclude an absence
/// without a proof: every row was decided, not proposed.
const LANGUAGE_SERVING: &[Serving] = &[
    Serving {
        ops: &[
            OpClass::Eq,
            OpClass::Ne,
            OpClass::Like,
            OpClass::Matches,
            OpClass::NotLike,
            OpClass::NotMatches,
        ],
        tier: Tier::StoredColumn,
        then: Then::Filters(Tier::Scan),
        exactness: Exactness::Exact,
        measured: Measured::At {
            ms_per_query: 200,
            corpus_symbols: 3_062_139,
            on: "2026-08-07",
            bench_class: "core_eq",
        },
    },
    Serving {
        ops: &[OpClass::Ord],
        tier: Tier::Scan,
        then: Then::Nothing,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
];

/// Zone maps prune whole segments for range operators and for `=`; nothing
/// prunes for the pattern operators.
const ZONE_MAP_SERVING: &[Serving] = &[
    Serving {
        ops: &[OpClass::Eq, OpClass::Ord],
        tier: Tier::ZoneMap,
        then: Then::Filters(Tier::Scan),
        exactness: Exactness::Superset,
        measured: Measured::Unmeasured,
    },
    Serving {
        ops: &[
            OpClass::Ne,
            OpClass::Like,
            OpClass::Matches,
            OpClass::NotLike,
            OpClass::NotMatches,
        ],
        tier: Tier::Scan,
        then: Then::Nothing,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
];

/// `guard_kind` carries the measurement that motivated the whole campaign:
/// an `=` on it read every row before it had a tier.
const GUARD_KIND_SERVING: &[Serving] = &[
    Serving {
        measured: Measured::At {
            ms_per_query: 200,
            corpus_symbols: 3_062_139,
            on: "2026-08-06",
            bench_class: "guard_kind_eq",
        },
        ..ENRICH_EQ
    },
    ENRICH_PATTERN,
    ENRICH_ORD,
    ENRICH_NE,
];

/// `guard_mentions` was posted without a number: its class is queued behind a
/// corpus rewarm, and the table says so rather than leaving the cell blank.
const GUARD_MENTIONS_SERVING: &[Serving] = &[
    Serving {
        measured: Measured::Pending {
            bench_class: "guard_mentions_eq",
        },
        ..ENRICH_EQ
    },
    ENRICH_PATTERN,
    ENRICH_ORD,
    ENRICH_NE,
];

/// `guard_defines` is the field the membership recipe is written against, so
/// the outstanding measurement is on its pattern operators.
const GUARD_DEFINES_SERVING: &[Serving] = &[
    ENRICH_EQ,
    Serving {
        measured: Measured::Pending {
            bench_class: "guard_defines_matches",
        },
        ..ENRICH_PATTERN
    },
    ENRICH_ORD,
    ENRICH_NE,
];

/// A field the symbol query refuses outright.
///
/// `source` records why — [`Source::Absent`] when nothing stores the value at
/// all, [`Source::OtherRowType`] when the name belongs to a different row
/// shape — and `elsewhere` names where it IS answered, so the refusal can
/// point somewhere instead of only saying no.
const fn refused(
    field: &'static str,
    aliases: &'static [&'static str],
    source: Source,
    elsewhere: Option<&'static str>,
) -> FieldTier {
    FieldTier {
        field,
        aliases,
        column: None,
        source,
        serving: REFUSED_ALL,
        gaps: &[],
        budget: None,
        elsewhere,
        post_group: false,
    }
}

/// Every field with a declared serving path.
///
/// In the order an agent meets them: the core row fields, then the enrichment
/// fields carrying posting lists, then the one row standing for every other
/// enrichment field.
///
/// The list is total over the universe `FIND symbols` validation accepts.
/// [`CATCH_ALL_FIELD`] is what makes that true for the language-declared
/// enrichment fields, which cannot be enumerated at compile time.
pub const FIELD_TIERS: &[FieldTier] = &[
    // ── Core row fields ──────────────────────────────────────────────────
    FieldTier {
        field: "name",
        aliases: &[],
        column: None,
        source: Source::CoreColumn,
        serving: NAME_SERVING,
        gaps: &[Gap::EmptyValue, Gap::DirtySession],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "fql_kind",
        aliases: &["kind"],
        column: None,
        source: Source::CoreColumn,
        serving: FQL_KIND_SERVING,
        gaps: &[Gap::DirtySession],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "language",
        aliases: &["lang"],
        column: Some("col_language_id"),
        source: Source::CoreColumn,
        serving: LANGUAGE_SERVING,
        gaps: &[Gap::ShortColumn, Gap::DirtySession],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "path",
        aliases: &["file"],
        column: None,
        source: Source::SegmentPath,
        // `IN` and `EXCLUDE` prune segments by path before any of this runs;
        // a `WHERE` on the path itself has no structure behind it.
        serving: SCAN_ALL,
        gaps: &[],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "line",
        aliases: &[],
        column: Some("line"),
        source: Source::CoreColumn,
        serving: ZONE_MAP_SERVING,
        gaps: &[Gap::DirtySession],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "usages",
        aliases: &[],
        column: Some("usages_count"),
        source: Source::Aggregate,
        serving: ZONE_MAP_SERVING,
        gaps: &[Gap::DirtySession],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "node_id",
        aliases: &[],
        column: None,
        source: Source::CoreColumn,
        serving: SCAN_ALL,
        gaps: &[],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "body",
        aliases: &[],
        column: None,
        // Read out of the file when the row is materialised, so a predicate
        // on it decides after the read rather than narrowing what is read.
        source: Source::MaterialisedText,
        serving: SCAN_ALL,
        gaps: &[],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "role",
        aliases: &[],
        column: None,
        // Written onto an occurrence row by the read pass that finds the site,
        // from the line's own text and the posting that labels it. Not an
        // index column, so nothing narrows a predicate on it: the row-level
        // filter decides, and it decides every row.
        source: Source::MaterialisedText,
        serving: SCAN_ALL,
        gaps: &[],
        budget: None,
        elsewhere: None,
        post_group: false,
    },
    // `value` and `type` are named in CORE_WHERE_FIELDS but resolve through
    // the enrichment extras like any other enrichment column, so they are
    // served — and keyed — exactly as the catch-all row describes.
    FieldTier {
        field: "value",
        aliases: &[],
        column: None,
        source: Source::EnrichmentColumn,
        serving: ENRICHMENT_SERVING,
        gaps: ROW_WALK_GAPS,
        budget: Some(Budget {
            per_file: 8,
            per_workspace: 64,
        }),
        elsewhere: None,
        post_group: false,
    },
    FieldTier {
        field: "type",
        aliases: &[],
        column: None,
        source: Source::EnrichmentColumn,
        serving: ENRICHMENT_SERVING,
        gaps: ROW_WALK_GAPS,
        budget: Some(Budget {
            per_file: 8,
            per_workspace: 64,
        }),
        elsewhere: None,
        post_group: false,
    },
    // ── Refused: no symbol row can carry these, and the query says so ─────
    //
    // Each of these was a live false absence on `FIND symbols`: validation
    // waved it through because `CORE_WHERE_FIELDS` is a union across every row
    // type, the symbol row could not resolve it, and the query answered a
    // confident zero. They are refused instead, with a message naming the
    // field and `elsewhere` — because zero rows is a claim about the corpus
    // and an error is a fact about the query.
    //
    // The raw tree-sitter kind is computed during parsing and kept by no row
    // of the columnar index, so it can only ever be reported absent. Serving
    // it would mean storing it, which is an index-output change. The refusal
    // lives in the columnar backend's own Stage 0 rather than at the engine,
    // because the legacy in-memory index does store it per row and filters on
    // it correctly — this table describes the indexed backend, which is the
    // one every session queries.
    refused(
        "node_kind",
        &[],
        Source::Absent,
        Some("fql_kind, the universal kind, which is stored"),
    ),
    refused(
        "text",
        &["content"],
        Source::OtherRowType,
        Some("SHOW body / SHOW NODE '<id>' WHERE text MATCHES '…', whose rows are source lines"),
    ),
    // Two zone-mapped columns with no query field at all: the zone maps prune
    // segments for node-span lookups, and no clause may name them. They are
    // declared so the zone-map list and this table agree as sets.
    FieldTier {
        column: Some("byte_start"),
        ..refused("byte_start", &[], Source::CoreColumn, None)
    },
    FieldTier {
        column: Some("byte_end"),
        ..refused("byte_end", &[], Source::CoreColumn, None)
    },
    // `count` is the one name that is refused in one clause and answered in
    // two: see `FieldTier::post_group`.
    FieldTier {
        post_group: true,
        ..refused(
            "count",
            &[],
            Source::Aggregate,
            Some(
                "HAVING count or ORDER BY count, which run after the grouping pass that writes it",
            ),
        )
    },
    refused(
        "size",
        &[],
        Source::OtherRowType,
        Some("FIND files, whose rows carry a byte size"),
    ),
    refused(
        "depth",
        &[],
        Source::OtherRowType,
        Some("FIND files and SHOW outline, whose rows carry a depth"),
    ),
    refused(
        "extension",
        &["ext"],
        Source::OtherRowType,
        Some("FIND files, whose rows carry an extension"),
    ),
    refused(
        "signature",
        &[],
        Source::OtherRowType,
        Some("SHOW signature OF '<name>', which renders one"),
    ),
    refused(
        "marker",
        &[],
        Source::OtherRowType,
        Some("SHOW body / SHOW NODE '<id>', whose line rows carry a marker"),
    ),
    refused(
        "declaration",
        &[],
        Source::OtherRowType,
        Some("SHOW members OF '<type>', whose rows carry a declaration"),
    ),
    // ── Enrichment fields carrying per-segment posting lists ─────────────
    //
    // The budgets are repeated here rather than computed from
    // `segment_builder::posting_budget`, and the repetition is the point: the
    // agreement test calls that function for every row and fails on the first
    // disagreement, which is what a parallel declaration buys before the
    // lists collapse into views over this one.
    posted("has_doc", 8, 64),
    posted("is_recursive", 8, 64),
    posted("has_fallthrough", 8, 64),
    posted("is_const", 8, 64),
    posted("is_mutable", 8, 64),
    posted("is_unsafe", 8, 64),
    posted("is_async", 8, 64),
    posted("is_generic", 8, 64),
    posted("has_todo", 8, 64),
    posted("is_exported", 8, 64),
    posted("has_catch_all", 8, 64),
    posted("has_escape", 8, 64),
    posted("has_shadow", 8, 64),
    posted("expanded_has_escape", 8, 64),
    posted("expansion_failed", 8, 64),
    posted("cast_style", 8, 64),
    posted("cast_safety", 8, 64),
    posted("scope", 8, 64),
    posted("binding_kind", 8, 64),
    posted("naming", 8, 64),
    posted("comment_style", 8, 64),
    posted("member_kind", 8, 64),
    posted("for_style", 8, 64),
    posted("escape_tier", 8, 64),
    posted("storage", 8, 64),
    posted("operator_category", 8, 64),
    posted("guard_branch", 8, 64),
    posted("catch_all_kind", 8, 64),
    posted("shift_direction", 8, 64),
    posted("increment_op", 8, 64),
    posted("increment_style", 8, 64),
    FieldTier {
        serving: GUARD_KIND_SERVING,
        ..posted("guard_kind", 8, 64)
    },
    // The five wide fields: tens of thousands of distinct values corpus-wide,
    // posted under a budget sized for that so `=` and the pattern operators
    // have a tier at all. What is keyed is the WHOLE value — for the guard
    // sets, the comma-joined string, which is what the row-level filter
    // compares an `=` against.
    FieldTier {
        serving: GUARD_DEFINES_SERVING,
        ..posted("guard_defines", 4096, 65_536)
    },
    FieldTier {
        serving: GUARD_MENTIONS_SERVING,
        ..posted("guard_mentions", 4096, 65_536)
    },
    posted("guard_negates", 4096, 65_536),
    FieldTier {
        // Its values are u64 hashes, and a numeric predicate is parsed into
        // an i64: a row whose group id is above i64::MAX cannot be matched by
        // any range operator, on any tier or on the scan.
        gaps: &[
            Gap::OverBudgetFile,
            Gap::OverBudgetWorkspace,
            Gap::EmptyValue,
            Gap::DirtySession,
            Gap::AboveI64,
        ],
        ..posted("guard_group_id", 4096, 65_536)
    },
    posted("key_path", 4096, 65_536),
    // ── Every other enrichment field ─────────────────────────────────────
    CATCH_ALL_FIELD,
];

/// The row standing for every language-declared enrichment field the list
/// above does not name.
///
/// Which fields those are depends on which language plugins are registered,
/// so they cannot be enumerated at compile time — but their serving path does
/// not vary, and this states it. Their keys come from walking every canonical
/// row rather than from segment posting lists, so the per-file budget gap
/// cannot apply to them: a row walk visits every row.
pub const CATCH_ALL_FIELD: FieldTier = FieldTier {
    field: "*",
    aliases: &[],
    column: None,
    source: Source::EnrichmentColumn,
    serving: ENRICHMENT_SERVING,
    gaps: ROW_WALK_GAPS,
    budget: Some(Budget {
        per_file: 8,
        per_workspace: 64,
    }),
    elsewhere: None,
    post_group: false,
};

/// The declared serving path for `field`, by canonical name or alias, falling
/// back to [`CATCH_ALL_FIELD`] for a language-declared enrichment field the
/// table does not name.
///
/// `None` means the table does not name it: either a language-declared
/// enrichment field, served exactly as [`CATCH_ALL_FIELD`] states, or a name
/// no `FIND symbols` query can use at all. Validation tells those two apart;
/// this table does not.
#[must_use]
pub fn lookup(field: &str) -> Option<&'static FieldTier> {
    FIELD_TIERS
        .iter()
        .find(|t| t.field != CATCH_ALL_FIELD.field && t.matches(field))
}

/// The canonical spelling of `field`, or `field` unchanged when the table does
/// not know the name.
///
/// Two spellings the DSL documents as identical must be answered identically,
/// and this is what makes that mechanical: every place that compares a clause
/// field to a canonical name compares this instead, so an alias added to
/// [`FIELD_TIERS`] reaches all of them at once. The alternative — teaching
/// each comparison its own alias list — is how `WHERE kind = 'guard'` came to
/// answer zero where `WHERE fql_kind = 'guard'` answered three, on the same
/// file and the same field.
///
/// The written spelling is what the result is LABELLED with; only the routing
/// is canonicalised. `GROUP BY file` still heads its key column `file`.
#[must_use]
pub fn canonical(field: &str) -> &str {
    lookup(field).map_or(field, |tier| tier.field)
}

/// Every field the symbol query refuses outright, in table order.
pub fn refused_fields() -> impl Iterator<Item = &'static FieldTier> {
    FIELD_TIERS.iter().filter(|tier| tier.is_refused())
}
