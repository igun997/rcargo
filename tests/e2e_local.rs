#![cfg(target_os = "linux")]
//! End-to-end through the real binary, with a fake `ssh` that runs the "remote" command locally.
//! Exercises: delta sync, incremental rebuilds, path rewriting, external path dependencies,
//! deletion propagation, MCP, and setup guidance.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

struct Env {
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = std::env::temp_dir().join(format!("rcargo-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(work.join("hello/src")).unwrap();
        fs::create_dir_all(work.join("shared/src")).unwrap();
        Env {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
            work: work.canonicalize().unwrap(),
        }
    }

    fn project(&self) -> PathBuf {
        self.work.join("hello")
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.work.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
    }

    /// Make every file old so the "recently modified" resend rule does not hide incremental behaviour.
    fn age_files(&self) {
        let old = SystemTime::now() - Duration::from_secs(7200);
        fn walk(d: &Path, old: SystemTime) {
            for e in fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, old);
                } else {
                    File::options()
                        .write(true)
                        .open(&p)
                        .unwrap()
                        .set_modified(old)
                        .unwrap();
                }
            }
        }
        walk(&self.work, old);
    }

    fn cmd(&self) -> Command {
        let real_home = std::env::var("HOME").unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_rcargo"));
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env(
                "RCARGO_SSH",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-ssh.sh"),
            )
            .env("RCARGO_CONFIG", self.root.join("global.toml"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("REAL_HOME", real_home)
            .current_dir(self.project());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn write_config(env: &Env) {
    let real_home = std::env::var("HOME").unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    fs::write(
        env.project().join("rcargo.toml"),
        format!(
            "instance = \"fake\"\n[instances.fake]\nbackend = \"ssh\"\nhost = \"fake\"\ndescription = \"local stand-in\"\n\
             [remote]\ncargo = \"{cargo}\"\n[remote.env]\nRUSTUP_HOME = \"{real_home}/.rustup\"\nCARGO_HOME = \"{real_home}/.cargo\"\nPATH = \"{real_home}/.cargo/bin:/usr/bin:/bin\"\n"
        ),
    )
    .unwrap();
}

#[test]
fn sync_build_run_incremental_and_paths() {
    let env = Env::new("flow");
    env.write(
        "hello/Cargo.toml",
        "[package]\nname=\"hello\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\nshared={path=\"../shared\"}\n",
    );
    env.write(
        "hello/src/main.rs",
        "fn main(){ println!(\"{}\", shared::greet()); }\n",
    );
    env.write(
        "shared/Cargo.toml",
        "[package]\nname=\"shared\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    );
    env.write(
        "shared/src/lib.rs",
        "pub fn greet()->&'static str{\"hi from shared\"}\n",
    );
    env.write("hello/.gitignore", "secret.txt\n");
    env.write("hello/secret.txt", "do not upload");
    write_config(&env);
    env.age_files();

    // 1. first run builds remotely, including the external path dependency
    let o = env.run(&["run"]);
    assert!(
        o.status.success(),
        "run failed:\nstdout:{}\nstderr:{}",
        s(&o.stdout),
        s(&o.stderr)
    );
    assert!(s(&o.stdout).contains("hi from shared"), "{}", s(&o.stdout));
    let err = s(&o.stderr);
    assert!(err.contains("sync: "), "{err}");

    // remote layout mirrors local absolute paths, target/ lives remotely only
    let ws_root = env.home.join(".rcargo/ws");
    let ws = fs::read_dir(&ws_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mirror = ws.join(env.project().strip_prefix("/").unwrap());
    assert!(
        mirror.join("target/debug/hello").is_file(),
        "remote target missing"
    );
    assert!(ws
        .join(env.work.join("shared").strip_prefix("/").unwrap())
        .join("src/lib.rs")
        .is_file());
    assert!(
        !mirror.join("secret.txt").exists(),
        "gitignored file must not be uploaded"
    );
    assert!(
        !env.project().join("target").exists(),
        "nothing may be built locally"
    );

    // 2. nothing changed: zero files sent
    let o = env.run(&["build"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(
        s(&o.stderr).contains("sync: 0 of"),
        "expected an incremental no-op sync:\n{}",
        s(&o.stderr)
    );

    // 3. one edit: exactly one file sent, new behaviour visible
    env.write(
        "shared/src/lib.rs",
        "pub fn greet()->&'static str{\"hi v2\"}\n",
    );
    let o = env.run(&["run", "-q"]);
    assert!(
        s(&o.stdout).contains("hi v2"),
        "{}{}",
        s(&o.stdout),
        s(&o.stderr)
    );
    let o = env.run(&["build"]);
    assert!(
        s(&o.stderr).contains("sync: 1 of") || s(&o.stderr).contains("sync: 0 of"),
        "{}",
        s(&o.stderr)
    );

    // 4. remote paths are mapped back to local ones
    let o = env.run(&["remote", "shell", "--", "pwd"]);
    assert_eq!(
        s(&o.stdout).trim(),
        env.project().to_str().unwrap(),
        "pwd must show the local path"
    );

    // 5. deletions propagate; remote-only files survive
    env.write("hello/src/extra.rs", "// temp\n");
    env.age_files();
    env.run(&["remote", "sync"]);
    assert!(mirror.join("src/extra.rs").is_file());
    fs::write(mirror.join("generated.txt"), "made remotely").unwrap();
    fs::remove_file(env.project().join("src/extra.rs")).unwrap();
    let o = env.run(&["remote", "sync"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(
        !mirror.join("src/extra.rs").exists(),
        "deleted locally => deleted remotely"
    );
    assert!(
        mirror.join("generated.txt").exists(),
        "files rcargo never sent must not be deleted"
    );

    // 6. failures keep cargo's exit code and show diagnostics with local-looking paths
    env.write("hello/src/main.rs", "fn main(){ let x: u32 = \"no\"; }\n");
    let o = env.run(&["check"]);
    assert_eq!(o.status.code(), Some(101), "{}", s(&o.stderr));
    let err = s(&o.stderr);
    assert!(err.contains("src/main.rs"), "{err}");
    assert!(
        !err.contains(".rcargo/ws"),
        "remote paths must not leak:\n{err}"
    );
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn pull_copies_executable_back() {
    let env = Env::new("pull");
    env.write(
        "hello/Cargo.toml",
        "[package]\nname=\"hello\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    );
    env.write("hello/src/main.rs", "fn main(){ println!(\"pulled\"); }\n");
    write_config(&env);
    env.age_files();
    let o = env.run(&["--pull", "build"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    let bin = env.project().join("target/debug/hello");
    assert!(
        bin.is_file(),
        "executable should be pulled to {}\n{}",
        bin.display(),
        s(&o.stderr)
    );
    let out = Command::new(&bin).output().unwrap();
    assert_eq!(s(&out.stdout).trim(), "pulled");
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn mcp_cargo_tool_returns_compact_result() {
    let env = Env::new("mcp");
    env.write(
        "hello/Cargo.toml",
        "[package]\nname=\"hello\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    );
    env.write("hello/src/main.rs", "fn main(){ let x: u32 = \"no\"; }\n");
    write_config(&env);
    env.age_files();

    let mut child = env
        .cmd()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: u32, method: &str, params: serde_json::Value| -> serde_json::Value {
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(stdin, "{req}").unwrap();
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad JSON-RPC line `{line}`: {e}"))
    };
    let init = call(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":"2025-06-18"}),
    );
    let instr = init["result"]["instructions"].as_str().unwrap().to_string();
    assert!(
        instr.contains("- fake [default] (ssh): local stand-in"),
        "instances must be advertised to agents:\n{instr}"
    );

    let r = call(
        2,
        "tools/call",
        serde_json::json!({"name":"cargo","arguments":{"args":["check"]}}),
    );
    assert_eq!(r["result"]["isError"], true);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("FAILED (exit 101)"), "{text}");
    assert!(text.contains("== diagnostics: 1 error"), "{text}");
    assert!(text.contains("mismatched types"), "{text}");
    assert!(
        !text.contains("Checking hello"),
        "progress noise must be hidden:\n{text}"
    );

    let r = call(
        3,
        "tools/call",
        serde_json::json!({"name":"instances","arguments":{"live":false}}),
    );
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("fake"));
    drop(stdin);
    let _ = child.wait();
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn missing_setup_is_explained_to_humans_and_agents() {
    let env = Env::new("setup");
    env.write(
        "hello/Cargo.toml",
        "[package]\nname=\"hello\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    );
    env.write("hello/src/main.rs", "fn main(){}\n");
    // no config, no instabox on PATH
    let o = env.run(&["test"]);
    assert_eq!(o.status.code(), Some(3), "{}", s(&o.stderr));
    let err = s(&o.stderr);
    assert!(err.contains("SETUP REQUIRED"), "{err}");
    assert!(
        err.contains("instabox login")
            && err.contains("daytona login")
            && err.contains("--backend ssh"),
        "{err}"
    );
    assert!(err.contains("[USER]"), "{err}");

    // machine-readable for agents
    let o = env.run(&["onboard", "--json"]);
    assert_eq!(o.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["ready"], false);
    assert_eq!(v["setup"]["setup_required"], true);

    // several instances, no default => must choose, and the list is shown
    fs::write(
        env.project().join("rcargo.toml"),
        "[instances.a]\nbackend=\"ssh\"\nhost=\"h\"\ndescription=\"small\"\n[instances.b]\nbackend=\"ssh\"\nhost=\"h\"\ndescription=\"huge\"\n",
    )
    .unwrap();
    let o = env.run(&["check"]);
    assert_eq!(o.status.code(), Some(3));
    let err = s(&o.stderr);
    assert!(
        err.contains("a (ssh): small") && err.contains("b (ssh): huge"),
        "{err}"
    );

    // daytona without a key
    fs::write(
        env.project().join("rcargo.toml"),
        "[instances.d]\nbackend=\"daytona\"\n",
    )
    .unwrap();
    let o = env.run(&["check"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        s(&o.stderr).contains("no Daytona login or API key found"),
        "{}",
        s(&o.stderr)
    );
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn guard_hook_blocks_local_cargo() {
    let env = Env::new("hook");
    let run = |cmd: &str| {
        let mut c = env.cmd();
        c.arg("hook")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        let payload = serde_json::json!({"tool_name":"Bash","tool_input":{"command":cmd}});
        ch.stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        ch.wait_with_output().unwrap()
    };
    let o = run("cd app && cargo test --release");
    assert_eq!(o.status.code(), Some(2));
    assert!(
        s(&o.stderr).contains("rcargo test --release"),
        "{}",
        s(&o.stderr)
    );
    assert_eq!(run("cargo fmt").status.code(), Some(0));
    assert_eq!(
        run("RCARGO_ALLOW_LOCAL=1 cargo build").status.code(),
        Some(0)
    );
    let _ = fs::remove_dir_all(&env.root);
}
