# Canonical Fixture Line Contract

Lines are 1-indexed.  Both `canonical.cpp` and `canonical.rs` share the
same line numbers for every universal construct.

## Symbol Table

| Line | Symbol      | fql_kind  | Enrichment Coverage                                      |
|------|-------------|-----------|----------------------------------------------------------|
|  1   | `foo`       | function  | NamingEnricher, MetricsEnricher                          |
|  5   | `Motor`     | struct    | NamingEnricher, CommentEnricher                          |
|  6   | `speed`     | field     | NamingEnricher, MemberEnricher (owner=Motor)             |
|  9   | `State`     | enum      | NamingEnricher                                           |
| 15   | (comment)   | comment   | doc comment `/// Documented …`                           |
| 16   | `bar`       | function  | CommentEnricher(has_doc), ControlFlowEnricher(if)        |
| 23   | `count`     | variable  | NumberEnricher(42, decimal), ScopeEnricher               |
| 25   | (comment)   | comment   | doc comment `/// Recursive factorial`                    |
| 26   | `factorial` | function  | RecursionEnricher, CommentEnricher(has_doc)              |
| 31   | `process`   | function  | TodoEnricher(TODO+FIXME), UnusedParamEnricher(unused)    |
| 37   | `helper`    | function  | ScopeEnricher(static/C++), ControlFlowEnricher(for/if/while), MetricsEnricher |
| 49   | `hex_value` | variable  | NumberEnricher(hex, 0xFF)                                |
| 50   | `bin_value` | variable  | NumberEnricher(binary, 0b1010)                           |
| 51   | `pi`        | variable  | NumberEnricher(float, 3.14159)                           |
| 53   | `transform` | function  | OperatorEnricher(+=, <<), CastEnricher                   |
| 61   | `checker`   | function  | RedundancyEnricher(duplicate condition)                  |
| 70   | `shadowed`  | function  | ShadowEnricher(x)                                       |
| 77   | `escaping`  | function  | EscapeEnricher(&local)                                   |
| 83   | `switcher`  | function  | ControlFlowEnricher(has_catch_all), FallthroughEnricher(C++ only) |
| 92   | `distant`   | function  | DeclDistanceEnricher(early used far)                     |
| 101  | `caller`    | function  | callees: bar, factorial                                  |

## Enricher Coverage Matrix

All 17 enrichers are exercised by at least one symbol:

| #  | Enricher              | Primary Symbol(s)            |
|----|-----------------------|------------------------------|
| 1  | ScopeEnricher         | helper (static), count       |
| 2  | NamingEnricher        | all named symbols            |
| 3  | CommentEnricher       | bar, factorial (has_doc)     |
| 4  | NumberEnricher        | count, hex_value, bin_value, pi |
| 5  | ControlFlowEnricher   | bar, helper, checker, switcher |
| 6  | OperatorEnricher      | transform (+=, <<)           |
| 7  | MetricsEnricher       | all functions                |
| 8  | CastEnricher          | transform                    |
| 9  | RedundancyEnricher    | checker                      |
| 10 | MemberEnricher        | speed (field of Motor)       |
| 11 | DeclDistanceEnricher  | distant                      |
| 12 | EscapeEnricher        | escaping                     |
| 13 | ShadowEnricher        | shadowed                     |
| 14 | UnusedParamEnricher   | process                      |
| 15 | FallthroughEnricher   | switcher (C++ only)          |
| 16 | RecursionEnricher     | factorial                    |
| 17 | TodoEnricher          | process                      |

## Line derivation (for const structs)

```
FOO_LINE       = 1
MOTOR_LINE     = FOO_LINE       + 4   // 5
SPEED_LINE     = MOTOR_LINE     + 1   // 6
STATE_LINE     = MOTOR_LINE     + 4   // 9
DOC_LINE       = STATE_LINE     + 6   // 15
BAR_LINE       = DOC_LINE       + 1   // 16
COUNT_LINE     = BAR_LINE       + 7   // 23
FACTORIAL_LINE = COUNT_LINE     + 3   // 26
PROCESS_LINE   = FACTORIAL_LINE + 5   // 31
HELPER_LINE    = PROCESS_LINE   + 6   // 37
HEX_LINE       = HELPER_LINE   + 12  // 49
BIN_LINE       = HEX_LINE      + 1   // 50
PI_LINE        = BIN_LINE       + 1   // 51
TRANSFORM_LINE = PI_LINE        + 2   // 53
CHECKER_LINE   = TRANSFORM_LINE + 8  // 61
SHADOWED_LINE  = CHECKER_LINE   + 9  // 70
ESCAPING_LINE  = SHADOWED_LINE  + 7  // 77
SWITCHER_LINE  = ESCAPING_LINE  + 6  // 83
DISTANT_LINE   = SWITCHER_LINE  + 9  // 92
CALLER_LINE    = DISTANT_LINE   + 9  // 101
```

## Cross-language notes

- `helper` is `static` in C++ (ScopeEnricher: scope=file, storage=static) but
  simply non-`pub` in Rust (is_exported=false). Both exercise ScopeEnricher.
- `switcher` uses `switch/case/default` in C++ and `match/_` in Rust.
  FallthroughEnricher fires only for C++ (case 2: case 3: fallthrough).
- `escaping` uses `int* ptr = &local` in C++ and `let ptr = &local` in Rust.
  Both use the language's address-of / reference expression.
- `transform` uses C-style cast `(int)y` in C++ and `y as i32` in Rust.
- `helper` uses `for(;;)` in C++ and `for in` in Rust; `sum--` vs `sum -= 1`.
- OperatorEnricher: `++`/`--` only fire for C++; Rust uses `+= 1` / `-= 1`.
