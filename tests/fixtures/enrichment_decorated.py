"""Decorated definitions — the wrapper must not become a row of its own.

tree-sitter-python wraps a decorated `def` or `class` in a
`decorated_definition`. The definition it wraps already produces a row whose
span is folded back to the leading decorator, so naming the wrapper too
produced a second row with the same name, kind, path and line — which the
dedupe collapsed, keeping the wrapper. The wrapper is not a function kind, so
the surviving row answered no function metric at all.
"""


def plain(a, b):
    return a + b


@deco
def decorated(a, b, c):
    # the wrapper used to hide this body from every function enricher
    return a


@deco
class Decorated:
    pass


class Plain:
    pass
