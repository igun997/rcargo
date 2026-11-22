//! Replays every recorded call in `golden/cases.jsonl` against the port and compares with what the
//! original answered. Case format: {"args":[...],"expect":{"ok":V}|{"err":"msg"},"ns":ORIGINAL_NANOS}
//! Numbers compare equal within 1e-9 (relative); an error case needs an error, not the same text.

use rcport::bridge::handle_line;
use serde_json::{json, Value};
use std::time::Instant;

fn close(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_f64() || y.is_f64() {
                let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
                (x - y).abs() <= 1e-9 * 1.0_f64.max(x.abs()).max(y.abs())
            } else {
                x == y
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| close(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| close(v, w)))
        }
        _ => a == b,
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 160 {
        format!("{}...", s.chars().take(160).collect::<String>())
    } else {
        s
    }
}

#[test]
fn golden_cases_match_the_original() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/cases.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let cases: Vec<Value> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("golden/cases.jsonl line {}: {e}", i + 1))
        })
        .collect();
    assert!(
        !cases.is_empty(),
        "golden/cases.jsonl has no cases: record real calls of the original first (see TASK.md)"
    );

    let (mut mismatches, mut error_cases) = (0usize, 0usize);
    let mut shown: Vec<String> = vec![];
    let (mut orig_ns, mut rust_ns) = (0u128, 0u128);
    for (i, c) in cases.iter().enumerate() {
        orig_ns += c["ns"].as_u64().unwrap_or(0) as u128;
        let expect = &c["expect"];
        let want_err = expect.get("err").is_some();
        error_cases += want_err as usize;
        let request = json!({"args": c["args"]}).to_string();
        let t = Instant::now();
        let got: Value = serde_json::from_str(&handle_line(&request)).expect("response is JSON");
        rust_ns += t.elapsed().as_nanos();
        let ok = if let Some(want) = expect.get("ok") {
            got.get("ok").is_some_and(|g| close(want, g))
        } else {
            want_err && got.get("err").is_some()
        };
        if !ok {
            mismatches += 1;
            if shown.len() < 5 {
                shown.push(format!(
                    "  case {}: args {}\n    original: {}\n    port:     {}",
                    i + 1,
                    short(&c["args"]),
                    short(expect),
                    short(&got)
                ));
            }
        }
    }

    let n = cases.len();
    let speed = if orig_ns > 0 && rust_ns > 0 {
        format!(
            "; port {:.2}ms vs original {:.2}ms = {:.1}x per call (port timing includes JSON in/out; the host bridge is not included, run the shim's bench for end to end)",
            rust_ns as f64 / 1e6,
            orig_ns as f64 / 1e6,
            orig_ns as f64 / rust_ns as f64
        )
    } else {
        String::new()
    };
    println!(
        "rcargo-port: {}/{} cases match ({} are error cases){}",
        n - mismatches,
        n,
        error_cases,
        speed
    );
    assert!(
        mismatches == 0,
        "rcargo-port: {mismatches} of {n} cases differ from the original; first ones:\n{}",
        shown.join("\n")
    );
}
