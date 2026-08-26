//! Marker comments in the leading position of a function body, under the Rust
//! grammar. Not compiled — this file is fixture text the test registry indexes.

fn leading_marker() -> i32 {
    // TODO: opens the body
    1
}

fn marker_after_statement() -> i32 {
    let value = 1;
    // TODO: the position that always worked
    value
}

fn marker_before_the_body() -> i32
// TODO: between the signature and the body
{
    2
}

// TODO: a doc comment, preceding the function and outside it
fn no_marker() -> i32 {
    3
}

// The documented boundary, given a body so it can be queried: Rust declares
// `line_comment` as its comment kind and `is_comment_kind` is an equality, so
// this `/* */` marker is invisible to the scan in every position — leading or
// not. The test that names this function asserts it answers NO marker; when
// that gap closes, the test goes red and every doc site that states the
// exclusion has to move with it — the assertion message says how to find them.
fn block_comment_marker() -> i32 {
    /* TODO: a block comment the scan does not reach */
    4
}
