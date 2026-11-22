# Port `{{FUNCTION}}` ({{LANG}}) to Rust

You are porting ONE function. Everything runs through `rcargo`; never run `cargo` on this computer.

- Original: `{{MODULE}}` :: `{{FUNCTION}}`
- Edit ONLY `src/port.rs`. `src/bridge.rs` is the fixed JSON glue, `tests/golden.rs` is the judge.
- The judge is `golden/cases.jsonl`: real calls of the original, one per line:
  `{"args":[...],"expect":{"ok":VALUE}|{"err":"message"},"ns":ORIGINAL_NANOSECONDS}`.
  The port is correct when every case matches (numbers within 1e-9; an error case needs an `Err`).

## Loop

1. Record real calls. In the app, put the shim's decorator/wrapper on the function (see the top of
   `shims/rcargo_port.{{EXT}}`), then run the app normally with `RCARGO_PORT_RECORD=<this folder>/golden/cases.jsonl`
   (`rcargo port record {{NAME}} -- <your command>` sets it for you). Up to 500 distinct calls are kept.
2. Add edge cases the real run never hit: `rcargo port expand {{NAME}}` derives inputs (empty, huge, unicode, negative,
   zero, truncated, doubled) and lets the ORIGINAL answer them, so expectations are never guessed.
3. Implement `call` in `src/port.rs`. Read the original, but trust the cases over your reading of it.
4. `rcargo port verify {{NAME}}` builds and replays every case on the remote machine. Fix the first mismatch it
   prints, run it again. Repeat until it says all cases match.
5. `rcargo port build {{NAME}}` builds the release binary and copies it to `target/release/{{BIN}}`.
6. `rcargo port bench {{NAME}}` times original vs port end to end through the bridge. If the port is not clearly
   faster, do not enable it: the work is too small or the function is called too often with tiny inputs. Port the
   outermost loop that calls it instead.
7. Enable by leaving the shim wrapper in place (it uses the binary when it exists). Disable any time with
   `RCARGO_PORT_DISABLE=1`, or remove the wrapper line.

## Protocol (for languages without a ready shim)

`target/release/{{BIN}}` reads one JSON line per request on stdin and writes one line on stdout:
request `{"args":[...]}` -> `{"ok":VALUE}` | `{"err":"message"}` | `{"fatal":"why"}` (fall back to the original on `fatal`).
Keep one process alive and reuse it. A C ABI is exported too: `rcargo_port_call(const char*) -> char*` and
`rcargo_port_free(char*)` (build the `cdylib`), with the same request/response lines.
To produce `golden/cases.jsonl` from another language, write one JSON line per call as above.

## Rules that catch most wrong ports

- Strings: the original probably counts characters; Rust `len()`/slicing counts bytes. Use `chars()`.
- Integers: Python ints never overflow; JS numbers are doubles; Rust integers wrap or panic. Use `i128`/`checked_*`
  or `f64` to match.
- `/` and `%` on negatives, rounding (`round` half away from zero vs banker's), float printing.
- Ordering: dict/hash iteration order and unstable sorts differ; sort explicitly where the original's order shows.
- Raise where the original raises (return `Err`), and nowhere else. Never panic; a panic is reported as `fatal`.
- Port pure, deterministic functions on plain data (numbers, strings, lists, maps). Not I/O, clocks, randomness,
  globals, or objects with methods.
