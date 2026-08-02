//! Every config this crate ships must survive strict deserialization.
//!
//! A config is compiled into the crate and parsed the first time its language
//! is used, so a key the schema does not declare would otherwise stay invisible
//! until someone noticed the feature it configures doing nothing.
//!
//! The directory is scanned rather than listed. A format added later is covered
//! without anyone remembering to add it here, which is the failure mode a
//! hand-written list has.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use forgeql_core::ast::lang_json::LanguageConfigJson;

#[test]
fn every_config_in_this_crate_deserializes_strictly() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config");
    let configs: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("config directory must exist")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();

    assert!(
        !configs.is_empty(),
        "no configs found in {} — the scan is silently covering nothing",
        dir.display()
    );

    let failures: Vec<String> = configs
        .iter()
        .filter_map(|path| {
            let bytes = fs::read(path).expect("readable config");
            LanguageConfigJson::from_json_bytes(&bytes)
                .err()
                .map(|err| format!("{}: {err}", path.display()))
        })
        .collect();

    assert!(
        failures.is_empty(),
        "configs that do not deserialize strictly:\n  {}",
        failures.join("\n  ")
    );
}
