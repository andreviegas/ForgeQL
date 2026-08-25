//! The engine-owned value universes must not fall behind the plugins.
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
//! What the configs cannot show — the kinds and roles the engine mints itself —
//! is deliberately NOT asserted here. Those are tied to the two lists by a
//! compile-time reference at the site that writes them, so any assertion this
//! file could make about them would be a tautology over the list it is
//! checking. The comment further down says it at length, because the tautology
//! is the tempting thing to write.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
