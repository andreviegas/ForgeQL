/* Marker comments in the leading position of a function body.
 *
 * A brace-delimited grammar puts a leading comment inside the compound
 * statement, so this position already answered. These functions pin that it
 * still does, and that scanning the function node beside its body counts each
 * marker once and not twice.
 */

int leading_marker(void)
{
    /* TODO: opens the body */
    return 1;
}

int marker_after_statement(void)
{
    int value = 1;
    /* TODO: the position that always worked */
    return value;
}

int marker_before_the_body(void) /* TODO: between the signature and the body */
{
    return 2;
}

// TODO: a doc comment, preceding the function and outside it
int no_marker(void)
{
    return 3;
}
