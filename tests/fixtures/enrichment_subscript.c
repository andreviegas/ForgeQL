/* Zero-subscript exemption, C grammar.
 *
 * tree-sitter-c names the subscript index field directly, so the literal's
 * immediate parent is the `subscript_expression` the enricher looks for and
 * the 0 is exempt. The assignment form is deliberate: there is no
 * `init_declarator` here, so nothing but the subscript rule can produce the
 * exemption. The 1 on the same line is an ordinary magic literal.
 *
 * The C++ grammar interposes a `subscript_argument_list` and the same rule
 * never fires — pinned as an open defect against the .cpp fixture.
 */
void subscript_exemption(int *buf)
{
	buf[1] = buf[0];
}
