# rcargo

**Build and test Rust on someone else's computer, so yours stays cool.**

Rust compiles are heavy. On a laptop with 8 GB of RAM a single `cargo build` can freeze everything, and an AI coding agent will happily start one for you. `rcargo` runs `cargo` on a remote machine ([instabox](https://instabox.sh), [Daytona](https://daytona.io), or any server you can SSH into) and streams the result back. Your editor and your agent keep working; the fan stays quiet.

- **Your code never goes through GitHub.** The working tree, including uncommitted and private code and `path = "../x"` dependencies, is synced over SSH. Only changed files are sent.
- **Fast after the first build.** The remote keeps `target/` between runs, so a one-line edit rebuilds in about a second (measured: 2.8 s wall clock end to end on a free instabox box).
- **Made for AI agents.** An MCP server, an agent skill, compact output (errors first, progress noise removed) and a guard that stops an agent from compiling locally.
- **You don't need Rust installed locally.** The first build on a fresh machine installs the toolchain there.
- **Many machines.** Register several instances (a small default, a big one for release builds); agents are told which exist and what each is for.
- **Tells you what to do when something is missing.** No instance, not logged in, no API key: you get exact numbered steps, marked when a human is needed.

## Easiest: paste this into your AI agent

Open your project in Claude Code, Codex, Cursor or any agent that can run shell commands, and paste the prompt below. The agent installs rcargo, wires itself up, walks you through anything it cannot do for you (logins, keys), and then proves it works.

````text
Set up rcargo in this project so that Rust builds and tests run on a remote machine instead of on this computer. rcargo is https://github.com/igun997/rcargo (read its README if you need details).

Work through these steps in order. Whenever a step needs me (a browser login, an API key, choosing a provider, installing software), stop, tell me in plain words exactly what to do and which command or link to use, and wait for me to say it is done. Never invent or guess credentials, and never ask me to paste secrets into this chat.

1. Check `rcargo --version`. If it is missing, install the prebuilt binary (nothing to compile):
   curl -fsSL https://raw.githubusercontent.com/igun997/rcargo/main/install.sh | sh
   If that fails, fall back to: CARGO_BUILD_JOBS=2 cargo install --git https://github.com/igun997/rcargo
   Make sure the install folder (usually ~/.local/bin) is on PATH for this session.
2. Go to the folder that contains this project's Cargo.toml and run `rcargo init`. It creates rcargo.toml, installs the agent skill, registers the MCP server, makes rust-analyzer check remotely, and installs a guard that blocks local cargo builds.
3. Run `rcargo onboard --live`. If it prints SETUP REQUIRED, follow its steps: run the steps that are not marked [USER] yourself, and for [USER] steps ask me. If no remote machine is configured, ask me which I want and recommend instabox (easiest, has a free tier):
   - instabox: `npx instabox@latest` (signs in and installs its CLI), then `rcargo instances add main --backend instabox --default --description "general purpose Rust builder"`
   - Daytona: I run `daytona login` (or set DAYTONA_API_KEY), then `rcargo instances add dt --backend daytona --set cpu=4 --set memory=8 --set disk=10 --default --description "Daytona sandbox, 4 vCPU"` (Daytona's default sandbox is only 1 vCPU / 1 GB, too small for Rust; lower the numbers if my plan's quota is smaller)
   - my own server: `rcargo instances add vps --backend ssh --set host=USER@SERVER --default --description "my build server"` (key-based ssh must already work)
   If I already have several machines, run `rcargo instances discover`, and register each one with a description that says what it is good for (for example "32 cores, use for release builds").
4. Run `rcargo remote doctor`, then `rcargo test` (or `rcargo check` if there are no tests) and show me the result. The first build on a new machine installs the Rust toolchain there, which takes a few minutes.
5. From now on never run cargo build, check, test, clippy, run, bench or install on this computer. Use `rcargo <the same arguments>` instead, or the `cargo` tool of the rcargo MCP server. Read .claude/skills/rcargo/SKILL.md (or .agents/skills/rcargo/SKILL.md) now and follow it, including how to choose between instances.
6. Tell me in plain English what you set up, which instance is the default, that I may need to restart this agent so it loads the new MCP server and skill, and that `rcargo remote down` pauses the remote machine to save money (it wakes up by itself on the next build).
````

Already set up and just want to add another machine later? Paste this instead:

````text
Add another remote build machine to rcargo. Run `rcargo instances` to see what exists, ask me which provider I want (instabox, Daytona, or my own SSH server), do the steps that do not need me, and ask me for the ones that do (logins, keys). Register it with `rcargo instances add NAME --backend ... --description "what this machine is good for"`, then verify with `rcargo -i NAME check` and tell me how to use it (`rcargo -i NAME build --release`).
````

## Quick start (3 steps)

```sh
# 1. install (a prebuilt binary, nothing to compile)
curl -fsSL https://raw.githubusercontent.com/igun997/rcargo/main/install.sh | sh

# 2. inside your Rust project folder: set everything up
rcargo init

# 3. build and test on the remote machine
rcargo test
```

`rcargo init` writes `rcargo.toml`, installs the agent skill, registers the MCP server, makes rust-analyzer check remotely, and installs the local-compile guard. Then it tells you what is still missing, for example:

```
SETUP REQUIRED: no remote instance is configured, so there is nowhere to build

Pick ONE of these options ([USER] = needs the human, e.g. a browser login or a secret):

  A. [USER] instabox cloud box (simplest; has a free tier)
       $ npm install -g instabox
       $ instabox login
       $ rcargo instances add main --backend instabox --default --description "general purpose Rust builder"
  B. [USER] Daytona sandbox ...
  C. [USER] Your own server or VPS reachable by ssh ...
```

Any `rcargo <anything>` that is not an rcargo command is passed to cargo and runs remotely:

```sh
rcargo check
rcargo clippy --workspace -- -D warnings
rcargo test --release
rcargo nextest run
rcargo +nightly build
rcargo --pull build --release      # copy the built executable back into ./target
```

## Instances (several machines)

```sh
rcargo instances                                   # list with live state and descriptions
rcargo instances add big --backend ssh --set host=me@build.example.com \
      --description "32-core server, use for release builds"
rcargo instances add dt  --backend daytona --description "Daytona sandbox"
rcargo instances use big                           # make it the default
rcargo instances discover                          # find boxes your accounts already have
rcargo -i big build --release                      # pick per command (or RCARGO_INSTANCE=big)
```

Choice order: `-i` / `RCARGO_INSTANCE`, then `instance = "NAME"` in the project's `rcargo.toml`, then your default, then the only instance. With several instances and no default, rcargo asks you (or the agent) to choose instead of guessing.

| Backend | What it needs | Notes |
|---|---|---|
| `instabox` | the `instabox` CLI, signed in | box is created on first use (`--set box=NAME`, `vcpus`/`mem_mib` on paid plans). Free plan: one box, so all projects share it. Verified end to end. |
| `daytona` | `daytona login` (rcargo reuses the CLI's stored login) or `DAYTONA_API_KEY`; `curl` | uses the REST API and Daytona's SSH gateway; the sandbox is created on first use and the Rust toolchain is installed automatically. Verified end to end (create, toolchain bootstrap, sync, test). |
| `ssh` | key-based ssh to `host` | your own VPS or desktop. Linux with `bash`, `tar`, `find`, `flock`. |

Daytona's default sandbox is 1 vCPU / 1 GB RAM / 3 GB disk, which is too small for most Rust builds. Size it when you register it (applies when the sandbox is created): `rcargo instances add dt --backend daytona --set cpu=4 --set memory=8 --set disk=10` (`memory` and `disk` are GB; stay within your plan's quota). instabox's default box is 4 vCPU / 8 GiB.

Instance settings live in `~/.config/rcargo/config.toml` (`$RCARGO_CONFIG`), project settings in `rcargo.toml`.

## AI agents

- **MCP**: `rcargo mcp` (stdio). Tools: `cargo`, `shell`, `sync`, `instances`, `setup`, `power`, `workspaces`. The server instructions and the `cargo` tool description list your instances and their descriptions, so the agent knows what it can choose from. `rcargo init` writes `.mcp.json`; other clients: `claude mcp add rcargo -- rcargo mcp`, `codex mcp add rcargo -- rcargo mcp`.
- **Skill**: `skills/rcargo/SKILL.md` (installed into `.claude/skills` and `.agents/skills`). It teaches the agent to use rcargo instead of local cargo, to choose among instances, and how to react to `SETUP REQUIRED`.
- **Guard**: a Claude Code hook (`rcargo hook`) that blocks local `cargo build/test/check/clippy/run...` with a message pointing to the rcargo equivalent. Override once with `RCARGO_ALLOW_LOCAL=1 cargo ...`.
- **Setup guidance for agents**: failures caused by missing prerequisites exit with code 3 (CLI) or return `isError` plus `structuredContent.setup_required` (MCP), with steps marked `needs_user` when a human must act (browser login, API keys). Agents are told never to invent credentials.

## Editors

`rcargo init` sets `rust-analyzer.check.overrideCommand` in `.vscode/settings.json` so the "check on save" step runs remotely. rust-analyzer still indexes locally; on very small machines consider `"rust-analyzer.cargo.buildScripts.enable": false` and closing other heavy apps.

## How it works

1. **Sync.** One SSH round trip lists size, mtime (nanoseconds) and mode of files already on the remote. Only differences are sent, as a tar stream over the same SSH session; deletions are propagated only for files rcargo itself sent. `.gitignore`, `.ignore` and `.rcargoignore` are honoured; `target/` and `.git/` are never sent. Files are mirrored at their absolute local path, so `../shared` path dependencies and relative layouts just work.
2. **Run.** The same session takes a per-workspace lock (parallel agents queue instead of corrupting each other), extracts the tar, and runs `cargo`. A multiplexed SSH connection is reused between commands.
3. **Stream back.** stdout/stderr are streamed live; remote paths in output are rewritten to local ones, so editors and agents can jump to files. Exit codes are preserved.

Secrets: `.env`, `.env.local`, `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*` and `.npmrc` are never uploaded unless you set `sync.default_excludes = false`. Environment variables for private registries or private git dependencies are passed explicitly with `forward_env = ["GH_TOKEN"]` in `rcargo.toml`; they travel over SSH stdin and are never written to disk or the process list.

## Housekeeping

```sh
rcargo remote status      # instance + this project's remote workspace
rcargo remote doctor      # check local tools, instance and remote toolchain
rcargo remote ls          # workspaces on the instance with disk usage
rcargo remote gc --days 7 --yes
rcargo remote shell       # shell in the remote workspace (or: rcargo remote shell -- ls target)
rcargo remote down        # pause the instance to save money (it resumes automatically)
rcargo onboard --live     # what is still missing, and how to fix it
```

## Limits (read these)

- The remote must be Linux (Debian/Ubuntu images are what `rcargo remote setup` knows). The client runs on Linux and macOS.
- Binaries built remotely are Linux binaries. `--pull` is useful when your machine is the same OS and architecture; otherwise run things remotely (`rcargo run`).
- `.cargo/config.toml` is synced too. Settings such as a custom `linker` or `target-dir` also apply remotely; `rcargo remote doctor` warns about them.
- Absolute path dependencies are mirrored at the same absolute path under the workspace, so Cargo's relative paths work; paths hard-coded inside build scripts that point outside the project will not.
- Interactive programs (stdin) are not forwarded; `rcargo run` is for programs that do not read the keyboard.
- Your sources are uploaded to the provider you choose. Use a provider you trust for private code.

## Development

```sh
cargo test        # unit tests + end-to-end suite (real binary, fake ssh)
```

MIT licensed.
