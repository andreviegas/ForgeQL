void foo() {
    return;
}

struct Motor {
    double speed;
};

enum State {
    Idle,
    Running,
    Stopped
};

/// Documented function
int bar(int x) {
    if (x > 0) {
        return x;
    }
    return 0;
}

int count = 42;

/// Recursive factorial
int factorial(int n) {
    if (n <= 1) return 1;
    return n * factorial(n - 1);
}

void process(int data, int unused) {
    // TODO: optimize this later
    // FIXME: handle edge cases
    int result = data + 1;
}

static int helper(int a, int b, int c) {
    int sum = a + b;
    for (int i = 0; i < c; i++) {
        if (i > a) {
            while (sum > 0) {
                sum--;
            }
        }
    }
    return sum;
}

int hex_value = 0xFF;
int bin_value = 0b1010;
double pi = 3.14159;

int transform(int x) {
    int y = x;
    y += 10;
    y = y << 2;
    int z = (int)y;
    return z;
}

int checker(int a) {
    if (a > 0 && a < 100) {
        if (a > 0 && a < 100) {
            return 1;
        }
    }
    return 0;
}

void shadowed() {
    int x = 1;
    {
        int x = 2;
    }
}

int escaping() {
    int local = 5;
    int* ptr = &local;
    return *ptr;
}

int switcher(int code) {
    switch (code) {
        case 1: return 10;
        case 2:
        case 3: return 20;
        default: return 0;
    }
}

int distant() {
    int early = 1;
    int a = 2;
    int b = 3;
    int c = 4;
    int d = 5;
    return early + d;
}

int caller(int n) {
    int a = bar(n);
    int b = factorial(a);
    return a + b;
}

void noop() {
}

const int MAGIC = 0xCAFE;

int no_default(int code) {
    switch (code) {
        case 1: return 10;
        case 2: return 20;
        case 3: return 30;
    }
    return -1;
}

int deeply_nested(int a) {
    if (a > 0) {
        if (a > 10) {
            if (a > 100) {
                if (a > 1000) {
                    return a;
                }
            }
        }
    }
    return 0;
}

int many_params(int a, int b, int c, int d, int e, int f) {
    return a + b + c + d + e + f;
    return a + b + c + d + e + f;
}

// ── parity-test fixtures (do not remove) ─────────────────────────────────────

// resolve_type_prefers_type_over_function: same bare name as 'Motor' struct (line 5).
// C/C++ allows a struct tag and a free function to share the same identifier.
int Motor(int rpm) { return rpm * 2; }

// resolve_body_follows_body_symbol_redirect: C++ out-of-line member definition.
// MemberEnricher stores body_symbol="Engine::start" on the in-class declaration.
class Engine {
public:
    void start();
};
void Engine::start() { /* out-of-line */ }

// resolve_symbol_deterministic_on_duplicates: forward-declare then define.
// Both rows are indexed under name 'noop_dup'; last-write-wins must be stable.
void noop_dup();
void noop_dup() {}
