---
name: rust-port
description: Use when the user wants to write something in Rust, rewrite or speed up code from Python, Node/TypeScript, Ruby, PHP, Go, Java, C# or C/C++, port a slow function to Rust, replace a library with a Rust crate, or asks "how do I do X in Rust". Also use when a folder is empty or holds non-Rust code and the user says they want Rust. Works with rcargo, so every build and test runs on a remote machine.
---

# rust-port: move the slow 5% to Rust, prove it, keep the rest

Do not rewrite a whole app in Rust unless the user asks for that. Rewriting costs months. Most of the runtime sits in a few functions: port only those, prove the port gives the same answers on real recorded calls, and keep the original as a fallback. All cargo work goes through `rcargo` (never run cargo build/test locally; read the `rcargo` skill).

## Step 0: look, then ASK THE USER

Run `rcargo port start --json` (or the MCP `port` tool with `action: "start"`) in the folder in question. It inspects the folder and returns `kind` plus `ask_user`:

| kind | meaning | what to do |
|---|---|---|
| `empty` | nothing there | Ask: new Rust project here, or port code from another folder? Do not assume. |
| `rust` | a Rust project | Ask: just set up remote builds, or port code from elsewhere into it? |
| `foreign` | Python/Node/Ruby/PHP/... code | Ask: port the slow functions (recommended) or a full rewrite in a sub-folder? Which function is slow and how is the app run? |
| `mixed` | Rust plus other languages | Ask the same, and which part. |
| `unknown` | files, no recognised source | Ask what is in it. |

Put those questions to the user in plain words, wait for the answer, and only then act. Never overwrite existing files; never decide between "new project" and "port" yourself in a non-empty folder.

- New Rust project: `rcargo port start` (interactive) creates `Cargo.toml` + `src/main.rs` in an empty folder or a named sub-folder and runs `rcargo init`. Without a terminal, create those two files yourself and run `rcargo init`. Library help: `rcargo port crates QUERY [--lang python]` maps libraries to crates (requests -> reqwest, pandas -> polars, ...).
- Port: continue below.

## Step 1: find what is slow (never guess)

Profile a real run. The `start` output prints the command for the language, for example `python -m cProfile -s cumtime app.py | head -30`, `node --cpu-prof app.js`, `go test -bench . -cpuprofile cpu.out`. Pick the OUTERMOST pure function that holds most of the time (a function that loops and calls helpers beats the tiny helper: crossing the bridge once instead of a million times).

Good candidates: deterministic, no I/O, no clock, no randomness, no global state, plain data in and out (numbers, strings, lists, maps), CPU-bound (string distance, parsing, scoring, matrix loops, compression). Bad: network/disk waits, database calls, framework glue, functions taking objects with methods. Tell the user honestly when nothing qualifies.

## Step 2: scaffold

`rcargo port init NAME --module path/to/file.py --function name` creates `ports/NAME/`: a standalone one-function crate, a JSON bridge, a recorder shim for the language (python, node, ruby, php have ready-made shims; use `--lang other` for Go, Java, C#, C/C++ and talk the JSON-lines protocol in `ports/NAME/TASK.md`), and a judge test. `ports/NAME/TASK.md` is your task file: read it.

## Step 3: record, expand, implement, verify

1. Put the shim's one-line decorator/wrapper on the function (instructions at the top of `ports/NAME/shims/rcargo_port.*`).
2. `rcargo port record NAME -- <command that runs the app>` records up to 500 real calls into `golden/cases.jsonl`. Zero recorded means the function was not reached with plain data; fix that first.
3. `rcargo port expand NAME` derives edge cases (empty, huge, unicode, negative, zero, truncated) and lets the ORIGINAL answer them. Never invent expected values by hand.
4. Edit only `ports/NAME/src/port.rs` (`call(args) -> Result<Value, String>`). Return `Err` exactly where the original raises. Never panic.
5. `rcargo port verify NAME` builds and replays every case on the remote machine. Fix the first printed mismatch, run again, until all cases match. Trust the cases over your reading of the original.
6. `rcargo port build NAME`, then `rcargo port bench NAME`. If the port is not clearly faster end to end, do NOT enable it; say so and port the outer loop instead, or stop.
7. Enable = leave the shim wrapper in place (it uses the binary when it exists). Disable = `RCARGO_PORT_DISABLE=1` or delete the wrapper line. The original code is never modified beyond that one line.

MCP: the `port` tool has `start`, `init`, `expand`, `status`, `crates`, `verify`, `build`; `record` and `bench` run your app locally, so use the CLI for them (they are light).

## Rules that catch most wrong ports

- Strings: originals count characters, Rust `len()` counts UTF-8 bytes. Use `chars()`.
- Integers: Python ints never overflow, JS numbers are doubles, Rust integers wrap or panic. Use `i128`, `checked_*`, or `f64` to match.
- Division/modulo of negatives, rounding (half away from zero vs banker's), float printing.
- Ordering: map iteration order and unstable sorts differ; sort explicitly where the original's order shows.
- Tuples and sets come back as JSON lists; objects as maps. Check callers.

## Reporting

Tell the user: what was ported, how many cases matched (and that they are real recorded calls plus derived edge cases), the end-to-end speedup from `bench`, and how to switch it off. Never claim a speedup you did not measure. Do not delete ports, remote workspaces or machines without an explicit yes.
