# Fixture for the LANGUAGE half of the stamp-only applicable set.
#
# Python declares no address-of operator, so EscapeEnricher returns before it
# reads anything and no Python function has ever been examined for escaping
# locals. Every function here must therefore answer NEITHER value for
# has_escape — not 'true', and not the default either.
#
# The other three enrichers DO run on Python: it declares a comment kind and a
# call expression, and the shadow enricher gates on the node kind and nothing
# else. So the very same rows answer 'true' or 'false' for has_todo,
# is_recursive and has_shadow. One file, one language, four fields, and the
# fields disagree — which is the whole claim.


def plain_function(x):
    return x + 1


def function_with_todo(x):
    # TODO: this one is stamped, so it is not a default
    return x + 2


def recursive_function(x):
    if x <= 0:
        return 0
    return recursive_function(x - 1)


def shadowing_function(x):
    total = 0

    def inner():
        total = 1
        return total

    return inner() + total
