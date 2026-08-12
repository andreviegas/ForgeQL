//! Tests for the resident-memory reading.
//!
//! The parse is tested against fixed blobs rather than against this machine:
//! a test that asserts on whatever `/proc/self/status` says right now can only
//! assert that a number is a number, which the parse already guarantees.

use super::{MemSnapshot, parse_status, snapshot};

/// A trimmed `/proc/self/status`, keeping the shape that matters: the three
/// fields the parse wants, separated by fields it must skip, with the tab and
/// the right-aligned value real kernels emit.
const STATUS: &str = "\
Name:\tforgeql
State:\tR (running)
VmPeak:\t 9999999 kB
VmSize:\t 8888888 kB
VmHWM:\t 5242880 kB
VmRSS:\t 4194304 kB
RssAnon:\t 1048576 kB
RssFile:\t 3145728 kB
RssShmem:\t       0 kB
Threads:\t16
";

#[test]
fn every_field_is_read_from_its_own_line() {
    assert_eq!(
        parse_status(STATUS),
        MemSnapshot::Read {
            anon_kb: 1_048_576,
            file_kb: 3_145_728,
            peak_kb: 5_242_880,
        }
    );
}

/// A field that is present but unparsable is a missing field, not a zero. A
/// zero would render as a measurement — "this phase used no memory" — which is
/// the one reading the log must never invent.
#[test]
fn an_unparsable_value_is_missing_rather_than_zero() {
    let mangled = STATUS.replace("RssAnon:\t 1048576 kB", "RssAnon:\t   ? kB");
    assert_eq!(parse_status(&mangled), MemSnapshot::Unavailable);
}

/// The same rule for a field the kernel does not publish at all. `RssAnon` and
/// `RssFile` are absent on kernels before 4.5, so this is a real shape rather
/// than a hypothetical one.
#[test]
fn a_missing_field_makes_the_whole_reading_unavailable() {
    let without = STATUS
        .lines()
        .filter(|l| !l.starts_with("RssFile:"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(parse_status(&without), MemSnapshot::Unavailable);
}

#[test]
fn nothing_at_all_is_unavailable_rather_than_a_panic() {
    assert_eq!(parse_status(""), MemSnapshot::Unavailable);
}

/// A prefix must not be taken for the field itself: `VmHWM` is the peak, and a
/// parse keying on `starts_with` would read `VmHWM` out of nothing here but
/// would read `RssAnon` out of a hypothetical `RssAnonymous`. Splitting on the
/// colon is what makes the key exact, and this pins it.
#[test]
fn a_longer_key_with_the_same_prefix_is_not_the_field() {
    let decoyed = STATUS.replace("RssAnon:\t 1048576 kB", "RssAnonOther:\t 7 kB");
    assert_eq!(
        parse_status(&decoyed),
        MemSnapshot::Unavailable,
        "RssAnonOther is not RssAnon, so the reading is incomplete"
    );
}

/// Rendering is what reaches the log, so the units are part of the contract:
/// three significant figures, largest fitting unit, and the process's own
/// memory named first.
#[test]
fn display_names_anonymous_memory_first() {
    let rendered = MemSnapshot::Read {
        anon_kb: 1_048_576,
        file_kb: 3_145_728,
        peak_kb: 5_242_880,
    }
    .to_string();
    assert_eq!(rendered, "anon=1.0GiB file=3.0GiB peak=5.0GiB");
}

#[test]
fn display_steps_down_through_the_units() {
    let small = MemSnapshot::Read {
        anon_kb: 512,
        file_kb: 2_048,
        peak_kb: 1_048_576,
    };
    assert_eq!(small.to_string(), "anon=512KiB file=2MiB peak=1.0GiB");
}

#[test]
fn display_says_so_when_there_is_nothing_to_report() {
    assert_eq!(MemSnapshot::Unavailable.to_string(), "unavailable");
}

/// The reading is diagnostic, so the one thing it must never do is fail the
/// work it is measuring. On Linux it reports; anywhere else it says it cannot,
/// and either way it returns.
#[test]
fn a_live_reading_returns_rather_than_failing() {
    match snapshot() {
        MemSnapshot::Read {
            anon_kb, peak_kb, ..
        } => {
            assert!(anon_kb > 0, "a running process holds anonymous memory");
            assert!(
                peak_kb >= anon_kb,
                "the peak is a high-water mark over both kinds, so it cannot \
                 sit below the anonymous half: peak={peak_kb} anon={anon_kb}"
            );
        }
        MemSnapshot::Unavailable => {}
    }
}
