//! Core's copies of what the language plugins declare must not fall behind
//! them.
//!
//! `field_tiers::FQL_KIND_VALUES` and `field_tiers::USAGE_ROLE_VALUES` are what
//! `filter::reject_unknown_enum_values` refuses against. A value a language
//! config can actually produce and the list does not carry is therefore not a
//! missing feature — it is a legitimate query refused, which is worse than the
//! silent zero the refusal exists to replace.
//!
//! So the check runs the other way round from the refusal: it reads every
//! language config JSON in the workspace and asserts the lists COVER them. It
//! finds those files by walking `crates/*/config/`, not by naming the plugin
//! crates, so a new language crate is covered the day its config lands rather
//! than the day someone remembers this file.
//!
//! The third copy checked here is the set of languages each stamp-only boolean
//! default speaks for. It is not a refusal list but an answer list, and it
//! fails the other way round: a language in core's copy whose config declares
//! none of the syntax the enricher needs gets a default published about an
//! analysis that never ran on it. Same sweep, same reason — a copy of a config
//! drifts unless something reads both.
//!
//! What the configs cannot show — the kinds and roles the engine mints itself —
//! is deliberately NOT asserted here. Those are tied to the two lists by a
//! compile-time reference at the site that writes them, so any assertion this
//! file could make about them would be a tautology over the list it is
//! checking. The comment further down says it at length, because the tautology
//! is the tempting thing to write.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forgeql_core::ast::lang_json::LanguageConfigJson;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use forgeql_core::field_tiers::{FQL_KIND_VALUES, USAGE_ROLE_VALUES};
use serde_json::Value;

/// The configs shipped when this was written.
///
/// A glob that matched nothing would pass every membership assertion below
/// without reading a byte, so the sweep is pinned to a floor rather than
/// trusted to have found something. It is a floor, not an equality: adding a
/// language must not fail this file.
const CONFIGS_AT_LEAST: usize = 16;

/// How many of those configs declare occurrence roles.
///
/// Ten of the sixteen do; the other six index no `mention_text_kinds` at all,
/// so the role sweep gets a floor rather than a per-file assertion. Without it
/// a schema move would leave one surviving config keeping the global counter
/// non-zero while the rest were skipped in silence.
const ROLE_DECLARING_CONFIGS_AT_LEAST: usize = 10;

/// Every `crates/*/config/*.json` in the workspace.
fn language_config_files() -> Vec<PathBuf> {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("forgeql-core sits under crates/");
    let mut out = Vec::new();
    for crate_entry in std::fs::read_dir(crates_dir).expect("read crates/") {
        let config_dir = crate_entry.expect("crate dir entry").path().join("config");
        if !config_dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&config_dir).expect("read config/") {
            let path = file.expect("config dir entry").path();
            if path.extension().is_some_and(|ext| ext == "json") {
                out.push(path);
            }
        }
    }
    out.sort();
    assert!(
        out.len() >= CONFIGS_AT_LEAST,
        "found {} language configs under crates/*/config/, expected at least {CONFIGS_AT_LEAST} \
         — an empty or short sweep passes every assertion in this file without checking anything",
        out.len(),
    );
    out
}

/// The parsed JSON of every language config, paired with its path for messages.
/// Whether a config declares a non-empty value at `key`.
///
/// Absent and empty mean the same thing to the config loader: the raw kind is
/// the empty string and the capability check is false. Python declares
/// `expressions.address_of` as `""` and is the live instance.
fn key_path_is_declared(json: &Value, key: &[&str]) -> bool {
    let mut node = json;
    for part in key {
        match node.get(*part) {
            Some(next) => node = next,
            None => return false,
        }
    }
    node.as_str().is_some_and(|raw| !raw.is_empty())
}

/// The predicate the field's enricher actually gates on.
///
/// One arm per capability-gated default, naming the same method the enricher
/// calls, so this test and the enricher cannot drift apart without one of them
/// failing to compile.
fn enricher_gate(field: &str, config: &forgeql_core::ast::lang::LanguageConfig) -> bool {
    match field {
        // `todo.rs` gates on this before reading anything.
        "has_todo" => config.has_comment(),
        // `recursion.rs` gates on this before looking for a self-call.
        "is_recursive" => config.has_call_expression(),
        // `escape.rs` gates on this before scanning declarations.
        "has_escape" => config.has_address_of(),
        other => panic!("no enricher gate recorded for {other}"),
    }
}

fn parsed_configs() -> Vec<(PathBuf, Value)> {
    language_config_files()
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).expect("read language config");
            let json = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
            (path, json)
        })
        .collect()
}

#[test]
fn every_kind_a_language_config_maps_to_is_in_the_engines_kind_universe() {
    let mut checked = 0usize;
    for (path, json) in parsed_configs() {
        // Per FILE, not just globally. A single global `checked > 0` would let
        // a config whose key was renamed or re-nested be skipped in silence
        // while one surviving file kept the counter non-zero — the sweep
        // reporting success over a plugin it never read. All sixteen configs
        // carry a `kind_map` today.
        let map = json
            .get("kind_map")
            .and_then(Value::as_object)
            .unwrap_or_else(|| {
                panic!(
                    "{}: no kind_map object — either the schema moved and this sweep now \
                     reads nothing, or a language config genuinely maps no kinds; either \
                     way the sweep must not pass in silence",
                    path.display()
                )
            });
        {
            for (raw_kind, mapped) in map {
                let kind = mapped.as_str().unwrap_or_else(|| {
                    panic!("{}: kind_map {raw_kind} is not a string", path.display())
                });
                assert!(
                    FQL_KIND_VALUES.contains(&kind),
                    "{}: kind_map maps {raw_kind} to fql_kind '{kind}', which FQL_KIND_VALUES \
                     does not carry — WHERE fql_kind = '{kind}' would be refused although rows \
                     of that kind exist",
                    path.display(),
                );
                checked += 1;
            }
        }
        for group in json
            .get("block_groups")
            .and_then(Value::as_array)
            .map_or(&[][..], |a| a.as_slice())
        {
            for key in ["member_fql_kind", "block_fql_kind"] {
                let kind = group.get(key).and_then(Value::as_str).unwrap_or_else(|| {
                    panic!("{}: block_groups entry has no {key}", path.display())
                });
                assert!(
                    FQL_KIND_VALUES.contains(&kind),
                    "{}: block_groups {key} is '{kind}', which FQL_KIND_VALUES does not carry",
                    path.display(),
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "no kind was checked — the configs parsed but declared no kind_map or block_groups"
    );
}

#[test]
fn every_role_a_language_config_declares_is_in_the_engines_role_universe() {
    let mut checked = 0usize;
    let mut configs_with_roles = 0usize;
    for (path, json) in parsed_configs() {
        let Some(kinds) = json
            .get("syntax")
            .and_then(|s| s.get("mention_text_kinds"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        configs_with_roles += 1;
        for (raw_kind, spec) in kinds {
            // Two spellings: the bare role, and the positioned form that names
            // the grammar field the role applies under.
            let role = match spec {
                Value::String(role) => role.as_str(),
                Value::Object(obj) => {
                    obj.get("role").and_then(Value::as_str).unwrap_or_else(|| {
                        panic!(
                            "{}: mention_text_kinds {raw_kind} has no role",
                            path.display()
                        )
                    })
                }
                other => panic!(
                    "{}: mention_text_kinds {raw_kind} is neither a role nor a positioned \
                     mention: {other}",
                    path.display()
                ),
            };
            assert!(
                USAGE_ROLE_VALUES.contains(&role),
                "{}: mention_text_kinds {raw_kind} declares role '{role}', which \
                 USAGE_ROLE_VALUES does not carry — FIND usages WHERE role = '{role}' would be \
                 refused although sites of that role exist",
                path.display(),
            );
            checked += 1;
        }
    }
    // Per-file coverage, not just a global count: one surviving config would
    // otherwise keep `checked` non-zero while a schema move silenced the rest.
    // Ten of the sixteen configs declare mention kinds; the other six index no
    // occurrence roles at all, so this is a floor rather than a per-file
    // assertion.
    assert!(
        configs_with_roles >= ROLE_DECLARING_CONFIGS_AT_LEAST,
        "only {configs_with_roles} config(s) declared syntax.mention_text_kinds, expected at \
         least {ROLE_DECLARING_CONFIGS_AT_LEAST} — a schema move would silence this sweep \
         while one surviving file kept it green"
    );
    assert!(
        checked > 0,
        "no role was checked — no config declared a mention kind"
    );
}

// The kinds and roles the engine mints itself are NOT asserted here, and the
// reason is worth writing down because the obvious test is a trap.
//
// `assert!(FQL_KIND_VALUES.contains(&ERROR_KIND))` cannot fail: the list is
// built from that very constant. It reads like a cross-check and is a
// tautology, and a green tautology beside a real sweep is worse than no test —
// it reports coverage of a route nothing checked.
//
// What actually ties a minted value to its universe is a compile-time
// reference. `casts.rs` writes `field_tiers::CAST_KIND`, `rows.rs` writes
// `MACRO_CALL_KIND`, `outline.rs` and `members.rs` write `UNKNOWN_KIND`, and
// `find.rs` tags sites with its own `ROLE_CODE` / `ROLE_TEXT` — every one of
// them a constant the two lists are built from. Rename one and both ends move
// together; there is no state in which they can disagree, so there is nothing
// for a test to catch.
//
// `guard` is not minted at all: the C and C++ `kind_map`s name it, so the
// config sweep above is what covers it.
//
// The route that WOULD want a behavioural test is the one no reference can
// tie: a kind or role an enricher composes at run time rather than naming.
// None exists today — every `ExtraRow` either goes through `map_kind` or names
// one of those constants — which is why this file sweeps configs and stops.
#[test]
fn neither_universe_repeats_a_value() {
    // A repeat would not change what is accepted, but it would make the refusal
    // message list the value twice, and it is the symptom of a merge that added
    // a name already there.
    for (name, values) in [
        ("FQL_KIND_VALUES", FQL_KIND_VALUES),
        ("USAGE_ROLE_VALUES", USAGE_ROLE_VALUES),
    ] {
        let distinct: BTreeSet<&str> = values.iter().copied().collect();
        assert_eq!(distinct.len(), values.len(), "{name} repeats a value");
    }
}

/// The languages each stamp-only default speaks for, recomputed from the
/// configs it claims to describe.
///
/// The field-tier table resolves an enricher's language CAPABILITY into a plain
/// list of language names at declaration time, so no reader has to reach the
/// registry while a query runs. That list is a COPY of what the configs say,
/// and a copy drifts: a language that gains an address-of operator, or a new
/// plugin that arrives carrying comments, changes what the enricher runs on and
/// changes nothing about the constant. Drift one way merely withholds an
/// answer. Drift the other way publishes a default for rows nothing ever
/// examined, which is the single thing the declaration exists to prevent.
///
/// The capability keys are spelled out here rather than read back from
/// `LanguageConfig`, for the same reason the posting budgets are repeated in
/// the field-tier table: two independent spellings disagree loudly, and one
/// shared spelling cannot disagree at all.
#[test]
fn every_stamp_only_default_covers_exactly_the_languages_whose_enricher_runs() {
    use forgeql_core::field_tiers;

    // (field, the config path its enricher's capability gate reads)
    const GATED: [(&str, &[&str]); 3] = [
        ("has_todo", &["syntax", "comment"]),
        ("is_recursive", &["expressions", "call"]),
        ("has_escape", &["expressions", "address_of"]),
    ];

    let configs = parsed_configs();
    let mut computed: Vec<BTreeSet<String>> = Vec::new();

    for (field, key) in GATED {
        let mut languages = BTreeSet::new();
        let mut read = 0usize;

        for (path, json) in &configs {
            // Per FILE, like the sweeps above: a config whose `language.name`
            // moved would otherwise drop out of every set in silence, and a
            // language missing from a list is exactly the drift being looked
            // for.
            let name = json
                .get("language")
                .and_then(|language| language.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    panic!(
                        "{} declares no language.name — the language lists in \
                         field_tiers are keyed by it and cannot be checked without it",
                        path.display()
                    )
                });
            read += 1;

            let mut node = json;
            let mut reached = true;
            for part in key {
                if let Some(next) = node.get(*part) {
                    node = next;
                } else {
                    reached = false;
                    break;
                }
            }
            // Absent and empty mean the same thing to the config loader: the
            // raw kind is the empty string and the capability check is false.
            if reached && node.as_str().is_some_and(|raw| !raw.is_empty()) {
                let _ = languages.insert(name.to_owned());
            }
        }

        assert_eq!(
            read,
            configs.len(),
            "{field}: read {read} of {} configs",
            configs.len()
        );

        // The sweep above reads the config FILE; the enricher reads a
        // `LanguageConfig` METHOD. Nothing else makes those two agree, so
        // rewriting `has_comment()` to consult a different key would leave this
        // test green while the list it computes went wrong in the dangerous
        // direction — a default published for rows the enricher no longer
        // examines. Building the real config from the same bytes and comparing
        // is what ties the key path to the gate.
        for (path, json) in &configs {
            let bytes = std::fs::read(path).expect("read language config");
            let config = LanguageConfigJson::from_json_bytes(&bytes)
                .unwrap_or_else(|e| panic!("{} must deserialize: {e:?}", path.display()))
                .into_language_config();
            let via_key = key_path_is_declared(json, key);
            let via_gate = enricher_gate(field, &config);
            assert_eq!(
                via_gate,
                via_key,
                "{field}: {} declares {key:?} = {via_key} but the predicate the \
                 enricher gates on answers {via_gate}. The language list is computed \
                 from the key path and consumed by the enricher through the \
                 predicate; when they disagree the list describes a gate that is \
                 not the one being applied",
                path.display(),
            );
        }
        assert!(
            !languages.is_empty(),
            "{field}: no config declares {key:?}, so the sweep found nothing and any \
             comparison below would pass against an empty set"
        );

        let default = field_tiers::stamp_default(field)
            .unwrap_or_else(|| panic!("{field} must declare a stamp-only default"));
        let declared: BTreeSet<String> = default
            .applicable_languages
            .iter()
            .map(|n| (*n).to_owned())
            .collect();

        assert_eq!(
            declared, languages,
            "{field}: the table declares {declared:?} and the shipped configs declare \
             {key:?} for {languages:?}. A language in the table and not in the configs \
             answers a default for an enricher that never ran on it; one in the configs \
             and not in the table withholds an answer the index can give"
        );

        computed.push(languages);
    }

    // Anti-vacuity: three sweeps that all computed the same set would pass
    // every assertion above while reading one key three times, or the wrong key
    // three times.
    assert!(
        computed.iter().any(|set| *set != computed[0]),
        "all three capability sweeps computed the same language set ({:?}), so this \
         test cannot tell the gates apart",
        computed[0]
    );
    // `has_shadow` is NOT in the sweep above, because its enricher reads no
    // language capability at all — its gates are the node kind and the grammar
    // node's `body` field, which no config declares. The test below settles it
    // from the grammars instead. It is still one of the four, so leaving it out
    // of both would be the gap: assert here that something else covers it.
    let shadow = field_tiers::stamp_default("has_shadow").expect("has_shadow declares a default");
    assert!(
        !shadow.applicable_languages.is_empty(),
        "has_shadow declares no languages, so its default speaks for no row and \
         reads as declared while behaving as absent",
    );
}

/// The languages whose config sends a function kind to `fql_kind = 'function'`.
///
/// Computed rather than counted, so the sample table in the test below cannot
/// quietly fall behind the plugins: a new language whose `kind_map` reaches
/// `function` appears here the day its config lands, and the test fails until
/// it is given a snippet.
fn languages_producing_function_rows() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (path, json) in parsed_configs() {
        let name = json
            .get("language")
            .and_then(|l| l.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{} declares no language.name", path.display()));
        let kinds: Vec<&str> = json
            .get("definitions")
            .and_then(|d| d.get("function_kinds"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let map = json.get("kind_map").and_then(Value::as_object);
        let reaches_function = kinds.iter().any(|raw| {
            map.and_then(|m| m.get(*raw))
                .and_then(Value::as_str)
                .is_some_and(|mapped| mapped == "function")
        });
        if reaches_function {
            let _ = out.insert(name.to_owned());
        }
    }
    out
}

/// The KINDS half of the declaration, checked the way the languages are.
///
/// `FUNCTION_ROWS` says the defaults speak for `fql_kind = 'function'`, and the
/// whole arithmetic rests on that being the rows the enrichers examined. But an
/// enricher gates on the RAW kind a config declares in
/// `definitions.function_kinds`, while a row's `fql_kind` is whatever
/// `kind_map` sent it to. Two lists, agreeing today, with nothing recomputing
/// it — and the corpus sums cannot: `'false'` is DEFINED as applicable minus
/// stored, so `true + false == the function rows` holds however wrong the
/// applicable set is, and an examined-and-unwritten row is byte-identical to
/// one nothing looked at.
///
/// So the check runs as the inverse. Every raw kind a config maps to `function`
/// must also be declared a function kind by that config; a `kind_map` entry
/// reaching `function` from a raw kind no enricher gate admits would put rows
/// in the applicable set that nothing ever read. That is the first stop this
/// work took, in configuration form rather than corpus form.
///
/// The OTHER direction is deliberately not asserted: cmake declares `macro_def`
/// a function kind and maps it to `macro`, which is the documented exclusion —
/// examined, and outside the kinds the declaration names.
#[test]
fn every_kind_that_becomes_a_function_row_is_one_an_enricher_gate_admits() {
    /// Reached today by c, cpp, python, rust, cmake, make and just.
    const FUNCTION_MAPPINGS_AT_LEAST: usize = 7;

    let mut checked = 0usize;
    for (path, json) in parsed_configs() {
        let map = json
            .get("kind_map")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{} declares no kind_map", path.display()));
        let declared: BTreeSet<&str> = json
            .get("definitions")
            .and_then(|d| d.get("function_kinds"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        for (raw, mapped) in map {
            if mapped.as_str() != Some("function") {
                continue;
            }
            checked += 1;
            assert!(
                declared.contains(raw.as_str()),
                "{}: kind_map sends `{raw}` to fql_kind = 'function', but \
                 definitions.function_kinds does not declare it — so no function \
                 enricher examines such a row while every stamp-only default \
                 speaks for it",
                path.display(),
            );
        }
    }

    assert!(
        checked >= FUNCTION_MAPPINGS_AT_LEAST,
        "found {checked} kind_map entries reaching 'function', expected at least \
         {FUNCTION_MAPPINGS_AT_LEAST} — a sweep that found none passes every \
         assertion above without reading a mapping",
    );
}

/// `has_shadow`'s languages, recomputed from the shipped GRAMMARS.
///
/// The three fields above are gated by a capability a config declares, so the
/// config sweep can read them. `ShadowEnricher` declares none: its only gates
/// are the node kind and the body node, which is a property of the grammar and
/// invisible to every config — so this one is derived from the grammars
/// instead. [`grammars_carrying_a_function_body`] says how, and why an outcome
/// test could not have found it.
#[test]
fn the_shadow_default_covers_exactly_the_grammars_whose_walk_can_start() {
    let walkable = grammars_carrying_a_function_body();
    let declared = declared_languages("has_shadow");
    assert_eq!(
        declared, walkable,
        "has_shadow declares {declared:?} and the grammars carry a `body` field on \
         the function kinds of {walkable:?}. A language in the declaration and not \
         in the grammars answers 'false' from a walk that never started; one in the \
         grammars and not in the declaration withholds an answer the index can give",
    );
}

/// The capability-gated default that ALSO walks a body.
///
/// `RecursionEnricher` reads `child_by_field_name("body")` and returns when
/// there is none, and the config sweep that computes its language list cannot
/// see that second gate. A future plugin declaring `expressions.call` on a
/// grammar with no function body would be added to `CALL_LANGUAGES` by that
/// sweep and would then answer a default from a walk that never started — the
/// config half saying yes and the grammar half saying no, with nothing
/// comparing them. This is what compares them.
///
/// Two fields are deliberately absent, for different reasons. `has_todo`'s scan
/// takes the body as OPTIONAL and reads the function's own comment children
/// besides, so it runs whether or not the grammar gives it one. `has_escape`
/// does not read the body field AT ALL — it gates on the node kind,
/// `has_address_of`, and whether the declaration scan found any locals — so
/// requiring a body of it would be a constraint its enricher does not have, and
/// would reject a future language that declared an address-of operator on a
/// bodyless function kind for a walk that would in fact have run. Checked
/// against the enrichers rather than assumed: of the four, `recursion`,
/// `shadow` and `todo` read the body field and `escape` does not.
#[test]
fn the_defaults_that_walk_a_body_only_cover_grammars_that_have_one() {
    // `is_recursive` is the only one left that needs this: of the four
    // enrichers, `recursion`, `shadow` and `todo` read the body field and
    // `escape` does not, `shadow` has its own grammar test above, and `todo`
    // treats the body as optional.
    let walkable = grammars_carrying_a_function_body();
    let field = "is_recursive";
    let declared = declared_languages(field);
    assert!(
        declared.is_subset(&walkable),
        "{field} declares {declared:?}, and only {walkable:?} carry a `body` \
         field on their function kinds. Its enricher returns without walking \
         for the difference, so the default would speak for rows nothing read",
    );
}

/// The languages a stamp-only default declares, as an owned set.
fn declared_languages(field: &str) -> BTreeSet<String> {
    forgeql_core::field_tiers::stamp_default(field)
        .unwrap_or_else(|| panic!("{field} declares a stamp-only default"))
        .applicable_languages
        .iter()
        .map(|n| (*n).to_owned())
        .collect()
}

/// Which languages' function kinds carry the `body` field a walk needs.
///
/// Several enrichers read `child_by_field_name("body")` and return when there
/// is none. That is a property of the GRAMMAR, invisible to every config, and
/// invisible to an outcome test as well: "walked the body and found nothing"
/// and "returned at the body gate" write exactly the same nothing, so the row
/// answers `'false'` either way and a fixture asserting the ANSWER passes under
/// both. Only the node can tell them apart, which is why this holds one.
///
/// It bites: a cmake `function_def` carries no `body` field. cmake and make
/// both declare function kinds that map to `fql_kind = 'function'`, both are
/// outside every capability list, and `has_shadow` was the one default that
/// would have spoken for their rows — 355 cmake functions and 47 Makefile rules
/// on Zephyr, answering "no shadowed variable" from a walk that never started.
fn grammars_carrying_a_function_body() -> BTreeSet<String> {
    use forgeql_core::ast::lang::LanguageSupport;

    /// Every language that produces `fql_kind = 'function'` rows, with a source
    /// snippet and the raw kind its config declares a function kind.
    ///
    /// Hand-written, so the set is checked against the configs below: a new
    /// plugin whose `kind_map` sends a function kind to `function` fails that
    /// check until it is given an entry here, rather than being silently left
    /// out of a set this then reports as complete.
    const SAMPLES: [(&str, &dyn LanguageSupport, &str, &str); 7] = [
        (
            "c",
            &forgeql_lang_c::CLanguage,
            "int f(int x) { return x; }\n",
            "function_definition",
        ),
        (
            "cpp",
            &forgeql_lang_cpp::CppLanguage,
            "int f(int x) { return x; }\n",
            "function_definition",
        ),
        (
            "rust",
            &forgeql_lang_rust::RustLanguage,
            "fn f(x: i32) -> i32 { x }\n",
            "function_item",
        ),
        (
            "python",
            &forgeql_lang_python::PythonLanguage,
            "def f(x):\n    return x\n",
            "function_definition",
        ),
        (
            "cmake",
            &forgeql_lang_text::CmakeLanguage,
            "function(f)\n  set(v 1)\nendfunction()\n",
            "function_def",
        ),
        (
            "make",
            &forgeql_lang_text::MakeLanguage,
            "target: dep\n\techo hi\n",
            "rule",
        ),
        (
            "just",
            &forgeql_lang_text::JustLanguage,
            "recipe:\n    echo hi\n",
            "recipe",
        ),
    ];

    fn first_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = first_of_kind(child, kind) {
                return Some(found);
            }
        }
        None
    }

    let sampled: BTreeSet<String> = SAMPLES.iter().map(|(n, ..)| (*n).to_owned()).collect();
    assert_eq!(
        sampled,
        languages_producing_function_rows(),
        "SAMPLES must hold one snippet per language that produces function rows; \
         a language in the configs and not in SAMPLES is one this says nothing about",
    );

    let mut walkable: BTreeSet<String> = BTreeSet::new();
    for (name, lang, source, kind) in SAMPLES {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");
        let tree = parser.parse(source.as_bytes(), None).expect("parse");
        let node = first_of_kind(tree.root_node(), kind).unwrap_or_else(|| {
            panic!("no {kind} node in the {name} snippet — the snippet is wrong, not the grammar")
        });
        if node.child_by_field_name("body").is_some() {
            let _ = walkable.insert(name.to_owned());
        }
    }
    assert!(
        walkable.len() < SAMPLES.len(),
        "every sampled grammar carries a `body` field, so this cannot tell a \
         walkable kind from an unwalkable one and would pass with the field name \
         misspelt",
    );
    walkable
}

/// No language may map a node to the kind the engine RENDERS for a kindless
/// row.
///
/// A row nothing maps stores the empty kind, and `SHOW outline` / `SHOW
/// members` print it as `unknown`. Since both spellings name the same rows,
/// the row side is spelled to the stored one before an equality is decided —
/// so a kind genuinely called `unknown` would answer as a kindless row and a
/// kindless row would answer as one of it, in both directions, with nothing
/// distinguishing them.
///
/// Nothing maps to it today, which is what makes the spelling safe. That is a
/// property of the shipped configs and not of the engine, and no other test
/// holds it: [`FQL_KIND_VALUES`] deliberately CARRIES `unknown`, so the sweep
/// above accepts such a mapping rather than rejecting it. A plugin adding one
/// would be a silent conflation, so it fails here instead.
#[test]
fn no_language_maps_a_node_to_the_rendered_kindless_spelling() {
    let rendered = forgeql_core::field_tiers::UNKNOWN_KIND;
    let mut checked = 0usize;
    for (path, json) in parsed_configs() {
        let map = json
            .get("kind_map")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{}: no kind_map object", path.display()));
        for (raw_kind, mapped) in map {
            assert_ne!(
                mapped.as_str(),
                Some(rendered),
                "{}: kind_map sends `{raw_kind}` to fql_kind '{rendered}', the spelling the \
                 engine renders for a row that has NO kind. The two would be indistinguishable \
                 to `WHERE fql_kind = '{rendered}'` and to its negation. Give the kind its own \
                 name, or the rendered spelling has to stop being a value a query can carry",
                path.display(),
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no kind_map entry was read");
}
