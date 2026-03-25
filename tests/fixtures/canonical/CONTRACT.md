# Canonical Fixture Line Contract

Lines are 1-indexed.  Both `canonical.cpp` and `canonical.rs` share the
same line numbers for every universal construct.

| Line | Symbol   | fql_kind   | Notes                          |
|------|----------|------------|--------------------------------|
|  1   | `foo`    | function   | simple function, no doc        |
|  5   | `Motor`  | struct     | struct definition               |
|  6   | `speed`  | field      | field inside `Motor`            |
|  9   | `State`  | enum       | enum definition                 |
| 15   | (comment)| comment    | doc comment `/// Documented …`  |
| 16   | `bar`    | function   | documented fn, has_doc=true     |
| 23   | `count`  | variable   | top-level variable              |

## Line derivation (for const structs)

```
FOO_LINE   = 1
MOTOR_LINE = FOO_LINE   + 4   // 5
SPEED_LINE = MOTOR_LINE + 1   // 6
STATE_LINE = MOTOR_LINE + 4   // 9
DOC_LINE   = STATE_LINE + 6   // 15
BAR_LINE   = DOC_LINE   + 1   // 16
COUNT_LINE = BAR_LINE   + 7   // 23
```
