---
name: rcargo
description: Use for ANY Rust build, check, test, clippy, bench, run or cargo install in a project with Cargo.toml. Runs cargo on a remote machine (instabox, Daytona or an SSH server) instead of the user's computer, which may be a small laptop that freezes on local compiles. Also use when asked to set up remote builds, add/choose a build instance, or when a command prints "SETUP REQUIRED".
---

# rcargo: build Rust on a remote machine, not on the user's laptop

The user's computer is often small (think 8 GB RAM). A local `cargo build` can freeze it. **Never run `cargo build/check/test/clippy/run/bench/doc/install` locally.** Use rcargo; it syncs the working tree (uncommitted and private code, plus `path = ".."` dependencies) to a remote machine over SSH, runs cargo there with a persistent `target/`, and streams the result back. No GitHub access is involved. `cargo fmt`, `cargo add`, `cargo metadata`, `cargo tree` are light and may run locally.

## How to call it

Prefer the MCP server (`rcargo mcp`) when its tools are available:

| Tool | Use |
|---|---|
| `cargo` | `args` = what follows `cargo`: `["check"]`, `["test","--workspace"]`, `["clippy","--","-D","warnings"]`, `["nextest","run"]`, `["+nightly","build"]`. Output is compact: errors first, then failing tests. |
| `shell` | inspect the remote workspace (`ls target/debug`, `du -sh target`). Not for compiling. |
| `sync` | upload changes without running anything |
| `instances` | list instances with descriptions and live state |
| `setup` | readiness check; returns exact steps for whatever is missing |
| `power` | `up` / `down` an instance |
| `workspaces` | disk usage per project on an instance |

Without MCP, use the CLI: `rcargo test`, `rcargo check --workspace`, `rcargo clippy -- -D warnings`, `rcargo -i NAME build --release`. Anything that is not an rcargo command is passed to cargo and run remotely. Add `--pull` (before the subcommand) to copy built executables back to `./target`.

## Several instances

A user may have more than one machine (for example a small default and a big one for release builds). Always check the list before choosing: the `instances` tool, or `rcargo instances`.

- Omit `instance` to use the default.
- If there is **no default** the call fails with SETUP REQUIRED and the list; pick by description or ask the user.
- Choose a bigger instance only for heavy work (release builds, large workspaces) and only when its description says it is bigger.
- **Never** create, delete or reconfigure instances yourself. Suggest it to the user.
- When the session is over and the user is done building, `power` `down` saves money (builds start it automatically).

## When something is missing: SETUP REQUIRED

If rcargo (CLI exit code 3, or an MCP result with `isError` and `structuredContent.setup_required`) prints **SETUP REQUIRED**, it lists numbered steps. Follow them literally:

1. Run steps **not** marked `[USER]` yourself.
2. For `[USER]` steps (browser login, API keys, installing software, dashboard) show the user the exact command or URL in plain words, then **wait** for their confirmation. Do not retry in a loop.
3. Never invent, guess, print or ask the user to paste credentials into the chat. Keys go into the environment or the provider's own login.
4. When the user says it is done, call `setup` (or `rcargo onboard --live`) to verify, then retry the original command.

Typical first-time path for a new user (explain it simply, they may not know Rust or servers):

- No instance yet, simplest: `npx instabox@latest` (signs in and installs the instabox CLI; a free tier exists), then `rcargo instances add main --backend instabox --default --description "general purpose builder"`. Headless agents: `instabox login --agent --name rcargo` prints a claim page and code; relay both to the user and wait.
- Daytona: the user runs `daytona login` once (rcargo reuses the CLI's stored login and refreshes it), or sets `DAYTONA_API_KEY`. For MCP an API key must be in the rcargo server's `env` block of the client config, because clients do not inherit the interactive shell. The sandbox is created on first use.
- Own server: `rcargo instances add vps --backend ssh --set host=user@server`; key-based SSH must already work.
- The first build on a fresh machine installs the Rust toolchain automatically (a few minutes, once).
- One-command project setup for humans: `rcargo init` (writes `rcargo.toml`, this skill, the MCP registration, a rust-analyzer override and a guard that blocks local `cargo build` in Claude Code).

## Reading results

- The header line shows the instance, SUCCESS/FAILED with the exit code and sync stats.
- Fix errors top to bottom; warnings only if asked. Rerun the same `cargo` call to verify.
- Only the first diagnostics are shown if there are many; fix those and rerun.
- Exit 75 / "gave up waiting for the remote workspace lock": another build runs in this workspace; retry shortly.
- Private git dependencies or registries need a token on the remote: add the variable name to `forward_env` in `rcargo.toml` (`forward_env = ["GH_TOKEN"]`); rcargo copies it from the user's environment at run time and never writes it to disk.
- `.cargo/config.toml` settings such as `linker` or `target-dir` also apply remotely; if the remote build fails on those, tell the user.
- Disk full on the remote: `rcargo remote ls`, then `rcargo remote gc --days 7 --yes` (ask first) or `cargo clean` through the `cargo` tool.
- **Never** run `rcargo remote destroy`, `rcargo remote rm` or `rcargo remote gc --all` on your own initiative: they delete the user's remote machine or work. Offer them and wait for an explicit yes.
