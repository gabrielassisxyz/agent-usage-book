// Compile-fail: a Secret must not be printable. The whole point of the wrapper
// is that a credential can be used on purpose but never printed by accident;
// Debug and Display are the accidental paths, and both must fail to resolve.

use agent_usage_book::auth::Secret;

fn secret() -> Secret<String> {
    unreachable!()
}

fn main() {
    println!("{:?}", secret());
    println!("{}", secret());
}
