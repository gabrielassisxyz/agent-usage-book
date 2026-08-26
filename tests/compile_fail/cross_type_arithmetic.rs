// Compile-fail canary: demonstrates the invariant the domain newtypes will enforce,
// that quantities are distinct types and arithmetic between them is closed. Adding a
// token count to a credit count must not compile. The domain beads replace this
// placeholder with cases against the real types; until then it proves the harness
// catches a real, specific compile error rather than a typo.

struct Tokens(u64);
struct Credits(u64);

fn main() {
    let tokens = Tokens(10);
    let credits = Credits(5);
    let _sum = tokens + credits;
}
