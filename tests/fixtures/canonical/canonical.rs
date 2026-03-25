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
