// Fixture for the stamp-only boolean defaults.
//
// has_todo, has_escape, has_shadow and is_recursive are written only when they
// hold. The other value is not stored anywhere, so it is answered from the
// declaration in the field-tier table: the rows the enricher examined, minus
// the rows it wrote.
//
// Four functions and one struct, and the struct is named to sort LAST. It is
// the load-bearing part twice over: a row of a kind no function enricher
// examines must answer NEITHER value, and — because "" sorts before "false" —
// its position under ORDER BY is the one place the resolver's effect on sorting
// is visible at all. With a two-valued field the unstamped rows sort where the
// valueless ones used to, so a fixture of functions alone could not tell the
// two readings apart.

struct zNotAFunction {
    int member;
};

int plainFunction(int x)
{
    return x + 1;
}

int functionWithTodo(int x)
{
    // TODO: this one is stamped, so it is not a default
    return x + 2;
}

int recursiveFunction(int x)
{
    if (x <= 0) {
        return 0;
    }
    return recursiveFunction(x - 1);
}

int shadowingFunction(int x)
{
    int value = x;
    {
        int value = 2;
        x += value;
    }
    return x + value;
}
