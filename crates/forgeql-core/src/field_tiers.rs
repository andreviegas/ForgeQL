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
//! which mechanism covers that, the budgets bounding it, the measurement
//! (or the bench class that would produce one), who owns the set of values
//! the field can take — the corpus, whose unstored values answer empty, or the
//! engine, whose unknown values are refused ([`ValueUniverse`]) — and, for a
//! field written only when it holds, what a row it examined and did not write
//! answers and which rows it examines ([`StampDefault`]).
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
//! # This table is checked, and eight questions are asked of it
//!
//! Most of it nothing reads while a query runs: the tests in
//! `tests/field_tier_table.rs` assert that it agrees with the const lists and
//! the behaviour it describes, so a disagreement is a test failure rather than
//! a silent wrong answer. Eight things ARE read while the engine runs:
//!
//! - [`canonical`] resolves an alias to the field it spells.
//! - The refusal family decides a decline and then words it:
//!   [`FieldTier::is_refused`] and [`FieldTier::is_refused_everywhere`] are the
//!   decision — the second also filters the field list a refusal offers as a
//!   suggestion — and [`FieldTier::refusal`] is the text the agent is shown.
//! - [`FieldTier::post_group`], reached through [`lookup`], says which CLAUSE
//!   accepts a field: `count` is refused in `WHERE` and answered after
//!   grouping because of that one flag.
//! - [`lookup`] is asked a second and different question by
//!   `filter::is_known_symbol_field`: not what a field does, but whether the
//!   NAME is one at all. It is how a misspelling is told apart from a real
//!   field that simply matched nothing, so `legacy::resolve` and
//!   `exec_show` word the two situations differently — and a field present in
//!   this table but in neither `SymbolMatch` list is real on that answer
//!   alone. Sharing an entry point with the bullet above is exactly why this
//!   one went uncounted through four review rounds.
//! - [`written_after_materialisation`] has TWO readers, and they are not the
//!   same question. `segment_reader::row_field` is the predicate one, where
//!   getting it wrong runs a filter later than it had to;
//!   `segment_reader::ranks_field_like_a_built_row` is the `ORDER BY` and
//!   top-K one, where getting it wrong ranks rows by an absence the built row
//!   does not share and sheds rows that belonged in the answer.
//! - [`engine_owned_values`] decides whether an unrecognised VALUE is a
//!   refusal or an honest empty answer.
//! - The constants those universes are built from — `ROLE_CODE`, `ROLE_TEXT`,
//!   `UNKNOWN_KIND`, `CAST_KIND`, `MACRO_CALL_KIND` — are read by the passes
//!   that MINT those values, so a value a pass stamps and the set a `WHERE` on
//!   that field is refused against cannot drift apart. Some of those sites are
//!   read passes (`query/find.rs`, `ast/show/members.rs`, `query/outline.rs`)
//!   and some are index passes (`ast/enrich/casts.rs`,
//!   `ast/index/file_indexer/rows.rs`), and the coupling is the same in both:
//!   scoping this entry to query time is one of the ways it has already been
//!   undercounted. These are references at the writing site rather than
//!   lookups, which is why they belong on the list and are the easiest to miss
//!   when counting readers. `file_indexer`'s own reference to
//!   [`FQL_KIND_VALUES`] is NOT one of them and does not belong here: it sits
//!   in a `#[cfg(test)]` module, asserts a subset, and mints nothing.
//! - [`stamp_default`], with [`is_stamp_default_value`] beside it, says what a
//!   row the enricher examined and did not write answers.
//!
//! The last does more than describe. Every other reader answers a question
//! about a NAME, a VALUE or a CLAUSE; `stamp_default` decides which STRUCTURE
//! serves the query — `=` on a declared default is routed to
//! [`Tier::StampDefault`] and every other value is not, and a non-negated
//! pattern that accepts the default makes the pattern tier stand aside
//! altogether. So for one value of four fields this table is not parallel to
//! the serving path but part of it, and what it claims about that value is a
//! claim about the code that runs.
//!
//! **This list is maintained by hand and nothing enforces it.** A ninth
//! reader would fail no test. Grep `field_tiers::` under
//! `crates/forgeql-core/src` before relying on it being complete: this list
//! has been read as closed at four, at five, at six and at seven, and was
//! wrong every time — the seventh count survived review and then lost two
//! minting sites and `is_known_symbol_field` to the first run of the grep this
//! paragraph prescribes. Eight is what the last such run returned,
//! not a promise that the run after it will agree. Read every hit before
//! counting it, too: that same run offered a `#[cfg(test)]` assertion as a
//! reader, and it took a second pair of eyes to throw it back out. The
//! sentence you are reading is the only thing standing in for a check, and it
//! is worth exactly what a sentence is worth.

/// Where a field's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Stored per row in the columnar row layout, one slot per row.
    CoreColumn,
    /// Stored in the per-row enrichment extras map, present only on rows an
    /// enricher wrote it to.
    EnrichmentColumn,
    /// Not stored per row: computed workspace-wide and attached at
    /// materialisation (the usage-count aggregate). Keyed by NAME, so it is
    /// attached to every row EXCEPT one whose name does not identify it across
    /// the workspace — a local-scope variable, which is served with no value
    /// for the field at all (`SymbolMatch::drop_meaningless_usage_count`), and
    /// therefore matches no predicate on it, ranks behind every row that has a
    /// value, and renders an empty metric column.
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

    /// The rows a stamp-only field's DECLARED DEFAULT can speak for, assembled
    /// by `fast_paths::stamp_default_candidates`: the kind bitmaps of the
    /// declaration's applicable kinds, unioned with the rows of segments that
    /// posted nothing for the field, intersected with the rows of segments
    /// written in one of its applicable languages.
    ///
    /// It is not [`Tier::KeyBitmap`] with a different value. No row stores the
    /// default, so the `field=value` keys are silent about it in both
    /// directions, and the per-segment proof `KeyBitmap` leans on to turn an
    /// empty answer into an absence is declined for exactly this value. Reading
    /// absence off the keys here would answer zero rows on every corpus.
    StampDefault,

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
            // session can leave out — and, for `StampDefault`, from key sets
            // that are complete and still propose too much: the kinds it unions
            // include the rows that DO carry the field, and those must not
            // answer the default. Either way the row-level evaluator is what
            // decides, and this tier's own empty answer settles nothing.
            Self::NamePrefix
            | Self::Trigram
            | Self::KeyBitmap
            | Self::ValueUniverse
            | Self::NumericIndex
            | Self::ZoneMap
            | Self::StampDefault => Exactness::Superset,
        }
    }

    /// The one function that assembles this tier's candidates, where the tier
    /// IS one function rather than a family of call sites.
    ///
    /// A tier name in this table is a claim about which code runs, and for most
    /// tiers the claim is about a structure — the name FST, the numeric index —
    /// reached from several places, with nothing single to name. Where one
    /// function is the whole tier, naming it is what lets the table test check
    /// the claim against the source rather than trust this comment, exactly as
    /// [`Exactness::SupersetProvenAbsent`]'s prover is checked.
    #[must_use]
    pub const fn implemented_by(self) -> Option<&'static str> {
        match self {
            Self::StampDefault => Some("fast_paths::stamp_default_candidates"),
            Self::NameFst
            | Self::NamePrefix
            | Self::Trigram
            | Self::KindBitmap
            | Self::KeyBitmap
            | Self::ValueUniverse
            | Self::NumericIndex
            | Self::StoredColumn
            | Self::ZoneMap
            | Self::Scan
            | Self::Refused => None,
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
    /// The value a stamp-only field's declaration answers for an examined row
    /// that carries nothing is keyed nowhere: no row stores it, so no key names
    /// the rows it speaks for. The postings say nothing about it in either
    /// direction, which is not the same as saying it is absent.
    StampDefaultValue,
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
            Self::StampDefaultValue => {
                "`=` is answered by fast_paths::stamp_default_candidates instead of by the \
                 keys, and the pattern tier stands aside — returning None, so the complete \
                 row scan runs — for any NON-NEGATED pattern that accepts the declared \
                 default. A negated one does not stand aside: it is served as the universe \
                 minus the matches, which leaves such a row a candidate anyway"
            }
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

    /// No time measurement, and a ceiling on the WORK stated in its place.
    ///
    /// A number timed by hand is not available to every author: the run-to-run
    /// spread on the reference box swamps anything under a couple of seconds,
    /// and the A/B harness that can tell smaller differences apart does not run
    /// from every session that has to declare a tier. Hand-timing anyway would
    /// publish noise as precision. A row ceiling is a property of the code
    /// rather than of the box, so it can be stated exactly and re-derived by
    /// reading the tier — and it bounds work, not time. [`Measured::At`] is
    /// still owed; this states what IS known meanwhile, which
    /// [`Measured::Unmeasured`] does not.
    ///
    /// It covers the whole ASK, both terms, because that is what
    /// [`Serving::measured`] is: what the structure named in [`Serving::then`]
    /// costs per candidate as well as what the structure itself costs. Stating
    /// only the narrowing would leave the table reading cheaper than the query
    /// is, which is the same species of untruth as naming the wrong tier.
    RowCeiling {
        /// Passes over the workspace's rows this serving makes, once per
        /// PREDICATE that reaches it — not per query: the arm runs inside the
        /// loop over a clause's `WHERE` predicates, so two predicates on
        /// defaulted fields sweep twice. One pass reads every indexed row of
        /// every segment, and no `IN` or `EXCLUDE` narrows it.
        passes: u32,
        /// What one row costs on a pass, and where, so the number can be
        /// checked against the code rather than believed.
        per_row: &'static str,
        /// What the structure named in [`Serving::then`] costs on top, per
        /// candidate the passes above propose.
        ///
        /// Written out because it is usually the larger term, and because a
        /// reader who stopped at the narrowing would price the ask at the
        /// cheaper half of it.
        then_per_candidate: &'static str,
    },
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

/// Who owns the set of values a field can take.
///
/// This is what decides what a value OUTSIDE the set means. Where the CORPUS
/// owns the universe, a value it never stored is a legitimate question with an
/// empty answer — `guard_kind = 'ifdef'` on a corpus holding no `#ifdef`
/// answers nothing, and nothing is the truth about that corpus. Where the
/// ENGINE owns it, no corpus anywhere can hold a value outside the list, so
/// naming one is a fact about the query rather than about the code, and it is
/// refused at validation with the accepted values named
/// ([`crate::filter::reject_unknown_enum_values`]).
///
/// Corpus-owned is the default and stays the default: the distinction exists to
/// keep the refusal OFF every field whose empty answer is honest, not to widen
/// it.
#[derive(Debug, Clone, Copy)]
pub enum ValueUniverse {
    /// The corpus decides which values exist, so an unrecognised one answers
    /// empty and the empty answer is correct.
    Corpus,
    /// The engine decides, and these are the values it can produce.
    Engine(&'static [&'static str]),
}

/// What a field answers on a row its enricher LOOKED AT and did not write.
///
/// A stamp-only field is written only when it holds; nothing is written when it
/// does not. Read literally, that makes the other value unqueryable: nothing
/// stores it, so nothing selects it, and an empty page reads as a claim about
/// the corpus rather than about the index. But the index already implies the
/// answer — the rows the enricher examined, minus the rows it wrote — so the
/// value can be answered at query time without storing it.
///
/// Three things have to be declared for that arithmetic to be safe, and they
/// are declared HERE, once:
///
/// * `value` — what an examined-and-unwritten row answers.
/// * `applicable_kinds` — the kinds the default speaks for. This is NOT "every
///   row": a row of an unlisted kind answers neither value, because carrying
///   nothing is what a row the enricher never examined and a row it examined
///   and did not write BOTH look like, and only this list separates them.
/// * `applicable_languages` — the languages the default speaks for, on exactly
///   the same terms. An enricher gates on more than the node kind: on a
///   language CAPABILITY the config declares, and on the shape of the grammar
///   node it is handed. Python declares no address-of operator, so escape
///   analysis has never run on a Python function; cmake and make declare no
///   comment kind, so no marker scan has read theirs; and a cmake `function_def`
///   carries no `body` field, so the shadow walk never starts on one either —
///   a gate no config declares and the reason this list has four entries rather
///   than three. Without it those rows would answer "false" about an analysis
///   that did not happen: the whole failure this declaration exists to prevent,
///   one level further in than the kinds catch.
///
/// The kinds are a set of `fql_kind`s, and that is a deliberate narrowing
/// rather than a synonym for "examined". An enricher gates on the raw node
/// kinds its language declares as function kinds, and a language may declare a
/// kind a function kind while mapping it to some other `fql_kind` — cmake does
/// exactly that with `macro_def`, which is examined and lands under
/// `fql_kind = 'macro'`. Such a row is stamped when the field holds and answers
/// nothing when it does not, exactly as before this table gained defaults.
/// Saying so in the same breath as the claim is the contract; widening the list
/// to cover it would put language-specific kind names in core.
///
/// The languages ARE resolved from those gates — but at declaration time, into
/// plain name lists, so no reader has to reach a registry it does not have.
/// Three of the four are recomputed from the shipped CONFIGS by a test that
/// fails on drift; `has_shadow`'s is recomputed from the shipped GRAMMARS by
/// another, because no config declares the gate it depends on.
///
/// One thing this does NOT model, and must not be read as modelling: how well
/// an enricher that DID run reads the code. A walk that misses a position
/// reports too few `'true'` rows, and the complement then reports too many
/// `'false'` ones — the default inherits exactly the accuracy of the value it
/// complements, no more and no less. That is a defect in the enricher, fixed in
/// the enricher; it is not the never-examined case this declaration separates.
/// A Rust `/* TODO */` is the live instance: the marker scan runs on Rust and
/// reads only the one comment kind the config names, so `has_todo` reports too
/// few `'true'` rows and `'false'` inherits exactly that. Those rows stay
/// answered. The cmake rows do not, and the difference is whether anything ran.
///
/// Every reader derives all three from this one declaration, through
/// [`StampDefault::speaks_for`]. A predicate reader that narrowed to a set the
/// row-level evaluator then disagreed with would recreate the
/// two-readers-disagree class the counted-grouping work closed.
#[derive(Debug, Clone, Copy)]
pub struct StampDefault {
    /// The value an examined row that carries nothing answers.
    pub value: &'static str,
    /// The `fql_kind`s the field's enricher examines, and therefore the only
    /// rows the default speaks for.
    pub applicable_kinds: &'static [&'static str],
    /// The languages whose enricher actually runs on such a row.
    ///
    /// An enricher gates on more than the node kind. It gates on a language
    /// CAPABILITY the config declares — an address-of operator, a call
    /// expression, a comment kind — and on the shape of the grammar node
    /// itself: several read `child_by_field_name("body")` and return when
    /// there is none, which is how a cmake function, whose grammar carries no
    /// `body` field, gets past every capability check and is still never
    /// walked. Either way the row sits INSIDE the applicable kinds and was
    /// never examined, so it answers neither value — the same standing a row
    /// of an unlisted kind has, and for the same reason.
    ///
    /// There is no "every language" here on purpose. Every default names the
    /// languages it speaks for, so adding one is a decision somebody makes
    /// rather than a default nobody checked; the two stops this declaration
    /// cost were both an unexamined "and the rest".
    pub applicable_languages: &'static [&'static str],

    /// How `field = value` is served, for THIS value — see [`Serving`].
    ///
    /// It is not the field's [`FieldTier::serving`] entry for `=`, and the
    /// difference is the whole reason this slot exists. That entry describes
    /// reading one `field=value` key, and says its empty answer may conclude an
    /// absence once a named per-segment proof agrees. Neither half holds here:
    /// nothing stores this value, so there is no key to read, and the proof is
    /// declined for it by name.
    ///
    /// Serving is keyed by operator class ACROSS a field's `serving` entries
    /// and by value class HERE. One slot, one entry — a second serving for the
    /// same (`=`, default value) pair cannot be written, which is the same move
    /// as leaving no `every language` variant for the list above.
    pub eq: Serving,
}

impl StampDefault {
    /// Whether this default speaks for a row of `kind` written in `language`.
    ///
    /// Both dimensions in one place, so no reader can honour one and forget the
    /// other. A row it does not speak for answers neither value — not the
    /// default, and not its opposite.
    #[must_use]
    pub fn speaks_for(self, kind: &str, language: &str) -> bool {
        self.applicable_kinds.contains(&kind) && self.applicable_languages.contains(&language)
    }
}

/// The kinds every stamp-only boolean declared here speaks for. Their enrichers
/// gate on the raw kinds a language declares as its function kinds, and for
/// C, C++, Rust and Python that maps onto `fql_kind = 'function'` exactly — the
/// function rows and the rows carrying a function metric agree row for row on
/// every corpus the tests read. Where a language declares a function kind and
/// maps it elsewhere, the rows land outside this set and keep their old
/// behaviour; [`StampDefault`] says which and why.
const FUNCTION_ROWS: &[&str] = &["function"];

/// Languages whose config declares a comment kind (`syntax.comment`).
///
/// `TodoEnricher` returns before reading anything without one, so `has_todo`
/// would otherwise answer "no marker" for a body no scanner ever looked at.
/// cmake and make declare function kinds and no comment kind, which is why
/// this is a list rather than every language.
const COMMENT_LANGUAGES: &[&str] = &["c", "cpp", "python", "rust"];

/// Languages whose config declares a call expression (`expressions.call`).
///
/// `RecursionEnricher` finds a self-call by matching call expressions; without
/// the kind it returns, and every function would answer "not recursive".
const CALL_LANGUAGES: &[&str] = &["c", "cpp", "python", "rust"];

/// Languages whose config declares an address-of expression
/// (`expressions.address_of`).
///
/// Python declares it as the empty string, so `EscapeEnricher` has never run on
/// a Python function. Answering `has_escape = 'false'` over every `function`
/// row regardless of language would publish "no local escapes" for every Python
/// function in the workspace — a claim about an analysis that did not happen.
const ADDRESS_OF_LANGUAGES: &[&str] = &["c", "cpp", "rust"];

/// Languages whose function kinds carry the `body` field the shadow walk needs.
///
/// `ShadowEnricher` reads no language capability — its only gates are the node
/// kind and `child_by_field_name("body")`, which it returns on when there is
/// none. That second gate is a property of the GRAMMAR, not of the config, and
/// it is what cmake and make fail: both declare function kinds that map to
/// `fql_kind = 'function'`, both pass every capability check there is, and
/// neither node carries a `body` field, so the walk never starts and the row
/// was never examined.
///
/// This one is therefore tied back not to the shipped configs but to the
/// shipped GRAMMARS, by a test that parses a function in each language that
/// produces `function` rows and asks the node.
const SHADOW_LANGUAGES: &[&str] = &["c", "cpp", "python", "rust"];

/// The `fql_kind` strings the indexer writes as literals rather than reading
/// out of a language config's `kind_map`.
///
/// Each is spelled once HERE and referenced at the site that writes it, so the
/// tie between a writing site and [`FQL_KIND_VALUES`] is a compile-time one.
/// That matters because the config sweep in
/// `tests/engine_owned_value_universes.rs` cannot see this route, and a kind
/// the engine mints but the universe does not carry refuses a legitimate query.
/// The compile-time reference IS the guard — a test asserting
/// `FQL_KIND_VALUES.contains(&ERROR_KIND)` would be asserting that a list
/// contains an element it is built from, and could not fail.
///
/// Two of them already had a name in `ast::lang`'s older `FQL_*` family and are
/// aliased to it rather than respelled. Note that family is not a safe blanket
/// source: `FQL_COMPOUND_ASSIGN` and `FQL_SHIFT` there read `compound_assign`
/// and `shift`, while the kinds rows actually carry are `compound_assignment`
/// and `shift_expression` — so only the two verified below are reused.
///
/// `guard` is deliberately NOT here. It reads like a minted kind and is not
/// one: the C and C++ configs map `preproc_ifdef`/`preproc_if`/`preproc_elif`
/// onto it, so the config sweep covers it and a constant would have claimed a
/// tie that does not exist.
pub const ERROR_KIND: &str = crate::ast::lang::FQL_ERROR;
/// See [`ERROR_KIND`].
pub const CAST_KIND: &str = crate::ast::lang::FQL_CAST;
/// See [`ERROR_KIND`].
pub const MACRO_CALL_KIND: &str = "macro_call";
/// How `SHOW outline` renders a row that carries no kind at all.
///
/// The stored value is the empty string; this is the spelling an agent sees and
/// therefore the spelling they filter on, so both are accepted values. See
/// [`ERROR_KIND`] for why it is spelled once.
pub const UNKNOWN_KIND: &str = "unknown";

/// Spell a value written on `fql_kind` the way the index stores it.
///
/// A row nothing maps carries the EMPTY kind, and that is the only spelling
/// stored, grouped or posted. [`UNKNOWN_KIND`] is a rendering: `SHOW outline`
/// and `SHOW members` print it in place of the empty string, so an agent
/// filtering on what the engine just printed writes `unknown`. Both spellings
/// name the same rows, so both sides of a comparison are spelled to the stored
/// one before they meet — the predicate value at the parser boundary
/// (`parser::clauses::parse_predicate`, the single place an agent's text
/// becomes an IR value) and the row's own value in the comparison funnel
/// (`filter::comparable_field_str`), which is what lets an outline row that
/// renders `unknown` answer the same equality as a `FIND symbols` row that
/// renders the empty string.
///
/// Every other value is returned unchanged, so this is a no-op on the 40-odd
/// real kinds and on every other field.
#[must_use]
pub fn stored_kind_value(value: &str) -> &str {
    if value == UNKNOWN_KIND { "" } else { value }
}
/// Every `fql_kind` the engine can put on a row.
///
/// The engine owns this universe because a language plugin does not invent
/// kinds — it MAPS its grammar's node kinds onto the names here (`kind_map`,
/// `block_groups`) — and the indexer mints a handful itself as literals:
/// `guard`, `error`, `cast`, `macro_call`, the empty kind a row carries when
/// nothing maps its grammar node, and `unknown`, which is how `SHOW outline`
/// renders that same row.
///
/// **The ownership is a convention this list does not enforce.**
/// `LanguageConfig::kind_map_lookup` returns whatever the config JSON says,
/// with no validation, so a config-only edit CAN put an unlisted kind on rows —
/// and it would then be refused on `WHERE` although its rows exist. What stops
/// that is a test, not the mechanism: `tests/engine_owned_value_universes.rs`
/// reads every `crates/*/config/*.json` (by glob, so a new plugin crate is
/// covered without being named) and fails if one maps to a kind this list does
/// not carry. A config outside that path, or a kind a plugin computes rather
/// than declares, is outside what the test can see. Deriving this list from the
/// `LanguageRegistry` at query time would make the claim mechanical; it does
/// not today.
///
/// This list being a SUPERSET is safe; being short is not — a kind missing here
/// refuses a legitimate query, which is worse than the silence the refusal
/// replaces. The core-minted kinds are named separately in that test, since no
/// config declares them and the config sweep cannot see that route. A third
/// list, `file_indexer::ADDRESSABLE_FQL_KINDS`, is asserted to be a subset of
/// this one in its own module, for the same reason.
pub const FQL_KIND_VALUES: &[&str] = &[
    // The kindless row, under BOTH the spellings the engine publishes for it.
    //
    // Stored it is the empty string, and `GROUP BY fql_kind` publishes those
    // rows under the empty name — so `= ''` is a question the engine puts into
    // an agent's hands and must not refuse. `SHOW outline` renders the same row
    // as `unknown` (`query/outline.rs` twice, `ast/show/members.rs` once), and
    // `SHOW outline … WHERE fql_kind = 'unknown'` matches what it rendered and
    // answers rows. A refusal that knew only one spelling would contradict a
    // value the engine had just printed, so both are accepted here; only the
    // rendering decides which one an agent sees.
    //
    // Both are SERVED, and they answer the same rows and the same total on
    // every verb whose rows carry a kind: `parse_predicate` spells either one to
    // the stored value before it reaches a reader, `step5_build_kind_postings`
    // posts the empty kind like any other so the equality is one bitmap read,
    // and on a scan a row whose kind is empty reports it as the value it is
    // rather than as a missing field. A row shape holding no kind COLUMN is the
    // stated exclusion: a `FIND usages` site is one line of a file, and this
    // field is accepted there and answers none of them, on either spelling.
    // `kindless_kind_equality.json` pins the two spellings against the counted
    // grouping on three corpora, and that exclusion beside them, and an empty
    // answer on this field is a fact about the corpus for these two values as it
    // is for every other.
    "",
    UNKNOWN_KIND,
    // Definitions and declarations.
    "function",
    "class",
    "struct",
    "union",
    "interface",
    "enum",
    "enumerator",
    "field",
    "method",
    "variable",
    "global_variable",
    "local_declaration",
    "namespace",
    "type_alias",
    "macro",
    MACRO_CALL_KIND,
    "import",
    // Statements and expressions.
    "call_statement",
    "return_expression",
    "if",
    "for",
    "while",
    "switch",
    "do",
    "do_while",
    "number",
    CAST_KIND,
    "increment",
    "compound_assignment",
    "shift_expression",
    // Structured text: JSON, YAML, TOML, INI, XML, Markdown, reStructuredText,
    // CMake, Make, just, Kconfig, DBC.
    "object",
    "array",
    "pair",
    "section",
    "heading",
    "paragraph",
    "list_item",
    "code_block",
    "table",
    "block_quote",
    // Comments, and the runs of adjacent siblings a language groups into one
    // addressable node.
    "comment",
    "comment_block",
    "include_block",
    "include_group",
    "macro_block",
    "import_block",
    "type_alias_block",
    "array_block",
    // A conditional directive with its guarded region, and a span the parser
    // could not parse. `guard` is config-derived (the C and C++ `kind_map`s
    // name it) and `error` is minted; `cast` and `macro_call` above are minted
    // too — they sit up there because this list groups by what a kind MEANS,
    // not by where its name comes from.
    "guard",
    ERROR_KIND,
];

/// The two occurrence roles the read pass writes itself, rather than reading
/// them out of a language config's `mention_text_kinds`.
///
/// Spelled here and referenced at the writing site (`query/find.rs`), for the
/// reason [`ERROR_KIND`] gives: the config sweep cannot see this route, and the
/// compile-time reference is what keeps a rename from leaving
/// [`USAGE_ROLE_VALUES`] behind.
pub const ROLE_CODE: &str = "code";
/// See [`ROLE_CODE`].
pub const ROLE_TEXT: &str = "text";

/// Every `role` an occurrence row can carry on `FIND usages`.
///
/// Engine-owned on the same terms as [`FQL_KIND_VALUES`]: [`ROLE_CODE`] is a
/// resolved identifier, [`ROLE_TEXT`] a site found by reading the file rather
/// than by a posting, and the rest are the mention roles a language config may
/// declare (`mention_text_kinds`). The glob test checks the configs against
/// this list; the two minted roles are tied to it by the constants above.
pub const USAGE_ROLE_VALUES: &[&str] = &[
    ROLE_CODE, "comment", "string", "config", "doc", ROLE_TEXT,
    // The spelling the renderer emits where a site carries no role at all: the
    // in-memory backend tags none of them (`SiteView.role` is `None` there) and
    // the CSV renderer prints that as the empty string. Accepted for the same
    // reason `unknown` is on the kinds — a universe that refuses what the
    // engine just printed contradicts itself.
    //
    // Accepted is all it is. `role = ''` MATCHES nothing on either backend, and
    // not because no site qualifies: the `Eq` arm of `eval_predicate_on` is
    // `is_some_and`, so a field the row does not carry fails every equality,
    // empty string included. On the indexed backend that answer is also the
    // right one — every site there is tagged. On the in-memory backend, which
    // tags none of them, it is a false zero: a value the renderer prints and the
    // predicate cannot reach. That miss is now this field's alone. The kindless
    // KINDS had the same shape and no longer do — they are served on both
    // backends, and `kindless_kind_equality.json` pins them where an error is a
    // failure — so this comment can no longer lean on their pin. Recorded as
    // OPEN in `HINTS.md`; the fix is the same shape as theirs, which is to give
    // the empty role a value the reader can find rather than a hole.
    "",
];
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
    /// Who owns the set of values this field takes, and therefore what a value
    /// outside that set means — an empty answer about the corpus, or a refusal
    /// naming the accepted values. See [`ValueUniverse`].
    pub values: ValueUniverse,
    /// For a field written only when it holds, what a row the enricher examined
    /// and did not write answers, and which rows it examined. `None` for every
    /// field whose values are all stored. See [`StampDefault`].
    pub default: Option<StampDefault>,
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

/// `=` on a value some row STORES reads the one `field=value` key. An empty
/// bitmap is candidates-none, not answer-none, until the per-segment proof
/// says the value is carried by no segment at all.
///
/// The value class matters here, and this entry does not speak for all of it:
/// a stamp-only field's declared default is stored by no row, so neither the
/// key read nor the proof describes how that value is answered.
/// [`STAMP_DEFAULT_EQ`] does, and [`StampDefault::eq`] is the one slot a
/// field's declaration carries it in.
const ENRICH_EQ: Serving = Serving {
    ops: &[OpClass::Eq],
    tier: Tier::KeyBitmap,
    then: Then::Filters(Tier::Scan),
    exactness: Exactness::SupersetProvenAbsent("fast_paths::no_segment_carries_enrichment_value"),
    measured: Measured::Unmeasured,
};

/// `=` on a stamp-only field's DECLARED DEFAULT is a different question from
/// `=` on a value some row stores, and a different structure answers it.
///
/// [`ENRICH_EQ`] reads one `field=value` key and, finding none, may conclude
/// through its named per-segment proof that the value is carried nowhere. That
/// conclusion is exactly wrong for this one value: nothing stores it, so the
/// postings are silent about it in both directions, and the proof is declined
/// for it by name (`SegmentReader::proves_enrichment_value_absent` returns
/// `false` when the value is a declared default, and the `=` arm returns before
/// reaching it at all). Declaring [`Exactness::SupersetProvenAbsent`] here
/// would name a prover no query runs.
///
/// What runs is `fast_paths::stamp_default_candidates`, and the exactness is
/// the plain [`Exactness::Superset`] that tier earns: the kind bitmaps it
/// unions include the rows that DO carry the field, so the row-level evaluator
/// is what removes them. The cost is stated as a ceiling in rows rather than a
/// time, and in both its terms: the narrowing sweeps every segment's language
/// column, once per predicate and narrowed by no `IN`, and the row view then
/// reads every candidate — which is the larger term, and wider than the
/// applicable kinds, because a segment that posted no keys for the field
/// contributes all of its rows. See [`Measured::RowCeiling`].
const STAMP_DEFAULT_EQ: Serving = Serving {
    ops: &[OpClass::Eq],
    tier: Tier::StampDefault,
    then: Then::Filters(Tier::Scan),
    exactness: Exactness::Superset,
    measured: Measured::RowCeiling {
        passes: 1,
        per_row: "one u32 language-column read, in segment_reader::segment_written_in, \
                  over the segments fast_paths::rows_written_in walks",
        then_per_candidate: "one row-view read per candidate proposed, and the candidates \
                             are the applicable kinds' rows PLUS every row of a segment \
                             that holds the column and posted no keys for it, intersected \
                             with the segments written in an applicable language — or, \
                             where the narrowing declines, every row in the workspace",
    },
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

/// [`POSTED_GAPS`] plus the one a stamp-only field adds: the value its
/// declaration answers is stored by no row, so no key names the rows it speaks
/// for.
///
/// Written out rather than derived, because a const slice cannot be extended in
/// a const context. The table test asserts the two lists differ by exactly this
/// one entry, so a gap added to [`POSTED_GAPS`] and forgotten here fails rather
/// than going quiet.
const STAMP_ONLY_GAPS: &[Gap] = &[
    Gap::OverBudgetFile,
    Gap::OverBudgetWorkspace,
    Gap::EmptyValue,
    Gap::DirtySession,
    Gap::StampDefaultValue,
];

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
        values: ValueUniverse::Corpus,
        default: None,
    }
}

/// A [`posted`] field written ONLY when it holds, and therefore one whose other
/// value has to be answered rather than looked up.
///
/// `default_value` is what a row the enricher examined and did not write
/// answers; `applicable_kinds` is which rows it examines. Both travel together
/// because neither is safe alone: the value without the kinds would speak for
/// rows nothing looked at, and the kinds without the value would say which rows
/// are in scope without saying what they answer.
///
/// The serving entry and the extra gap travel with them, from
/// [`STAMP_DEFAULT_EQ`] and [`STAMP_ONLY_GAPS`]. A field cannot declare a
/// default and leave the table describing that value the way it describes the
/// stored ones — which it would do by inheriting [`ENRICH_EQ`] through
/// [`posted`], and which is what the four fields shipped doing.
const fn stamp_only(
    field: &'static str,
    per_file: usize,
    per_workspace: usize,
    default_value: &'static str,
    applicable_kinds: &'static [&'static str],
    applicable_languages: &'static [&'static str],
) -> FieldTier {
    FieldTier {
        default: Some(StampDefault {
            value: default_value,
            applicable_kinds,
            applicable_languages,
            eq: STAMP_DEFAULT_EQ,
        }),
        gaps: STAMP_ONLY_GAPS,
        ..posted(field, per_file, per_workspace)
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
///
/// The bitmap is exact and covers EVERY value the engine can put on a row, the
/// empty kind included: `step5_build_kind_postings` posts it like any other,
/// intersected with each segment's canonical row set, so `WHERE fql_kind = ''`
/// is one binary search and one bitmap decode — the same `Tier::KindBitmap`
/// cost as `= 'function'`, not a scan — and it selects exactly the rows
/// `GROUP BY fql_kind` counts under the empty name. `= 'unknown'`, the spelling
/// `SHOW outline` renders for that same row, is spelled to the stored value at
/// the parser boundary and therefore reads the same bitmap and returns the same
/// total. Every other operator on this field scans, and on a scan a row whose
/// kind is empty reports it as the value it is rather than as a missing field —
/// the readers holding a kind column say so (`segment_reader::materialize_rows`
/// AND `materialize_one_row`, the separate builder behind the row-view page,
/// plus both row views, the legacy row and its prefilter) — so `NOT MATCHES`
/// keeps that row instead of silently shedding it. A row shape with no kind
/// COLUMN still resolves to nothing and still matches nothing: a `FIND usages`
/// site is one line of a file, and this field is accepted there and answers
/// none of them, on either spelling.
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Engine(FQL_KIND_VALUES),
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
    },
    FieldTier {
        field: "usages",
        aliases: &[],
        // The column exists so this table and `ZONEMAP_NUMERIC_FIELDS` agree as
        // sets — `every_zone_mapped_column_is_declared` checks exactly that —
        // and for no other reason: `usages_count` on a segment is a stale
        // all-zeros legacy field, and its zone map must never prune. Both
        // pruners now skip the name outright (`query/find.rs`,
        // `query/resolve.rs`), so the serving below is the truth: a complete
        // row scan, with the workspace total attached at materialisation.
        // Claiming `ZoneMap` here described a prune that provably cannot run.
        column: Some("usages_count"),
        source: Source::Aggregate,
        serving: SCAN_ALL,
        // No gap, like every other `SCAN_ALL` row: the scan reads every row,
        // and the value's one dirty-session caveat is not a gap in the answer
        // either. On a session with uncommitted edits the aggregate the value
        // comes from is the master's, and `stamp_usage_counts_with` corrects
        // each row through `UsageAdjust::corrected` before any predicate sees
        // it — so the number served is exact. `Gap::DirtySession` would have
        // been the wrong declaration twice over: it describes what a serving
        // structure cannot see, and its fallback names the row union, which
        // adds rows rather than correcting counts.
        gaps: &[],
        budget: None,
        elsewhere: None,
        post_group: false,
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Engine(USAGE_ROLE_VALUES),
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
        values: ValueUniverse::Corpus,
        default: None,
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
    stamp_only(
        "is_recursive",
        8,
        64,
        "false",
        FUNCTION_ROWS,
        CALL_LANGUAGES,
    ),
    posted("has_fallthrough", 8, 64),
    posted("is_const", 8, 64),
    posted("is_mutable", 8, 64),
    posted("is_unsafe", 8, 64),
    posted("is_async", 8, 64),
    posted("is_generic", 8, 64),
    stamp_only("has_todo", 8, 64, "false", FUNCTION_ROWS, COMMENT_LANGUAGES),
    posted("is_exported", 8, 64),
    posted("has_catch_all", 8, 64),
    stamp_only(
        "has_escape",
        8,
        64,
        "false",
        FUNCTION_ROWS,
        ADDRESS_OF_LANGUAGES,
    ),
    stamp_only(
        "has_shadow",
        8,
        64,
        "false",
        FUNCTION_ROWS,
        SHADOW_LANGUAGES,
    ),
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
    values: ValueUniverse::Corpus,
    default: None,
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

/// The accepted values for a field whose value universe the ENGINE owns.
///
/// `None` for every field whose values the CORPUS decides, and that is most of
/// them: those keep the absent-value fast path, where a value the corpus never
/// stored answers empty and the empty answer is a fact about the code. A
/// `Some` here is the narrower claim that no corpus can ever hold a value
/// outside the list, which is what makes refusing one a fact about the query.
///
/// The field is taken AS WRITTEN — the caller does not canonicalise, and does
/// not need to: `lookup` matches a row's aliases as well as its canonical name,
/// which is how `kind` reaches `fql_kind`'s universe. Tightening `lookup` to an
/// exact match would silently drop every alias from the check.
#[must_use]
pub fn engine_owned_values(field: &str) -> Option<&'static [&'static str]> {
    match lookup(field)?.values {
        ValueUniverse::Corpus => None,
        ValueUniverse::Engine(values) => Some(values),
    }
}

/// The fields that declare a stamp-only default.
///
/// A fast door in front of [`stamp_default`], which is consulted once per ROW
/// by the predicate, ordering and grouping paths. Without it every one of those
/// rows walked the whole of `FIELD_TIERS` to be told that its field declares no
/// default — a cost paid by `ORDER BY` and `GROUP BY` on ANY enrichment field,
/// including the ten stamp-only booleans this mechanism deliberately does not
/// speak for.
///
/// It is a hand-written copy of a fact the table already holds, so
/// `field_tier_table::only_the_four_ruled_fields_declare_a_stamp_default`
/// derives the same set from `FIELD_TIERS` and fails if the two disagree: a
/// fifth declaration that missed this list would lose its default in silence.
/// The same test asserts the four declare no ALIASES, because an alias reaches
/// `lookup` and would never reach this list.
pub const STAMP_DEFAULT_FIELDS: [&str; 4] =
    ["is_recursive", "has_todo", "has_escape", "has_shadow"];

/// What `field` answers on a row nothing wrote it to.
///
/// `None` when every value the field takes is stored, which is the case for all
/// but a handful of fields. When it is `Some`, the declaration carries the
/// value, the kinds it speaks for and the languages it speaks for.
///
/// This is the ONE place the default is read. Every reader that has to agree
/// about it — the workspace bitmap prefilter, the per-row predicate evaluator,
/// the counted grouping — calls this, so none of them can drift from another.
/// `field` is looked up as written, so an alias would resolve the same
/// declaration; none of the four declares one today, and the table test says so.
#[must_use]
pub fn stamp_default(field: &str) -> Option<StampDefault> {
    if !STAMP_DEFAULT_FIELDS.contains(&field) {
        return None;
    }
    lookup(field)?.default
}

/// Whether `field` answers `value` for a row the enricher examined and did not
/// write. False for every field with no declared default, and for any other
/// value of a field that has one.
///
/// The distinction matters at exactly one place per reader: a value nothing
/// stores usually means "no row can carry this", and for this one value it
/// means the opposite.
#[must_use]
pub fn is_stamp_default_value(field: &str, value: &str) -> bool {
    stamp_default(field).is_some_and(|d| d.value == value)
}

/// Every field the symbol query refuses outright, in table order.
pub fn refused_fields() -> impl Iterator<Item = &'static FieldTier> {
    FIELD_TIERS.iter().filter(|tier| tier.is_refused())
}

/// Whether a result row is given this field **after** it is built, from
/// something no segment column holds.
///
/// [`Source::MaterialisedText`] is the declaration that a field arrives that
/// way: `body` is read out of the file as the row is materialised, and `role`
/// is written onto an occurrence row by the read pass that finds the site. A
/// reader looking only at the columns sees nothing for either and would be
/// wrong to conclude the row carries nothing — so a predicate on one of them
/// has to wait for the row, exactly as `usages` and `node_id` do.
///
/// This is the guard on treating "no column holds it" as "nothing holds it".
/// The two coincide today on the on-disk `FIND symbols` path, because
/// `SegmentReader::materialize_rows` fills a row's map from enrichment columns
/// and from nothing else — but by accident of that one function rather than by
/// anything that would stop the next writer, which is why the rule asks here
/// instead of assuming.
#[must_use]
pub fn written_after_materialisation(field: &str) -> bool {
    lookup(field).is_some_and(|tier| tier.source == Source::MaterialisedText)
}
