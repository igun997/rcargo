//! Compact, token-cheap rendering of a cargo run for AI agents: diagnostics first,
//! failing tests next, then bounded tails. Progress noise is dropped.

use serde_json::Value;
use std::path::PathBuf;

use crate::run::Stream;

fn subcommand_index(args: &[String]) -> Option<usize> {
    args.iter().position(|a| !a.starts_with('+'))
}

/// Cargo subcommands that accept `--message-format` and compile things.
pub fn supports_message_format(args: &[String]) -> bool {
    subcommand_index(args)
        .map(|i| {
            matches!(
                args[i].as_str(),
                "build" | "b" | "test" | "t" | "bench" | "check" | "c" | "clippy" | "rustc"
            )
        })
        .unwrap_or(false)
}

pub fn produces_artifacts(args: &[String]) -> bool {
    supports_message_format(args)
}

/// Insert `--message-format=<fmt>` after the subcommand unless the caller already chose one.
/// Returns (args, injected).
pub fn inject_message_format(mut args: Vec<String>, fmt: &str) -> (Vec<String>, bool) {
    if !supports_message_format(&args) {
        return (args, false);
    }
    let before_dd = args.iter().take_while(|a| a.as_str() != "--");
    if before_dd
        .into_iter()
        .any(|a| a.starts_with("--message-format"))
    {
        return (args, false);
    }
    let i = subcommand_index(&args).unwrap_or(0);
    args.insert(i + 1, format!("--message-format={fmt}"));
    (args, true)
}

/// The built executable named by a `compiler-artifact` JSON line, if any.
pub fn artifact_of_line(line: &[u8]) -> Option<PathBuf> {
    if !line.starts_with(b"{\"reason\":\"compiler-artifact\"") {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    v["executable"].as_str().map(PathBuf::from)
}

#[derive(Default)]
pub struct Capture {
    pub errors: usize,
    pub warnings: usize,
    diags: Vec<(bool, String)>, // (is_error, rendered)
    seen: std::collections::HashSet<String>,
    out: Vec<String>,
    err: Vec<String>,
    pub build_success: Option<bool>,
    pub artifacts: Vec<PathBuf>,
}

fn is_progress(line: &str) -> bool {
    let t = line.trim_start();
    [
        "Compiling ",
        "Checking ",
        "Fresh ",
        "Downloading ",
        "Downloaded ",
        "Updating ",
        "Locking ",
        "Adding ",
        "Blocking ",
        "Documenting ",
        "Running ",
        "Packaging ",
        "Verifying ",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

impl Capture {
    pub fn feed(&mut self, stream: Stream, raw: &[u8]) {
        let line = String::from_utf8_lossy(raw)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        match stream {
            Stream::Err => self.err.push(line),
            Stream::Out => {
                if line.starts_with('{') && line.contains("\"reason\"") {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        self.json(&v);
                        return;
                    }
                }
                self.out.push(line);
            }
        }
    }

    fn json(&mut self, v: &Value) {
        match v["reason"].as_str() {
            Some("compiler-message") => {
                let m = &v["message"];
                let level = m["level"].as_str().unwrap_or("");
                let is_err = matches!(level, "error" | "error: internal compiler error");
                if !is_err && level != "warning" {
                    return;
                }
                let Some(r) = m["rendered"].as_str() else {
                    return;
                };
                if !self.seen.insert(r.to_string()) {
                    return;
                }
                if is_err {
                    self.errors += 1;
                } else {
                    self.warnings += 1;
                }
                self.diags.push((is_err, r.trim_end().to_string()));
            }
            Some("build-finished") => self.build_success = v["success"].as_bool(),
            Some("compiler-artifact") => {
                if let Some(e) = v["executable"].as_str() {
                    self.artifacts.push(PathBuf::from(e));
                }
            }
            _ => {}
        }
    }

    pub fn render(&self, code: i32, max_chars: usize) -> String {
        let mut s = String::new();
        let budget = |frac: f64| ((max_chars as f64) * frac) as usize;

        if !self.diags.is_empty() {
            s += &format!(
                "== diagnostics: {} error(s), {} warning(s) ==\n",
                self.errors, self.warnings
            );
            let mut used = 0;
            let mut omitted = 0;
            let ordered = self
                .diags
                .iter()
                .filter(|d| d.0)
                .chain(self.diags.iter().filter(|d| !d.0));
            for (_, text) in ordered {
                if used + text.len() > budget(0.6) && used > 0 {
                    omitted += 1;
                    continue;
                }
                used += text.len();
                s += text;
                s += "\n\n";
            }
            if omitted > 0 {
                s += &format!("(+{omitted} more diagnostics omitted; fix the above first)\n\n");
            }
        }

        let blocks = failure_blocks(&self.out);
        let results: Vec<&String> = self
            .out
            .iter()
            .chain(self.err.iter())
            .filter(|l| {
                l.starts_with("test result:") || (l.starts_with("test ") && l.ends_with("FAILED"))
            })
            .collect();
        if !blocks.is_empty() || !results.is_empty() {
            s += "== tests ==\n";
            let mut used = 0;
            for b in &blocks {
                if used + b.len() > budget(0.3) {
                    s += "(more failure output omitted)\n";
                    break;
                }
                used += b.len();
                s += b;
                s += "\n";
            }
            for r in results {
                s += r;
                s += "\n";
            }
            s += "\n";
        }

        let have_tests = s.contains("== tests ==");
        let out_tail: Vec<&String> = self.out.iter().collect();
        if !out_tail.is_empty() && blocks.is_empty() && (!have_tests || code != 0) {
            s += "== stdout (tail) ==\n";
            s += &tail(&out_tail, budget(0.25));
            s += "\n";
        }

        let hidden = self.err.iter().filter(|l| is_progress(l)).count();
        let err_lines: Vec<&String> = self.err.iter().filter(|l| !is_progress(l)).collect();
        let diag_text_in_stderr = !self.diags.is_empty();
        let err_lines: Vec<&String> = if diag_text_in_stderr {
            // with JSON diagnostics cargo's stderr only repeats a summary; keep just the tail
            err_lines.into_iter().rev().take(8).rev().collect()
        } else {
            err_lines
        };
        if !err_lines.is_empty() {
            s += "== stderr (tail) ==\n";
            s += &tail(&err_lines, budget(0.25));
            s += "\n";
        }
        if hidden > 0 {
            s += &format!("({hidden} progress lines hidden)\n");
        }
        s.trim_end().to_string()
    }
}

fn failure_blocks(out: &[String]) -> Vec<String> {
    let mut blocks = vec![];
    let mut cur: Option<Vec<&str>> = None;
    for l in out {
        let starts = l.starts_with("---- ") && l.ends_with(" ----");
        if starts || l == "failures:" {
            if let Some(c) = cur.take() {
                blocks.push(c.join("\n"));
            }
            if starts {
                cur = Some(vec![l.as_str()]);
            }
        } else if let Some(c) = cur.as_mut() {
            if c.len() < 40 {
                c.push(l.as_str());
            }
        }
    }
    if let Some(c) = cur {
        blocks.push(c.join("\n"));
    }
    blocks
}

fn tail(lines: &[&String], max: usize) -> String {
    let mut kept: Vec<&str> = vec![];
    let mut used = 0;
    for l in lines.iter().rev() {
        if used + l.len() + 1 > max && !kept.is_empty() {
            break;
        }
        used += l.len() + 1;
        kept.push(l);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    let mut s = String::new();
    if omitted > 0 {
        s += &format!("... ({omitted} earlier lines omitted)\n");
    }
    s += &kept.join("\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(level: &str, rendered: &str) -> String {
        serde_json::json!({"reason":"compiler-message","message":{"level":level,"rendered":rendered}}).to_string()
    }

    #[test]
    fn message_format_injection() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (r, inj) =
            inject_message_format(a(&["test", "--release", "--", "--nocapture"]), "json");
        assert!(inj);
        assert_eq!(
            r,
            a(&[
                "test",
                "--message-format=json",
                "--release",
                "--",
                "--nocapture"
            ])
        );
        let (r, inj) = inject_message_format(a(&["+nightly", "build"]), "json");
        assert!(inj);
        assert_eq!(r, a(&["+nightly", "build", "--message-format=json"]));
        assert!(!inject_message_format(a(&["build", "--message-format=short"]), "json").1);
        assert!(!inject_message_format(a(&["fmt"]), "json").1);
        assert!(!inject_message_format(a(&["run"]), "json").1);
        // a flag that only appears after `--` belongs to the program, not cargo
        assert!(inject_message_format(a(&["test", "--", "--message-format=x"]), "json").1);
    }

    #[test]
    fn artifacts_from_json() {
        let l = br#"{"reason":"compiler-artifact","package_id":"x","executable":"/home/u/p/target/debug/p","fresh":false}"#;
        assert_eq!(
            artifact_of_line(l),
            Some(PathBuf::from("/home/u/p/target/debug/p"))
        );
        assert_eq!(
            artifact_of_line(br#"{"reason":"compiler-artifact","executable":null}"#),
            None
        );
        assert_eq!(artifact_of_line(b"plain"), None);
        let mut c = Capture::default();
        c.feed(Stream::Out, l);
        assert_eq!(c.artifacts.len(), 1);
    }

    #[test]
    fn diagnostics_errors_first_and_deduped() {
        let mut c = Capture::default();
        c.feed(
            Stream::Out,
            format!("{}\n", msg("warning", "warning: unused x\n")).as_bytes(),
        );
        c.feed(
            Stream::Out,
            format!(
                "{}\n",
                msg("error", "error[E0425]: cannot find value `y`\n")
            )
            .as_bytes(),
        );
        c.feed(
            Stream::Out,
            format!(
                "{}\n",
                msg("error", "error[E0425]: cannot find value `y`\n")
            )
            .as_bytes(),
        );
        c.feed(
            Stream::Out,
            b"{\"reason\":\"build-finished\",\"success\":false}\n",
        );
        c.feed(Stream::Err, b"   Compiling foo v0.1.0\n");
        c.feed(Stream::Err, b"error: could not compile `foo`\n");
        assert_eq!((c.errors, c.warnings), (1, 1));
        assert_eq!(c.build_success, Some(false));
        let r = c.render(101, 10_000);
        let e = r.find("E0425").unwrap();
        let w = r.find("unused x").unwrap();
        assert!(e < w, "errors before warnings:\n{r}");
        assert!(r.contains("1 error(s), 1 warning(s)"));
        assert!(r.contains("(1 progress lines hidden)"));
        assert!(!r.contains("Compiling foo"));
    }

    #[test]
    fn test_failures_extracted() {
        let mut c = Capture::default();
        for l in [
            "running 2 tests",
            "test a ... ok",
            "test b ... FAILED",
            "",
            "failures:",
            "",
            "---- b stdout ----",
            "thread 'b' panicked at src/lib.rs:3:5:",
            "assertion `left == right` failed",
            "",
            "failures:",
            "    b",
            "",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored",
        ] {
            c.feed(Stream::Out, format!("{l}\n").as_bytes());
        }
        let r = c.render(101, 10_000);
        assert!(r.contains("---- b stdout ----"));
        assert!(r.contains("assertion `left == right` failed"));
        assert!(r.contains("test result: FAILED. 1 passed; 1 failed"));
        assert!(
            !r.contains("running 2 tests"),
            "noise stays out when failures are shown:\n{r}"
        );
    }

    #[test]
    fn budget_truncates() {
        let mut c = Capture::default();
        for i in 0..50 {
            c.feed(
                Stream::Out,
                format!(
                    "{}\n",
                    msg("error", &format!("error: number {i} {}", "x".repeat(200)))
                )
                .as_bytes(),
            );
        }
        let r = c.render(1, 2_000);
        assert!(r.contains("more diagnostics omitted"));
        assert!(r.len() < 4_000, "len {}", r.len());
    }

    #[test]
    fn plain_command_output_tail() {
        let mut c = Capture::default();
        for i in 0..1000 {
            c.feed(Stream::Out, format!("line {i}\n").as_bytes());
        }
        let r = c.render(0, 400);
        assert!(r.contains("line 999"));
        assert!(!r.contains("line 0\n"));
        assert!(r.contains("earlier lines omitted"));
    }
}
