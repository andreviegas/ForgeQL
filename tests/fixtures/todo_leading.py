"""Marker comments in the leading position of a function body.

tree-sitter-python does not open the body block until the first statement, so
a comment written as the first line of a body is parsed as a sibling of the
block rather than a child of it. A walk that visits only the body misses
exactly that position: `leading_marker` and `decorated_leading_marker` answer
no marker under such a walk, while `docstring_then_marker` — one statement
further in — answers under either, which is what makes the pair evidence that
the variable is position rather than the file.

The other two functions bound the region from the far side. `trailing_marker`
is the mirror position — a marker on the LAST line of the body — and it is NOT
a blind spot: the block has already opened, so that comment is inside it and
answered before this change as well. `marker_between_decorator_and_def` IS
outside the region: it is a child of the wrapper, a sibling of the definition
that owns the row, and stays unscanned. `no_marker` bounds it from a third
side: the marker above it is the function's doc comment, a sibling of the
function node, so `no_marker` must answer no marker at all.
"""


def leading_marker():
    # TODO: opens the body
    return 1


def docstring_then_marker():
    """A docstring is a statement, so the block opens on it."""
    # TODO: sits inside the block, one line further in
    return 2


def marker_after_statement():
    value = 1
    # TODO: the position that always worked
    return value


# TODO: a doc comment, preceding the function and outside it
def no_marker():
    return 3


@deco
def decorated_leading_marker():
    # TODO: leading and decorated at once
    return 4


def trailing_marker():
    value = 1
    return value
    # TODO: the last line of the body


@deco
# TODO: between the decorator and the def
def marker_between_decorator_and_def():
    return 6
