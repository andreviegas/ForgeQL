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
//! Two things the configs cannot show are asserted separately: the kinds the
//! indexer mints itself (`error`, and the empty kind a row carries when nothing
//! maps it) and the roles the read pass mints (`code`, `text`). A universe
//! built only from what the configs declare would refuse the commonest role
//! there is.

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
        if let Some(map) = json.get("kind_map").and_then(Value::as_object) {
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
    for (path, json) in parsed_configs() {
        let Some(kinds) = json
            .get("syntax")
            .and_then(|s| s.get("mention_text_kinds"))
            .and_then(Value::as_object)
        else {
            continue;
        };
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
    assert!(
        checked > 0,
        "no role was checked — no config declared a mention kind"
    );
}

#[test]
fn the_kinds_the_indexer_mints_are_in_the_universe_too() {
    // The indexer writes these `fql_kind` strings itself rather than reading
    // them out of a `kind_map`, so the config sweep above does not decide
    // whether they are covered — a rename on the core side would slip past it.
    //
    // - `error` is minted for a span the parser could not parse.
    // - `guard` is minted for a conditional directive's region.
    // - `cast` is written as a literal by the cast enricher, because a named
    //   cast's raw node is a call expression rather than a cast one.
    // - `macro_call` is written as a literal by the row emitter for a macro
    //   invocation.
    // - the empty kind is what a row carries when nothing maps its grammar
    //   node, and `GROUP BY fql_kind` publishes those rows under it — so `= ''`
    //   is a question the engine puts into an agent's hands.
    //
    // Three of the five ALSO appear in a `kind_map` today (`guard`, `cast` and
    // `macro_call` in the C/C++/Rust configs), which is exactly why they are
    // named here: the config sweep passing says nothing about the core route
    // they actually travel.
    for kind in ["error", "guard", "cast", "macro_call", ""] {
        assert!(
            FQL_KIND_VALUES.contains(&kind),
            "the indexer mints fql_kind '{kind}' and FQL_KIND_VALUES does not carry it"
        );
    }
}

#[test]
fn the_roles_the_read_pass_mints_are_in_the_universe_too() {
    // `code` is a resolved identifier and `text` is a site found by reading the
    // file rather than by a posting. No config declares either, and `code` is
    // the commonest role a `FIND usages` answer carries.
    for role in ["code", "text"] {
        assert!(
            USAGE_ROLE_VALUES.contains(&role),
            "the read pass mints role '{role}' and USAGE_ROLE_VALUES does not carry it"
        );
    }
}

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
