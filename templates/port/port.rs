//! The Rust port of `{{FUNCTION}}` ({{LANG}}). This is the only file you edit.
//!
//! Match what the original DOES on the recorded calls in `golden/cases.jsonl`, not how it is written.
//! Watch for: byte vs character indexing (Rust strings are UTF-8 bytes), integer overflow and
//! division/modulo of negatives, float formatting, unstable ordering of maps and sorts, and
//! exceptions that the original raises (return `Err` there).

use serde_json::Value;

/// `args` are the positional arguments of the original call, as JSON values.
/// Return the result as JSON, or `Err(message)` exactly where the original raises or throws.
pub fn call(args: &[Value]) -> Result<Value, String> {
    let _ = args;
    Err("not ported yet: implement `call` in src/port.rs".to_string())
}
