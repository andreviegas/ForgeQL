fn foo() {
    return;
}

struct Motor {
    speed: f64,
}

enum State {
    Idle,
    Running,
    Stopped,
}

/// Documented function
fn bar(x: i32) -> i32 {
    if x > 0 {
        return x;
    }
    return 0;
}

let count: i32 = 42;

/// Recursive factorial
fn factorial(n: i32) -> i32 {
    if n <= 1 { return 1; }
    return n * factorial(n - 1);
}

fn process(data: i32, unused: i32) {
    // TODO: optimize this later
    // FIXME: handle edge cases
    let result = data + 1;
}

fn helper(a: i32, b: i32, c: i32) -> i32 {
    let mut sum = a + b;
    for i in 0..c {
        if i > a {
            while sum > 0 {
                sum -= 1;
            }
        }
    }
    return sum;
}

let hex_value: i32 = 0xFF;
let bin_value: i32 = 0b1010;
let pi: f64 = 3.14159;

fn transform(x: i32) -> i32 {
    let mut y = x;
    y += 10;
    y = y << 2;
    let z = y as i32;
    return z;
}

fn checker(a: i32) -> i32 {
    if a > 0 && a < 100 {
        if a > 0 && a < 100 {
            return 1;
        }
    }
    return 0;
}

fn shadowed() {
    let x = 1;
    {
        let x = 2;
    }
}

fn escaping() -> i32 {
    let local = 5;
    let ptr = &local;
    return *ptr;
}

fn switcher(code: i32) -> i32 {
    match code {
        1 => return 10,
        2 |
        3 => return 20,
        _ => return 0,
    }
}

fn distant() -> i32 {
    let early = 1;
    let a = 2;
    let b = 3;
    let c = 4;
    let d = 5;
    return early + d;
}

fn caller(n: i32) -> i32 {
    let a = bar(n);
    let b = factorial(a);
    return a + b;
}

fn noop() {
}

const MAGIC: i32 = 0xCAFE;

fn no_default(code: i32) -> i32 {
    match code {
        1 => 10,
        2 => 20,
        3 => 30,
        _ => -1,
    }
}

fn deeply_nested(a: i32) -> i32 {
    if a > 0 {
        if a > 10 {
            if a > 100 {
                if a > 1000 {
                    return a;
                }
            }
        }
    }
    return 0;
}

fn many_params(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    return a + b + c + d + e + f;
}
