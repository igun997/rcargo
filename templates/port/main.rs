//! JSON-lines server: reads one request per line on stdin, answers one line on stdout.
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if writeln!(out, "{}", rcport::bridge::handle_line(&line)).is_err() || out.flush().is_err() {
            break;
        }
    }
}
