//! Parser tests for `BEGIN TRANSACTION` / `VERIFY` / `COMMIT` sequences and
//! the `JOB` verbs.
//!
//! Not every test that mentions a transaction is here. Two live in
//! `mutations.rs` with the statement they are really about:
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

/// A step argument must be able to carry prose, and prose carries both quote
/// characters.
///
/// A quoted argument cannot hold the quote that delimits it, and this DSL
/// escapes neither by doubling nor by backslash: `''` splits the argument in
/// two and `\'` fails to parse. So anything holding an apostrophe *and* a
/// double quote — an issue body quoting an error message beside a
/// `WHERE name = 'x'` snippet — was unwritable at any length. The body is
/// bound to the step's stdin, so the limit was never size; it was quoting.
#[test]
fn parse_run_argument_heredoc_carries_both_quote_characters() {
    let fql = "RUN 'file_issue' <<BODY\n\
               The engine answers `WHERE name = 'parse_offset'` with a row that\n\
               carries no handle, and the message reads \"not found\".\n\
               It doesn't refuse — that is the defect.\n\
               BODY";
    match &parse(fql).unwrap()[0] {
        ForgeQLIR::Run { step, args } => {
            assert_eq!(step, "file_issue");
            assert_eq!(args.len(), 1, "the heredoc is one argument, not several");
            let body = &args[0];
            assert!(body.contains("'parse_offset'"), "apostrophes lost: {body}");
            assert!(body.contains("\"not found\""), "double quotes lost: {body}");
            assert!(body.contains("doesn't refuse"), "apostrophe lost: {body}");
            assert!(
                !body.contains("BODY"),
                "the delimiter leaked into the value: {body}"
            );
            assert_eq!(body.lines().count(), 3, "line structure lost: {body}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

/// `VERIFY build` takes its arguments through the same rule, so it gains the
/// same ability — and must keep accepting quoted ones.
#[test]
fn parse_verify_and_run_still_take_quoted_arguments() {
    match &parse("VERIFY build 'release' 'x86_64'").unwrap()[0] {
        ForgeQLIR::VerifyBuild { step, args } => {
            assert_eq!(step, "release");
            assert_eq!(args, &["x86_64".to_string()]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("RUN 'bench_mem' 'all'").unwrap()[0] {
        ForgeQLIR::Run { step, args } => {
            assert_eq!(step, "bench_mem");
            assert_eq!(args, &["all".to_string()]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    // JOB START submits a verify step with arguments through the same binding,
    // so it must take the same arguments as the VERIFY spelling of that step.
    match &parse("JOB START 'gate' <<NOTE\nheld 'x' and \"y\"\nNOTE").unwrap()[0] {
        ForgeQLIR::JobStart { label, args } => {
            assert_eq!(label, "gate");
            assert_eq!(args, &["held 'x' and \"y\"".to_string()]);
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
    match &parse("VERIFY build 'gate' <<NOTE\nran with 'x' and \"y\"\nNOTE").unwrap()[0] {
        ForgeQLIR::VerifyBuild { step, args } => {
            assert_eq!(step, "gate");
            assert_eq!(args, &["ran with 'x' and \"y\"".to_string()]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
