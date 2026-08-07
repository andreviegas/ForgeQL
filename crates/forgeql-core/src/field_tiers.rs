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
//! A field name that belongs to another row type (`FIND files`, `FIND
//! globals`, `SHOW` line rows) appears here only where `CORE_WHERE_FIELDS`
//! names it, and its row describes what that name does on a **symbol** row —
//! which for most of them is [`Tier::Unserved`], the defect family this table
//! was built to surface.
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
    /// Validation rejects the predicate rather than answering it.
    Refused,
    /// Accepted by validation, carried by no symbol row, and therefore matched
    /// by nothing: a confident wrong answer. Every row declaring this is a
    /// known defect, pinned by a test in `tests/field_tier_table.rs`.
    Unserved,
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
    /// The structure that takes over when the primary stands aside — a short
    /// literal falling through to trigrams, a regex the FST cannot walk, a
    /// field with no keys. `None` means the primary always answers.
    pub then: Option<Tier>,
    /// Whether that structure's answer is the answer.
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
}

impl FieldTier {
    /// Whether `name` addresses this field, canonically or by alias.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        self.field == name || self.aliases.contains(&name)
    }
}

/// Every operator class answered by the complete row scan — no index, and no
/// gap either, since a scan reads every row.
const SCAN_ALL: &[Serving] = &[Serving {
    ops: ALL_OPS,
    tier: Tier::Scan,
    then: None,
    exactness: Exactness::Exact,
    measured: Measured::Unmeasured,
}];

/// Every operator class refused at validation.
const REFUSED_ALL: &[Serving] = &[Serving {
    ops: ALL_OPS,
    tier: Tier::Refused,
    then: None,
    exactness: Exactness::NotApplicable,
    measured: Measured::Unmeasured,
}];

/// Every operator class silently matching nothing — the defect family.
const UNSERVED_ALL: &[Serving] = &[Serving {
    ops: ALL_OPS,
    tier: Tier::Unserved,
    then: None,
    exactness: Exactness::NotApplicable,
    measured: Measured::Unmeasured,
}];

/// `=` on an enrichment field reads the one `field=value` key. An empty
/// bitmap is candidates-none, not answer-none, until the per-segment proof
/// says the value is carried by no segment at all.
const ENRICH_EQ: Serving = Serving {
    ops: &[OpClass::Eq],
    tier: Tier::KeyBitmap,
    then: Some(Tier::Scan),
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
    then: Some(Tier::Scan),
    exactness: Exactness::Superset,
    measured: Measured::Unmeasured,
};

/// Range operators read the overlay's numeric enrichment index.
const ENRICH_ORD: Serving = Serving {
    ops: &[OpClass::Ord],
    tier: Tier::NumericIndex,
    then: Some(Tier::Scan),
    exactness: Exactness::Superset,
    measured: Measured::Unmeasured,
};

/// `!=` has no arm anywhere in `prefilter_global`; it scans.
const ENRICH_NE: Serving = Serving {
    ops: &[OpClass::Ne],
    tier: Tier::Scan,
    then: None,
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
    }
}

/// `name` is the only field with a different structure per operator class.
const NAME_SERVING: &[Serving] = &[
    Serving {
        ops: &[OpClass::Eq],
        tier: Tier::NameFst,
        then: None,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
    Serving {
        ops: &[OpClass::Like],
        tier: Tier::NamePrefix,
        then: Some(Tier::Trigram),
        exactness: Exactness::Superset,
        measured: Measured::Unmeasured,
    },
    Serving {
        ops: &[OpClass::Matches],
        tier: Tier::NameFst,
        then: Some(Tier::Trigram),
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
        then: None,
        exactness: Exactness::Exact,
        measured: Measured::Unmeasured,
    },
];

/// `fql_kind` has a bitmap for `=` and nothing else.
const FQL_KIND_SERVING: &[Serving] = &[
    Serving {
        ops: &[OpClass::Eq],
        tier: Tier::KindBitmap,
        then: None,
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
        then: None,
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
        then: Some(Tier::Scan),
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
        then: None,
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
        then: Some(Tier::Scan),
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
        then: None,
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
    },
    FieldTier {
        field: "fql_kind",
        aliases: &["kind"],
        column: None,
        source: Source::CoreColumn,
        serving: FQL_KIND_SERVING,
        gaps: &[Gap::DirtySession],
        budget: None,
    },
    FieldTier {
        field: "language",
        aliases: &["lang"],
        column: Some("col_language_id"),
        source: Source::CoreColumn,
        serving: LANGUAGE_SERVING,
        gaps: &[Gap::ShortColumn, Gap::DirtySession],
        budget: None,
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
    },
    FieldTier {
        field: "line",
        aliases: &[],
        column: Some("line"),
        source: Source::CoreColumn,
        serving: ZONE_MAP_SERVING,
        gaps: &[Gap::DirtySession],
        budget: None,
    },
    FieldTier {
        field: "usages",
        aliases: &[],
        column: Some("usages_count"),
        source: Source::Aggregate,
        serving: ZONE_MAP_SERVING,
        gaps: &[Gap::DirtySession],
        budget: None,
    },
    FieldTier {
        field: "node_id",
        aliases: &[],
        column: None,
        source: Source::CoreColumn,
        serving: SCAN_ALL,
        gaps: &[],
        budget: None,
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
    },
    // ── Refused: accepted nowhere, and told so ───────────────────────────
    FieldTier {
        field: "node_kind",
        aliases: &[],
        column: None,
        // The raw tree-sitter kind is computed during parsing and kept by no
        // row of the columnar index, so it can only ever be reported absent.
        // Refusing the predicate is the whole of its serving path until
        // something stores it, which is an index-output change. The refusal
        // lives in the columnar backend's own Stage 0 rather than at the
        // engine, because the legacy in-memory index does store it per row
        // and filters on it correctly — this table describes the indexed
        // backend, which is the one every session queries.
        //
        // Refused in WHERE, ORDER BY and GROUP BY on `FIND symbols`, `FIND
        // globals`, `FIND usages` and `FIND files`. `SHOW outline`, `SHOW
        // members` and `SHOW callees` still accept it and drop every row in
        // silence: those three answer with a JSON value rather than a Result,
        // so refusing there is a signature change that belongs with the wider
        // decision about the other unserved names below.
        source: Source::Absent,
        serving: REFUSED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "text",
        aliases: &["content"],
        column: None,
        source: Source::OtherRowType,
        serving: REFUSED_ALL,
        gaps: &[],
        budget: None,
    },
    // Two zone-mapped columns with no query field at all: the zone maps prune
    // segments for node-span lookups, and validation refuses a WHERE on them
    // because they are neither core WHERE fields nor enrichment columns.
    // They are declared so the zone-map list and this table agree as sets.
    FieldTier {
        field: "byte_start",
        aliases: &[],
        column: Some("byte_start"),
        source: Source::CoreColumn,
        serving: REFUSED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "byte_end",
        aliases: &[],
        column: Some("byte_end"),
        source: Source::CoreColumn,
        serving: REFUSED_ALL,
        gaps: &[],
        budget: None,
    },
    // ── Unserved: accepted, carried by no symbol row, matching nothing ───
    //
    // Each of these is a live false absence on `FIND symbols`: validation
    // waves it through because CORE_WHERE_FIELDS is a union across every row
    // type, the symbol row cannot resolve it, and the query answers a
    // confident zero. `count` is meant as an alias of `usages`; the rest
    // belong to `FIND files`, `FIND globals` or `SHOW` line rows. They are
    // declared rather than fixed here because refusing or aliasing these
    // documented field names is a contract decision. Each is pinned by a
    // behavioural test: WHERE still answers a confident zero, GROUP BY on the
    // same name now errors rather than fabricating one empty-named group, and
    // both halves are pinned, so the day either is fixed this table must be
    // updated with it.
    FieldTier {
        field: "count",
        aliases: &[],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "size",
        aliases: &[],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "depth",
        aliases: &[],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "extension",
        aliases: &["ext"],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "signature",
        aliases: &[],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "marker",
        aliases: &[],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
    FieldTier {
        field: "declaration",
        aliases: &[],
        column: None,
        source: Source::OtherRowType,
        serving: UNSERVED_ALL,
        gaps: &[],
        budget: None,
    },
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
