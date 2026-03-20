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
