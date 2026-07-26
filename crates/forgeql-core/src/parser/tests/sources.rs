//! Parser tests for the source and session verbs: `USE`, `CREATE SOURCE`,
//! `REFRESH SOURCE`, `VACUUM`, and the `SHOW SOURCES` / `SHOW BRANCHES` /
//! `SHOW VERSION` / `SHOW STATS` readouts.
//!
//! `SHOW` statements that read *code* rather than session state are not here;
//! they live with the rest of the code-exposure tests.

use crate::parser::*;

#[test]
fn parse_create_source() {
    let ops = parse("CREATE SOURCE 'pisco' FROM 'git@github.com:org/pisco.git'").unwrap();
    match &ops[0] {
        ForgeQLIR::CreateSource { name, url } => {
            assert_eq!(name, "pisco");
            assert_eq!(url, "git@github.com:org/pisco.git");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_refresh_source() {
    let ops = parse("REFRESH SOURCE 'pisco-code'").unwrap();
    match &ops[0] {
        ForgeQLIR::RefreshSource { name } => {
            assert_eq!(name, "pisco-code");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_vacuum() {
    let ops = parse("VACUUM SOURCE 'pisco-code' KEEP 2 ALL APPLY").unwrap();
    match &ops[0] {
        ForgeQLIR::Vacuum {
            source,
            keep,
            all,
            apply,
        } => {
            assert_eq!(source.as_deref(), Some("pisco-code"));
            assert_eq!(*keep, 2);
            assert!(*all);
            assert!(*apply);
        }
        _ => panic!("wrong variant"),
    }
    // Bare VACUUM previews every source with conservative defaults.
    let ops = parse("VACUUM").unwrap();
    match &ops[0] {
        ForgeQLIR::Vacuum {
            source,
            keep,
            all,
            apply,
        } => {
            assert_eq!(*source, None);
            assert_eq!(*keep, 0);
            assert!(!*all);
            assert!(!*apply);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_use_source_without_as_is_error() {
    // USE without AS 'branch-name' must be a parse error (grammar enforces it)
    assert!(
        parse("USE pisco.main").is_err(),
        "USE without AS should be a parse error"
    );
    assert!(
        parse("USE pisco-code.main").is_err(),
        "USE without AS (hyphenated source) should be a parse error"
    );
}

#[test]
fn parse_use_source_with_as() {
    // plain identifier
    let ops = parse("USE pisco.main AS 'my-feature'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "pisco");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "my-feature");
        }
        _ => panic!("wrong variant"),
    }
    // hyphenated source name
    let ops2 = parse("USE pisco-code.main AS 'refactor'").unwrap();
    match &ops2[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "pisco-code");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "refactor");
        }
        _ => panic!("wrong variant for hyphenated name"),
    }
}

#[test]
fn parse_use_source_commit_hash_branch() {
    // Digit-led commit hash — unparseable before commit-base support was added.
    let ops = parse("USE forgeql-pub.594cc8b AS 'review'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "forgeql-pub");
            assert_eq!(branch, "594cc8b");
            assert_eq!(as_branch, "review");
        }
        _ => panic!("wrong variant for commit-hash branch"),
    }
    // A full 40-char hash parses too.
    let ops2 = parse("USE forgeql-pub.0c7a0fb14ea282d260b1b2af035c8c55a174e437 AS 'r2'").unwrap();
    match &ops2[0] {
        ForgeQLIR::UseSource { branch, .. } => {
            assert_eq!(branch, "0c7a0fb14ea282d260b1b2af035c8c55a174e437");
        }
        _ => panic!("wrong variant for full-hash branch"),
    }
}

#[test]
fn parse_show_sources() {
    let ops = parse("SHOW SOURCES").unwrap();
    assert!(matches!(ops[0], ForgeQLIR::ShowSources));
}

#[test]
fn parse_show_branches_with_source_is_rejected() {
    let q = char::from(39u8);
    let input = format!("SHOW BRANCHES OF {q}pisco{q}");
    assert!(parse(&input).is_err());
}

#[test]
fn parse_show_branches() {
    let ops = parse("SHOW BRANCHES").unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], ForgeQLIR::ShowBranches));
}

#[test]
fn parse_show_version() {
    let ops = parse("SHOW VERSION").unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], ForgeQLIR::ShowVersion));
}

#[test]
fn parse_show_stats_no_session() {
    let ops = parse("SHOW STATS").unwrap();
    assert!(matches!(ops[0], ForgeQLIR::ShowStats { session_id: None }));
}

#[test]
fn parse_show_stats_for_session() {
    let ops = parse("SHOW STATS FOR 'my-session'").unwrap();
    assert!(matches!(
        ops[0],
        ForgeQLIR::ShowStats { session_id: Some(ref s) } if s == "my-session"
    ));
}

// (parse_disconnect test removed — DISCONNECT command eliminated)

#[test]
fn parse_use_source_without_as_is_a_parse_error() {
    // This test replaces parse_use_source_without_as_has_no_as_branch.
    // AS 'branch-name' is now mandatory — omitting it must be a parse error.
    let err = parse("USE pisco-code.main");
    assert!(err.is_err(), "USE without AS should be a parse error");
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("USE requires an AS clause"),
        "error should hint about AS clause; got: {msg}"
    );
}

#[test]
fn parse_use_source_as_sets_as_branch() {
    let ops = parse("USE pisco-code.main AS 'agent/refactor-signal-api'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "pisco-code");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "agent/refactor-signal-api");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_use_missing_dot_is_error() {
    // USE requires "source.branch" with a dot separator.
    assert!(
        parse("USE pisco main AS 'my-alias'").is_err(),
        "USE without dot separator should be rejected"
    );
}

// ── Relaxed quoting (double-quoted and bare values) ──────────────────────

#[test]
fn parse_use_source_hyphenated_branch() {
    // Branch position now uses source_name instead of identifier → hyphens accepted.
    let ops = parse("USE forgeql-pub.line-budget AS 'lb2'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "forgeql-pub");
            assert_eq!(branch, "line-budget");
            assert_eq!(as_branch, "lb2");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_use_source_bare_as_branch() {
    // AS target accepts bare (unquoted) value.
    let ops = parse("USE forgeql-pub.main AS my-feature").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "forgeql-pub");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "my-feature");
        }
        _ => panic!("wrong variant"),
    }
}
