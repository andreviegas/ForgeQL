/* enrichment_bug2b.c — Regression fixture for Bug 2b.
 *
 * Bug 2b: tree-sitter ERROR-recovery phantom number_literal nodes.
 *
 * When `module_param(identifier, int, number)` appears at file scope, using a
 * type keyword (int) as a macro argument confuses tree-sitter-cpp's parser.
 * It misreads the parenthesised argument list as a parameter declaration,
 * which cascades into ERROR recovery for subsequent macro calls.  The
 * error-recovery tokeniser then re-lexes the string content of
 * MODULE_PARM_DESC arguments and emits phantom `number_literal` nodes as
 * direct children of the ERROR node (not inside any string_literal).
 *
 * The sentinel values 8881 and 8882 are chosen to be unique; they must NOT
 * appear in the ForgeQL symbol index.
 */

module_param(bug2b_p1, int, 0);
MODULE_PARM_DESC(bug2b_p1, "rate config: values 1 through 9");

module_param(bug2b_p2, int, 0);
MODULE_PARM_DESC(bug2b_p2, "threshold config");

module_param(bug2b_p3, int, 0);
MODULE_PARM_DESC(bug2b_p3, "upper bound "
                           "8881 minimum, 8882 maximum");
