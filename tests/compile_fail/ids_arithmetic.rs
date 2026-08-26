// Adding two session identifiers must not compile: an identifier is not a
// quantity, and the absence of an Add implementation is what makes it a
// compile error.

use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};

fn main() {
    let a = SessionId::new(SourceNamespace::new("x"), NativeSessionId::new("1"));
    let b = SessionId::new(SourceNamespace::new("x"), NativeSessionId::new("2"));
    let _sum = a + b;
}
