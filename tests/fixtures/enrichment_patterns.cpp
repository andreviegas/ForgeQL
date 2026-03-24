/**
 * enrichment_patterns.cpp — Test fixture for enrichment integration tests.
 *
 * Every section is labeled with the enricher it exercises.
 * DO NOT reformat — line numbers and exact text are used by tests.
 */

#include <cstdint>
#include <cstdio>
#include <cstring>

/* ------------------------------------------------------------------ */
/* NamingEnricher patterns                                              */
/* ------------------------------------------------------------------ */

int camelCaseVar = 0;
int PascalCaseVar = 0;
int snake_case_var = 0;
int UPPER_SNAKE_VAR = 0;
int flatcasevar = 0;

/* ------------------------------------------------------------------ */
/* CommentEnricher patterns                                             */
/* ------------------------------------------------------------------ */

/// This is a doc-line comment
int docLineTarget = 1;

/** This is a doc-block comment */
void docBlockFunction(void) {}

/* This is a plain block comment */
void noDocFunction(void) {}

// This is a plain line comment
void anotherNoDocFunction(void) {}

/* ------------------------------------------------------------------ */
/* NumberEnricher patterns                                              */
/* ------------------------------------------------------------------ */

static const int decNum = 42;
static const int hexNum = 0xFF;
static const int binNum = 0b1010;
static const int octNum = 0777;
static const double floatNum = 3.14;
static const double sciNum = 1.5e-3;
static const unsigned int suffixU = 100u;
static const unsigned long suffixUL = 200UL;
static const long long suffixLL = 300LL;
static const int zeroVal = 0;
static const int oneVal = 1;

/* ------------------------------------------------------------------ */
/* ControlFlowEnricher patterns                                         */
/* ------------------------------------------------------------------ */

void controlFlowPatterns(int a, int b, int c, int d) {
    // Simple condition — single test
    if (a > 0) {
        (void)a;
    }

    // Complex condition — multiple tests + mixed logic
    if (a > 0 && b < 10 || c == 5) {
        (void)b;
    }

    // Deeply nested parens
    if (((a > 0) && (b < 10)) || ((c == 5) && (d != 0))) {
        (void)c;
    }

    // Assignment in condition
    int x;
    if ((x = a + b) > 0) {
        (void)x;
    }

    // Switch with default
    switch (a) {
        case 0: break;
        case 1: break;
        default: break;
    }

    // Switch without default
    switch (b) {
        case 0: break;
        case 1: break;
    }

    // While loop
    while (a > 0 && b != 0) {
        a--;
    }

    // For loop
    for (int i = 0; i < 10; i++) {
        (void)i;
    }

    // Do-while
    do {
        a++;
    } while (a < 100);
}

/* Skeleton regression tests — operators must appear between leaf letters,
   overflow beyond 26 unique terms must use uppercase labels, and very
   long skeletons must be truncated.                                     */

void skeletonOperatorsPreserved(int a, int b, int c, int d, int e) {
    // Condition: a > b && c < d || e != a
    // Expected skeleton:  a>b&&c<d||e!=a  (no adjacent letters)
    if (a > b && c < d || e != a) {
        (void)a;
    }
}

void skeletonBitwiseOperators(int flags, int mask1, int mask2) {
    // Bitwise operators in switch condition: flags & mask1 | mask2
    // Expected skeleton:  a&b|c   (operators must appear between letters)
    switch (flags & mask1 | mask2) {
        case 0: break;
        default: break;
    }
}

void skeletonConditionalExpr(int a, int b, int c) {
    // Ternary expression in condition:  (a > b) ? c : a
    // The ? and : are operator tokens that should not be silently dropped.
    if ((a > b) ? c : a) {
        (void)a;
    }
}

void skeletonManyUniqueTerms(int v01,int v02,int v03,int v04,int v05,
                             int v06,int v07,int v08,int v09,int v10,
                             int v11,int v12,int v13,int v14,int v15,
                             int v16,int v17,int v18,int v19,int v20,
                             int v21,int v22,int v23,int v24,int v25,
                             int v26,int v27,int v28) {
    // 28 unique identifiers — exceeds a-z, must use A/B for overflow
    if (v01==v02 && v03==v04 && v05==v06 && v07==v08 && v09==v10 &&
        v11==v12 && v13==v14 && v15==v16 && v17==v18 && v19==v20 &&
        v21==v22 && v23==v24 && v25==v26 && v27==v28) {
        (void)v01;
    }
}

/* ------------------------------------------------------------------ */
/* OperatorEnricher patterns                                            */
/* ------------------------------------------------------------------ */

void operatorPatterns(int val) {
    // Prefix increment/decrement
    ++val;
    --val;

    // Postfix increment/decrement
    val++;
    val--;

    // Compound assignments
    val += 10;
    val -= 5;
    val *= 2;
    val /= 3;
    val %= 7;
    val &= 0xFF;
    val |= 0x01;
    val ^= 0x0F;

    // Shift expressions
    int shifted = val << 4;
    int rshifted = val >> 2;
    (void)shifted;
    (void)rshifted;
}

/* ------------------------------------------------------------------ */
/* MetricsEnricher patterns                                             */
/* ------------------------------------------------------------------ */

inline void inlineFunc(void) {}

static void staticFunc(void) {}

void manyParams(int a, int b, int c, int d, int e) {
    (void)a; (void)b; (void)c; (void)d; (void)e;
}

void multiReturn(int x) {
    if (x > 0) return;
    if (x < 0) return;
    return;
}

void withStrings(void) {
    (void)printf("hello");
    (void)printf("world");
    (void)printf("test");
}

struct SimpleStruct {
    int fieldA;
    int fieldB;
    int fieldC;
};

enum SimpleEnum {
    ENUM_A,
    ENUM_B,
    ENUM_C,
    ENUM_D,
};

class SimpleClass {
public:
    int publicField;
    void publicMethod(void) {}
    void declaredMethod(int arg);
private:
    int privateField;
protected:
    int protectedField;
};

volatile int volatileVar = 0;
const int constVar = 42;

/* ------------------------------------------------------------------ */
/* CastEnricher patterns                                                */
/* ------------------------------------------------------------------ */

void castPatterns(void* ptr) {
    int cStyleCast = (int)(long)ptr;
    int* reinterpreted = reinterpret_cast<int*>(ptr);
    const void* constCasted = const_cast<const void*>(ptr);
    (void)cStyleCast;
    (void)reinterpreted;
    (void)constCasted;
}

/* ------------------------------------------------------------------ */
/* RedundancyEnricher patterns                                          */
/* ------------------------------------------------------------------ */

int getValue(void);
int isReady(void);

void redundancyPatterns(int* ptr1, int* ptr2) {
    // Repeated null checks on different pointers
    if (ptr1 != nullptr) {
        *ptr1 = 1;
    }
    if (ptr2 != nullptr) {
        *ptr2 = 2;
    }

    // Repeated function call in conditions — getValue() called twice
    if (getValue() > 0) {
        (void)ptr1;
    }
    if (getValue() < 100) {
        (void)ptr2;
    }

    // Another repeated call — isReady() called twice
    if (isReady() && ptr1 != nullptr) {
        *ptr1 = getValue();
    }
    if (isReady() || ptr2 == nullptr) {
        (void)ptr2;
    }
}

/* Duplicate logic — repeated sub-expressions within one condition */
#define FLAG1 0x01
#define FLAG2 0x02
void dupLogicPatterns(int a, int b, int x, int *ptr) {
    // dup_logic=true: bitwise dup (the Bluetooth AOD_1US bug pattern)
    if (a & FLAG1 || a & FLAG1) {
        (void)a;
    }
    // dup_logic=true: comparison dup
    if (x > 0 && x > 0) {
        (void)x;
    }
    // dup_logic=true: dup in 3-way chain
    if (a == 1 || b == 2 || a == 1) {
        (void)a;
    }
    // dup_logic=true: null check dup
    if (ptr != nullptr && ptr != nullptr) {
        (void)ptr;
    }
    // dup_logic=false: different operands (control case)
    if (a > 0 && b < 10) {
        (void)a;
    }
    // dup_logic=false: same operator shape but different operands
    if (a > 0 || b > 0) {
        (void)b;
    }
    // dup_logic=false: ptr vs *ptr are distinct in skeleton
    if (ptr != nullptr && *ptr != 0) {
        (void)ptr;
    }
}

/* Duplicate conditions — same skeleton in two ifs */
void duplicateConditions(int a, int b) {
    if (a > 0 && b < 10) {
        (void)a;
    }
    // Same skeleton as above: a>b && c<d
    if (a > 0 && b < 10) {
        (void)b;
    }
}

/* ------------------------------------------------------------------ */
/* Phase 8 — Additional enrichment patterns                             */
/* ------------------------------------------------------------------ */

/* for_style: range-based for loop */
void rangeForLoop(int arr[], int len) {
    for (int x : arr) {
        (void)x;
    }
    (void)len;
}

/* throw_count: function with throw statements */
void throwingFunction(int x) {
    if (x < 0) throw "negative";
    if (x > 100) throw "too_large";
}

/* override / final: class hierarchy */
class Base {
public:
    virtual void overriddenMethod() {}
    virtual void finalMethod() {}
};

class Derived : public Base {
public:
    void overriddenMethod() override {}
    void finalMethod() final {}
};

/* ------------------------------------------------------------------ */
/* DeclDistanceEnricher patterns                                        */
/* ------------------------------------------------------------------ */

/* No locals → decl_distance=0, decl_far_count=0, has_unused_reassign=false */
void noLocals(int a) {
    (void)a;
}

/* All locals used immediately (distance < 2) → decl_distance=0 */
void allNearby(int a) {
    int x = a + 1;
    (void)x;
    int y = a + 2;
    (void)y;
}

/* One local declared far from first use → distance = 5 */
void oneFarDecl(void) {
    int farVar = 0;
    (void)printf("line1\n");
    (void)printf("line2\n");
    (void)printf("line3\n");
    (void)printf("line4\n");
    (void)printf("val=%d\n", farVar);
}

/* Two far locals → decl_distance = sum of both distances */
void twoFarDecls(void) {
    int alpha = 0;
    int beta = 0;
    (void)printf("spacer1\n");
    (void)printf("spacer2\n");
    (void)printf("spacer3\n");
    (void)printf("a=%d\n", alpha);
    (void)printf("b=%d\n", beta);
}

/* Dead store: variable written twice without read between */
void deadStorePattern(int a) {
    int x = a;
    x = a + 1;
    (void)x;
}

/* Compound assign is NOT a dead store (reads before writing) */
void compoundAssignNotDeadStore(int a) {
    int x = a;
    x += 1;
    (void)x;
}

/* Parameters excluded — only local counted */
void paramExcluded(int param) {
    int loc = 0;
    (void)printf("gap\n");
    (void)printf("gap\n");
    (void)printf("p=%d loc=%d\n", param, loc);
}

/* ------------------------------------------------------------------ */
/* EscapeEnricher patterns                                              */
/* ------------------------------------------------------------------ */

/* Tier 1: direct return &local — 100% dangling pointer */
int* escapeDirectAddr(void) {
    int x = 42;
    return &x;
}

/* Tier 2: return local array — array decay to dangling pointer */
int* escapeArrayDecay(void) {
    int arr[10];
    arr[0] = 1;
    return arr;
}

/* Tier 3: indirect alias — ptr = &local; return ptr */
int* escapeIndirectAlias(void) {
    int val = 7;
    int *p = &val;
    return p;
}

/* Safe: static local — address is stable across calls */
int* escapeStaticSafe(void) {
    static int s = 99;
    return &s;
}

/* Safe: no escape — returns address of parameter (not local stack) */
int* escapeNoEscapeParam(int *buf) {
    return buf;
}

/* Safe: no locals at all */
int escapeNoLocals(int a) {
    return a + 1;
}

/* Tier 1 inside ternary: return cond ? &local : nullptr */
int* escapeTernary(int flag) {
    int x = 10;
    return flag ? &x : nullptr;
}

/* ------------------------------------------------------------------ */
/* ShadowEnricher patterns                                              */
/* ------------------------------------------------------------------ */

/* Inner block redeclares outer variable */
void shadowBasic(int n) {
    int x = 1;
    if (n > 0) {
        int x = 2;  /* shadows outer x */
        (void)x;
    }
    (void)x;
}

/* For-loop variable shadows parameter */
void shadowForLoop(int i) {
    for (int i = 0; i < 10; i++) {  /* shadows param i */
        (void)i;
    }
}

/* Multiple shadows */
void shadowMultiple(void) {
    int a = 1;
    int b = 2;
    {
        int a = 10;  /* shadows a */
        int b = 20;  /* shadows b */
        (void)a;
        (void)b;
    }
    (void)a;
    (void)b;
}

/* No shadowing at all */
void shadowNone(int n) {
    int x = n + 1;
    if (x > 0) {
        int y = x + 2;
        (void)y;
    }
    (void)x;
}

/* Nested shadow: outer -> middle -> inner */
void shadowNested(void) {
    int val = 1;
    {
        int val = 2;  /* shadows outer val */
        {
            int val = 3;  /* shadows middle val */
            (void)val;
        }
        (void)val;
    }
    (void)val;
}

/* ------------------------------------------------------------------ */
/* UnusedParamEnricher patterns                                         */
/* ------------------------------------------------------------------ */

/* One unused parameter */
int unusedParamOne(int used, int unused_p) {
    return used + 1;
}

/* All parameters used */
int unusedParamNone(int a, int b) {
    return a + b;
}

/* All parameters unused */
void unusedParamAll(int x, int y, int z) {
    (void)0;
}

/* No parameters at all */
int unusedParamEmpty(void) {
    return 42;
}

/* ------------------------------------------------------------------ */
/* FallthroughEnricher patterns                                         */
/* ------------------------------------------------------------------ */

/* One case falls through (case 1 has no break) */
void fallthroughOne(int x) {
    switch (x) {
        case 1:
            x++;
            /* fallthrough — no break */
        case 2:
            x--;
            break;
        default:
            break;
    }
}

/* No fallthrough — all cases properly terminated */
void fallthroughNone(int x) {
    switch (x) {
        case 1:
            x++;
            break;
        case 2:
            x--;
            return;
        default:
            break;
    }
}

/* Empty case grouping (intentional, not flagged) + a real fallthrough */
void fallthroughGrouped(int x) {
    switch (x) {
        case 1:
        case 2:
            /* intentional grouping — empty cases are not flagged */
            x++;
            /* but THIS case falls through to case 3 */
        case 3:
            x--;
            break;
        default:
            break;
    }
}

/* No switch at all */
void fallthroughNoSwitch(int x) {
    if (x > 0) x++;
}

/* ------------------------------------------------------------------ */
/* RecursionEnricher patterns                                           */
/* ------------------------------------------------------------------ */

/* Direct recursion: factorial */
int recursiveFactorial(int n) {
    if (n <= 1) return 1;
    return n * recursiveFactorial(n - 1);
}

/* Multiple self-calls */
int recursiveFib(int n) {
    if (n <= 1) return n;
    return recursiveFib(n - 1) + recursiveFib(n - 2);
}

/* Not recursive */
int notRecursive(int n) {
    return n * 2;
}

/* Calls another function, not itself */
int callsOther(int n) {
    return notRecursive(n) + 1;
}

// ── TodoEnricher fixtures ────────────────────────────────────────────

/* Function with a single TODO */
int todoSingle(int x) {
    // TODO: refactor this later
    return x + 1;
}

/* Function with multiple different markers */
int todoMultiple(int x) {
    // TODO: first thing
    // FIXME: broken path
    /* HACK: temporary workaround */
    return x;
}

/* Function with no markers at all */
int todoNone(int x) {
    // just a normal comment
    return x * 2;
}

/* Function with repeated same marker */
int todoRepeated(int x) {
    // TODO: item A
    // TODO: item B
    // XXX: watch out
    return x;
}

// ── False-positive regression fixtures ───────────────────────────────

/* Comparisons only — should NOT have has_assignment_in_condition */
int noAssignCompare(int addr, int size) {
    if ((addr < 0 || addr >= 100) || ((100 - addr) < size)) {
        return -1;
    }
    if (addr <= 50 && addr != 0) {
        return 1;
    }
    return 0;
}
