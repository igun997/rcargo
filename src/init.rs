//! `rcargo init` / `rcargo integrate` / `rcargo hook`: one-command setup for people (and agents)
//! who just want their laptop to stay cool.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{config, guidance, project};

const SKILL_MD: &str = include_str!("../skills/rcargo/SKILL.md");

const PROJECT_TOML: &str = r#"# rcargo project settings. Remote machines ("instances") are configured per user:
#   rcargo instances            # list them
#   rcargo instances add ...    # add one
#
# Pin this project to one instance (otherwise your default is used):
# instance = "main"

[sync]
# Extra things that must never be uploaded (gitignore syntax). .gitignore is already honoured.
exclude = []

[remote]
# Environment variables copied from YOUR shell to the remote at run time, never written to disk.
# Needed for private git dependencies or private registries, e.g. ["GH_TOKEN"].
forward_env = []
"#;

#[derive(Args, Default)]
pub struct IntegrateArgs {
    /// Install the agent skill (Claude Code / Codex / Cursor style SKILL.md).
    #[arg(long)]
    pub skill: bool,
    /// Register the MCP server (.mcp.json, .cursor/mcp.json).
    #[arg(long)]
    pub mcp: bool,
    /// Make rust-analyzer check on the remote (.vscode/settings.json).
    #[arg(long)]
    pub vscode: bool,
    /// Block local `cargo build/test/...` in Claude Code and point to rcargo instead.
    #[arg(long)]
    pub guard: bool,
    /// Everything above (default when no flag is given).
    #[arg(long)]
    pub all: bool,
    /// Install the skill for your user (~/.claude, ~/.agents) instead of this project.
    #[arg(long)]
    pub global: bool,
}

fn base_dir(global: bool) -> Result<PathBuf> {
    if global {
        return Ok(crate::util::home());
    }
    let cwd = std::env::current_dir()?;
    Ok(project::discover(&cwd).map(|p| p.root).unwrap_or(cwd))
}

pub fn init() -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let proj = project::discover(&cwd).map_err(|_| {
        anyhow::anyhow!(
            "run `rcargo init` inside your Rust project folder (the one with Cargo.toml)"
        )
    })?;
    let toml_path = proj.root.join("rcargo.toml");
    if toml_path.exists() {
        println!("  = rcargo.toml already exists, left as is");
    } else {
        std::fs::write(&toml_path, PROJECT_TOML)?;
        println!("  + created rcargo.toml");
    }
    integrate_in(
        &proj.root,
        &IntegrateArgs {
            all: true,
            ..Default::default()
        },
    )?;

    println!("\nChecking what is still needed...\n");
    let loaded = config::load(&cwd)?;
    let rep = guidance::onboard(&loaded, None, false);
    print!("{}", rep.render());
    if rep.ready() {
        println!(
            "\nTry it:  rcargo test      (it builds on the remote machine; your laptop stays idle)"
        );
    }
    Ok(0)
}

pub fn integrate(a: IntegrateArgs) -> Result<i32> {
    let base = base_dir(a.global)?;
    integrate_in(&base, &a)?;
    Ok(0)
}

fn integrate_in(base: &Path, a: &IntegrateArgs) -> Result<()> {
    let everything = a.all || !(a.skill || a.mcp || a.vscode || a.guard);
    let global = a.global;
    if everything || a.skill {
        for d in [".claude/skills/rcargo", ".agents/skills/rcargo"] {
            let dir = base.join(d);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
            println!("  + agent skill: {}", dir.join("SKILL.md").display());
        }
    }
    if global {
        println!("(--global installs the skill only; run `rcargo integrate` inside a project for MCP/editor/guard)");
        return Ok(());
    }
    if everything || a.mcp {
        let entry = json!({"command": "rcargo", "args": ["mcp"]});
        merge_json(&base.join(".mcp.json"), |v| {
            v["mcpServers"]["rcargo"] = entry.clone();
        })?;
        println!("  + MCP server registered in .mcp.json (Claude Code)");
        if base.join(".cursor").is_dir() {
            merge_json(&base.join(".cursor/mcp.json"), |v| {
                v["mcpServers"]["rcargo"] = entry.clone();
            })?;
            println!("  + MCP server registered in .cursor/mcp.json (Cursor)");
        }
        println!("    other agents: `claude mcp add rcargo -- rcargo mcp`   `codex mcp add rcargo -- rcargo mcp`");
    }
    if everything || a.vscode {
        merge_json(&base.join(".vscode/settings.json"), |v| {
            v["rust-analyzer.check.overrideCommand"] = json!([
                "rcargo",
                "-q",
                "check",
                "--workspace",
                "--all-targets",
                "--message-format=json"
            ]);
        })?;
        println!("  + rust-analyzer will run its checks on the remote (.vscode/settings.json)");
    }
    if everything || a.guard {
        merge_json(&base.join(".claude/settings.json"), |v| {
            let hooks = &mut v["hooks"]["PreToolUse"];
            if !hooks.is_array() {
                *hooks = json!([]);
            }
            let arr = hooks.as_array_mut().unwrap();
            let present = arr.iter().any(|e| e.to_string().contains("rcargo hook"));
            if !present {
                arr.push(json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "rcargo hook"}]}));
            }
        })?;
        println!("  + guard installed: Claude Code can no longer compile on this computer by accident (.claude/settings.json)");
    }
    Ok(())
}

/// Read-modify-write a JSON file, creating it if needed. Refuses to touch files that are not plain JSON.
fn merge_json(path: &Path, f: impl FnOnce(&mut Value)) -> Result<()> {
    let mut v: Value = if path.is_file() {
        let s = std::fs::read_to_string(path)?;
        if s.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&s).with_context(|| {
                format!(
                    "{} is not plain JSON (comments?); add the rcargo entry by hand, see README",
                    path.display()
                )
            })?
        }
    } else {
        json!({})
    };
    if !v.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    f(&mut v);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(())
}

// ------------------------------------------------------------------ guard hook

const HEAVY: &[&str] = &[
    "build",
    "b",
    "check",
    "c",
    "test",
    "t",
    "run",
    "r",
    "clippy",
    "bench",
    "doc",
    "rustc",
    "nextest",
    "install",
    "miri",
    "tarpaulin",
    "llvm-cov",
    "fuzz",
    "udeps",
    "audit-build",
];

/// Does this shell command line start a local cargo compile? Returns the rcargo equivalent.
pub fn local_cargo_invocation(cmdline: &str) -> Option<String> {
    if cmdline.contains("RCARGO_ALLOW_LOCAL=") {
        return None;
    }
    let seps: &[char] = &[';', '&', '|', '\n', '(', ')', '`'];
    for seg in cmdline.split(seps) {
        let mut toks: Vec<&str> = seg.split_whitespace().collect();
        while let Some(t) = toks.first() {
            let is_assign = t.contains('=')
                && !t.starts_with('-')
                && t.split('=').next().is_some_and(crate::util::valid_env_key);
            if is_assign || matches!(*t, "sudo" | "time" | "nice" | "env" | "command" | "exec") {
                toks.remove(0);
            } else {
                break;
            }
        }
        let Some(first) = toks.first() else { continue };
        let name = first.rsplit('/').next().unwrap_or(first);
        if name != "cargo" {
            continue;
        }
        let rest: Vec<&str> = toks[1..]
            .iter()
            .copied()
            .filter(|t| !t.starts_with('+'))
            .collect();
        let Some(sub) = rest.first() else { continue };
        if HEAVY.contains(sub) {
            return Some(format!("rcargo {}", toks[1..].join(" ")));
        }
    }
    None
}

/// Claude Code PreToolUse hook: JSON on stdin; exit 2 blocks the tool call and shows stderr to the agent.
pub fn hook() -> Result<i32> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    let cmd = v["tool_input"]["command"].as_str().unwrap_or("");
    if let Some(alt) = local_cargo_invocation(cmd) {
        eprintln!(
            "Blocked: this would compile Rust on the user's own computer, which is slow and can freeze it.\n\
             Run it on the remote build machine instead:\n    {alt}\n\
             (MCP clients can call the `cargo` tool of the rcargo server.) \
             If the user explicitly wants a local run, prefix the command with RCARGO_ALLOW_LOCAL=1."
        );
        return Ok(2);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_heavy_local_cargo() {
        for c in [
            "cargo build",
            "cargo test --release",
            "cd x && cargo check --workspace",
            "RUSTFLAGS=-Awarnings cargo clippy",
            "sudo cargo install ripgrep",
            "cargo +nightly build",
            "/home/u/.cargo/bin/cargo run",
            "echo hi; time cargo nextest run",
            "cargo b",
        ] {
            assert!(local_cargo_invocation(c).is_some(), "should block: {c}");
        }
    }

    #[test]
    fn allows_light_and_remote_commands() {
        for c in [
            "rcargo test",
            "cargo fmt",
            "cargo metadata --format-version 1",
            "cargo --version",
            "cargo add serde",
            "cargo tree",
            "git commit -m 'cargo build fixed'",
            "echo cargo build",
            "RCARGO_ALLOW_LOCAL=1 cargo build",
            "ls",
        ] {
            assert!(local_cargo_invocation(c).is_none(), "should allow: {c}");
        }
    }

    #[test]
    fn suggests_rcargo_equivalent() {
        assert_eq!(
            local_cargo_invocation("cargo test --release -- --nocapture").unwrap(),
            "rcargo test --release -- --nocapture"
        );
        assert_eq!(
            local_cargo_invocation("cargo +nightly build").unwrap(),
            "rcargo +nightly build"
        );
    }

    #[test]
    fn merge_json_creates_and_preserves() {
        let d = std::env::temp_dir().join(format!("rcargo-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("s.json");
        std::fs::write(
            &p,
            r#"{"keep": 1, "hooks": {"PreToolUse": [{"matcher":"Edit"}]}}"#,
        )
        .unwrap();
        merge_json(&p, |v| {
            v["hooks"]["PreToolUse"]
                .as_array_mut()
                .unwrap()
                .push(json!({"matcher":"Bash"}));
        })
        .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["keep"], 1);
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        std::fs::write(&p, "// comment\n{}").unwrap();
        assert!(
            merge_json(&p, |_| {}).is_err(),
            "refuses JSONC instead of clobbering it"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
