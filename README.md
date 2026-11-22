# rcargo

**Build and test Rust on someone else's computer, so yours stays cool.**

Rust compiles are heavy. On a laptop with 8 GB of RAM a single `cargo build` can freeze everything, and an AI coding agent will happily start one for you. `rcargo` runs `cargo` on a remote machine ([instabox](https://instabox.sh), [Daytona](https://daytona.io), or any server you can SSH into) and streams the result back. Your editor and your agent keep working; the fan stays quiet.

- **Your code never goes through GitHub.** The working tree, including uncommitted and private code and `path = "../x"` dependencies, is synced over SSH. Only changed files are sent.
- **Fast after the first build.** The remote keeps `target/` between runs, so a one-line edit rebuilds almost instantly. Measured end to end on a free instabox box (4 vCPU): `rcargo test` with nothing changed 1.0 s, one-line edit plus run 1.3 s, cold first build with one dependency 9 s. A warm run makes zero provider calls and reuses one SSH connection; rcargo itself uses about 35 ms of CPU. The build speed itself is whatever the remote machine gives you.
- **Made for AI agents.** An MCP server, an agent skill, compact output (errors first, progress noise removed) and a guard that stops an agent from compiling locally.
- **You don't need Rust installed locally.** The first build on a fresh machine installs the toolchain there.
- **Many machines.** Register several instances (a small default, a big one for release builds); agents are told which exist and what each is for.
- **Tells you what to do when something is missing.** No instance, not logged in, no API key: you get exact numbered steps, marked when a human is needed.
- **Move the slow part of any program to Rust.** `rcargo port` takes a hot function from Python, Node, Ruby or PHP (or anything that can speak JSON), has an agent port only that, proves it gives the same answers on real recorded calls, and swaps it in. Heavy builds run remotely like everything else. See [Port code to Rust](#port-code-to-rust).

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
   - instabox: `npx instabox@latest` (signs in and installs its CLI), then `rcargo instances add main --backend instabox --default --description "general purpose Rust builder"`. If I prefer an API key (no CLI needed), I create one in the instabox dashboard and set INSTABOX_API_KEY.
   - Daytona: I run `daytona login` (or set DAYTONA_API_KEY), then `rcargo instances add dt --backend daytona --set snapshot=daytona-large --default --description "Daytona sandbox, 4 vCPU / 8 GB"` (Daytona's default sandbox is only 1 vCPU / 1 GB, too small for Rust; `daytona snapshot list` shows the sizes available to me, use daytona-medium if my quota is smaller)
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

Want to write something in Rust, or speed up an existing Python/Node/Ruby/PHP project? Paste this (after rcargo is set up):

````text
I want to use Rust, either for a new project or to speed up code I already have. Install rcargo if `rcargo --version` is missing (curl -fsSL https://raw.githubusercontent.com/igun997/rcargo/main/install.sh | sh), then run `rcargo init` if this is a Rust project, or `rcargo port start --json` in this folder. That command inspects the folder (empty, Rust, or code in another language) and returns questions. Ask me those questions in plain words and wait for my answers: do not decide for me whether this is a new project or a port, and never overwrite my files. Then follow the rust-port skill (.claude/skills/rust-port/SKILL.md or .agents/skills/rust-port/SKILL.md; `rcargo port init` installs it). Run every build and test through rcargo, never locally. Finish by telling me what was built, how many real recorded calls matched, and the measured speedup.
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

## Port code to Rust

Rewriting a whole app in Rust costs months. Most of the runtime sits in a few functions. `rcargo port` moves only those, keeps your app, and refuses to call a port correct until it matches the original on real data. The approach (port only the hot 5%, prove it on recorded calls, keep the original as fallback) is adopted from [fast-anything](https://github.com/EL4CTEO/fast-anything) by EL4CTEO, MIT licensed; see [Credits](#credits). What rcargo adds: every build and test runs on your remote machine, and the same flow works with an AI agent through the `rust-port` skill and the `port` MCP tool.

```sh
rcargo port start              # looks at this folder, then asks what you want (see below)
rcargo port init fuzzy --module app/fuzzy.py --function dedupe
rcargo port record fuzzy -- python app.py     # keeps up to 500 real calls (golden cases)
rcargo port expand fuzzy       # edge cases (empty, huge, unicode, negative...), answered by YOUR code
# implement ports/fuzzy/src/port.rs (an agent does this from ports/fuzzy/TASK.md)
rcargo port verify fuzzy       # remote build + replay every case; repeat until all match
rcargo port build fuzzy        # remote release build, binary copied to ports/fuzzy/target/release
rcargo port bench fuzzy        # original vs port, end to end, bridge included
```

`rcargo port start` never assumes. An **empty folder** (apart from README/LICENSE) offers a new Rust project or porting code from another folder. A **Rust project** offers remote-build setup or porting into it. A folder with **existing code** is inspected first: languages, file counts, the biggest files as likely suspects, the command to profile it. Then it asks whether to port the slow functions (recommended), start a separate rewrite in a sub-folder, or do nothing yet. It never overwrites files. In a terminal it asks interactively; an AI agent gets the same questions as JSON (`rcargo port start --json`, or the `port` MCP tool) and must put them to you.

| Language | Recorder and bridge | Notes |
|---|---|---|
| Python 3.8+ | ready-made shim (`@rcargo_port.port`) | standard library only |
| Node 18+ / TypeScript | ready-made shim (`port(fn)`) | Linux and macOS (synchronous bridge over FIFOs) |
| Ruby 3.0+ | ready-made shim (`RcargoPort.wrap`) | |
| PHP 8.0+ | ready-made shim (`RcargoPort::call`) | |
| Go, Java, C#, C/C++, anything | `--lang other` | same Rust crate, JSON-lines protocol and C ABI are generated; you add the few-line recorder (`TASK.md` explains) |

How the proof works: `golden/cases.jsonl` holds real calls plus derived edge cases, each with the answer the original gave. `verify` replays all of them against the Rust port (numbers equal within 1e-9; an error case needs an error). The port is called through one long-lived process speaking JSON lines, so there are no language headers or bindings to install. If the binary is missing, arguments are not plain data, or the port reports a fatal problem, the shim falls back to the original. `RCARGO_PORT_DISABLE=1` switches it off, and removing the one wrapper line undoes the edit.

Limits: port pure, deterministic functions on plain data (numbers, strings, lists, maps). Tuples come back as lists. Integers beyond 2^53 lose precision in Node. `bench` can show that a port is *slower* end to end (tiny work, called too often); then do not enable it, port the outer loop. `rcargo port crates requests` tells you which crate replaces a library. `rcargo port status` lists ports.

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

**Reusing a machine you already have.** A name that already exists at the provider is used as is (nothing is created): `rcargo instances add mine --backend instabox --set box=EXISTING` or `--backend daytona --set name=EXISTING`. Several instances may point at one box; each project still gets its own workspace on it. `rcargo instances discover` lists what your accounts already have, and when a machine rcargo would create does not exist yet, it prints the ones you could reuse instead.

| Backend | What it needs | Notes |
|---|---|---|
| `instabox` | `INSTABOX_API_KEY` (REST API, **no CLI needed**) or the `instabox` CLI, signed in | box is created on first use (`--set box=NAME`, `vcpus`/`mem_mib` on paid plans). Free plan: one box, so all projects share it. Verified end to end in both modes (create, destroy, build, test). |
| `daytona` | `daytona login` (rcargo reuses the CLI's stored login) or `DAYTONA_API_KEY`; `curl` | uses the REST API and Daytona's SSH gateway; the sandbox is created on first use and the Rust toolchain is installed automatically. Verified end to end (create, toolchain bootstrap, sync, test). |
| `ssh` | key-based ssh to `host` | your own VPS or desktop. Linux with `bash`, `tar`, `find`, `flock`. |

**Credential order.** If an API key is in the environment (`INSTABOX_API_KEY`, `DAYTONA_API_KEY`) rcargo uses it and talks to the provider's REST API directly, so the provider CLI does not even need to be installed. Otherwise it falls back to the CLI's stored login (`instabox login`, `daytona login`). For MCP clients, put the key in the rcargo server's `env` block, because clients do not inherit your shell. Keys are sent to `curl` on stdin, never on a command line.

Daytona's default sandbox is 1 vCPU / 1 GB RAM / 3 GB disk, which is too small for most Rust builds. Daytona sizes a sandbox through its snapshot and rejects `cpu`/`memory`/`disk` next to one (its API answers "Cannot specify Sandbox resources when using a snapshot"), so pick a bigger snapshot when you register the instance (applies when the sandbox is created): `rcargo instances add dt --backend daytona --set snapshot=daytona-large` (4 vCPU / 8 GB / 10 GB on the account this was checked with; `daytona snapshot list` shows yours). instabox's default box is 4 vCPU / 8 GiB.

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
rcargo remote destroy     # DELETE the machine itself (dry run; add --yes to do it)
rcargo onboard --live     # what is still missing, and how to fix it
```

### Cleaning up

| What | Command |
|---|---|
| This project's `target/` only | `rcargo clean` (it is `cargo clean`, run remotely) |
| This project's whole remote workspace | `rcargo remote rm` |
| Old workspaces on the machine | `rcargo remote ls`, then `rcargo remote gc --days 14 --yes` (everything: `--all`) |
| Pause the machine (stops billing, keeps files) | `rcargo remote down` |
| Delete the machine and local caches | `rcargo remote destroy --yes` (instabox and Daytona only; SSH hosts are never deleted) |
| Forget an instance in your config | `rcargo instances rm NAME` (does not touch the machine) |

Deleting machines and workspaces is a CLI-only action; the MCP server and the agent skill have no delete tool, and agents are told to ask first.

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

## Credits

`rcargo port` adopts the workflow of **[fast-anything](https://github.com/EL4CTEO/fast-anything)** by EL4CTEO (MIT): profile, port only the hot function, replay real recorded calls plus derived edge cases against the port, enable only when everything matches, keep the original as the fallback. Thank you. This is an independent implementation (a JSON-lines bridge generated per port, shims in the host language, builds on rcargo's remote machines); fast-anything's own tool does more (automatic profiling, native bindings such as PyO3 and napi-rs, more languages) and is worth using on its own. Its license notice is kept in [NOTICE](NOTICE). rcargo is MIT licensed too, so the two are compatible.
