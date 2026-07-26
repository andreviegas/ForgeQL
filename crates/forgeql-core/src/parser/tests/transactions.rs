//! Parser tests for `BEGIN TRANSACTION` / `VERIFY` / `COMMIT` sequences and
//! the `JOB` verbs.
//!
//! Not every test that mentions a transaction is here. Two live in the parent
//! with the statement they are really about:
//! `parse_commit_message_heredoc_with_apostrophes` is a heredoc-quoting test
//! and `parse_change_in_transaction_sequence` is a `CHANGE` test.

use crate::parser::*;

#[test]
fn parse_transaction() {
    let fql = "BEGIN TRANSACTION 'refactor-signal'";
    let ops = parse(fql).unwrap();
    match &ops[0] {
        ForgeQLIR::BeginTransaction { name } => {
            assert_eq!(name, "refactor-signal");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_commit_standalone() {
    let fql = "COMMIT MESSAGE 'Refactor signal controller'";
    let ops = parse(fql).unwrap();
    match &ops[0] {
        ForgeQLIR::Commit { message } => {
            assert_eq!(message, "Refactor signal controller");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_checkpoint_sequence() {
    let fql = "BEGIN TRANSACTION 'refactor-signal'\n\
                   CHANGE FILE 'src/signal.cpp' MATCHING 'setPeakLevel' WITH 'setMaxIntensity'\n\
                   VERIFY build 'release'\n\
                   COMMIT MESSAGE 'Refactor signal controller'";
    let ops = parse(fql).unwrap();
    assert_eq!(ops.len(), 4);
    assert!(matches!(&ops[0], ForgeQLIR::BeginTransaction { .. }));
    assert!(matches!(&ops[1], ForgeQLIR::ChangeContent { .. }));
    assert!(matches!(&ops[2], ForgeQLIR::VerifyBuild { .. }));
    assert!(matches!(&ops[3], ForgeQLIR::Commit { .. }));
}

#[test]
fn parse_job_commands() {
    match &parse("JOB START 'test-all'").unwrap()[0] {
        ForgeQLIR::JobStart { label, args } => {
            assert_eq!(label, "test-all");
            assert!(args.is_empty());
        }
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("JOB START 'bless' 'zephyr' 'pytorch'").unwrap()[0] {
        ForgeQLIR::JobStart { label, args } => {
            assert_eq!(label, "bless");
            assert_eq!(args, &["zephyr".to_string(), "pytorch".to_string()]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("JOB STATUS 'j-00001a'").unwrap()[0] {
        ForgeQLIR::JobStatus { id } => assert_eq!(id, "j-00001a"),
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("JOB LIST").unwrap()[0] {
        ForgeQLIR::JobList => {}
        other => panic!("wrong variant: {other:?}"),
    }
}
