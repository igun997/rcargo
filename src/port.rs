//! `rcargo port`: move the slow part of a Python / Node / Ruby / PHP (or any) program to Rust
//! without rewriting the program, and prove the port behaves like the original.
//!
//! Flow: `start` (look at the folder, ask what the user wants) -> `init` (a one-function crate with a
//! JSON bridge, a recorder shim and a judge test) -> `record` real calls -> `expand` edge cases
//! answered by the original -> implement -> `verify` / `build` on the remote machine (never locally)
//! -> `bench` end to end. The heavy part (cargo) goes through the normal rcargo remote path.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{cmds, init};

const T_CARGO: &str = include_str!("../templates/port/Cargo.toml");
const T_LIB: &str = include_str!("../templates/port/lib.rs");
const T_PORT: &str = include_str!("../templates/port/port.rs");
const T_BRIDGE: &str = include_str!("../templates/port/bridge.rs");
const T_MAIN: &str = include_str!("../templates/port/main.rs");
const T_GOLDEN: &str = include_str!("../templates/port/golden.rs");
const T_TASK: &str = include_str!("../templates/port/TASK.md");
const T_CRATES: &str = include_str!("../templates/port/crates.tsv");
const SHIMS: [(&str, &str, &str); 4] = [
    ("python", "py", include_str!("../templates/port/shim.py")),
    ("node", "cjs", include_str!("../templates/port/shim.cjs")),
    ("ruby", "rb", include_str!("../templates/port/shim.rb")),
    ("php", "php", include_str!("../templates/port/shim.php")),
];

#[derive(Subcommand)]
pub enum PortCmd {
    /// Look at a folder and decide what to do: port code to Rust, or start a new Rust project. Asks you (or tells an AI agent which questions to ask you).
    Start {
        /// Folder to look at (default: here).
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Create `ports/NAME`: a one-function Rust crate, a bridge and the recorder for your language.
    Init {
        name: String,
        /// python | node | ruby | php | other (default: guessed from --module).
        #[arg(long)]
        lang: Option<String>,
        /// Source file that holds the function to port.
        #[arg(long)]
        module: Option<PathBuf>,
        /// Function to port (`name`, or `Class.method`).
        #[arg(long)]
        function: Option<String>,
    },
    /// Run your app once while recording real calls of the function into golden/cases.jsonl.
    Record {
        name: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Add edge cases (empty, huge, unicode, negative, truncated...) answered by the ORIGINAL function.
    Expand {
        name: String,
        #[arg(long, default_value_t = 300)]
        limit: usize,
    },
    /// Build the port on the remote machine and replay every recorded case against it.
    Verify { name: String },
    /// Build the release binary on the remote machine and copy it to ports/NAME/target/release.
    Build { name: String },
    /// Time the original against the port, end to end through the bridge (runs locally, light).
    Bench { name: String },
    /// List the ports of this project with their cases and build state.
    Status { name: Option<String> },
    /// Which Rust crate replaces which library (`rcargo port crates requests`).
    Crates {
        query: Vec<String>,
        #[arg(long)]
        lang: Option<String>,
    },
}

pub fn run(instance: Option<&str>, cmd: PortCmd) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let show = |r: Result<(i32, String)>| -> Result<i32> {
        let (code, text) = r?;
        print!("{text}");
        Ok(code)
    };
    match cmd {
        PortCmd::Start { dir, json } => start(dir, json),
        PortCmd::Init {
            name,
            lang,
            module,
            function,
        } => {
            let (_, text) = init_port(
                &cwd,
                &name,
                lang.as_deref(),
                module.as_deref(),
                function.as_deref(),
            )?;
            print!("{text}");
            Ok(0)
        }
        PortCmd::Record { name, command } => record(&cwd, &name, &command),
        PortCmd::Expand { name, limit } => show(expand(&cwd, &name, limit)),
        PortCmd::Verify { name } => {
            let p = find_port(&cwd, &name)?;
            std::env::set_current_dir(&p.dir)?;
            cmds::cargo(instance, verify_args(), false, false)
        }
        PortCmd::Build { name } => {
            let p = find_port(&cwd, &name)?;
            std::env::set_current_dir(&p.dir)?;
            let code = cmds::cargo(instance, build_args(&p.bin()), true, false)?;
            if code == 0 {
                println!("binary: {}", p.binary().display());
            }
            Ok(code)
        }
        PortCmd::Bench { name } => show(bench(&cwd, &name)),
        PortCmd::Status { name } => {
            print!("{}", status(&cwd, name.as_deref())?);
            Ok(0)
        }
        PortCmd::Crates { query, lang } => {
            print!("{}", crates(&query.join(" "), lang.as_deref()));
            Ok(0)
        }
    }
}

pub fn verify_args() -> Vec<String> {
    ["test", "--release", "--test", "golden", "--", "--nocapture"]
        .map(String::from)
        .to_vec()
}

pub fn build_args(bin: &str) -> Vec<String> {
    ["build", "--release", "--bin", bin]
        .map(String::from)
        .into_iter()
        .collect()
}

// ------------------------------------------------------------------ ports on disk

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Meta {
    pub name: String,
    pub lang: String,
    pub function: String,
    /// Path of the original source file, relative to the project root (the folder holding `ports/`).
    pub module: String,
}

pub struct Port {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub meta: Meta,
}

impl Port {
    pub fn bin(&self) -> String {
        format!("{}-port", self.meta.name)
    }
    fn binary(&self) -> PathBuf {
        self.dir.join("target/release").join(self.bin())
    }
    fn cases(&self) -> PathBuf {
        self.dir.join("golden/cases.jsonl")
    }
}

fn valid_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && n.len() <= 40
}

pub fn find_port(from: &Path, name: &str) -> Result<Port> {
    for a in from.ancestors() {
        let dir = a.join("ports").join(name);
        let f = dir.join("port.toml");
        if f.is_file() {
            let meta: Meta = toml::from_str(&std::fs::read_to_string(&f)?)
                .with_context(|| format!("reading {}", f.display()))?;
            return Ok(Port {
                root: a.to_path_buf(),
                dir,
                meta,
            });
        }
    }
    bail!("no port named `{name}` here; create it with `rcargo port init {name} --module FILE --function NAME` (see `rcargo port status`)")
}

fn lang_of_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "py" => "python",
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => "node",
        "rb" => "ruby",
        "php" => "php",
        "go" => "go",
        "java" | "kt" => "java",
        "cs" => "csharp",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "rs" => "rust",
        _ => return None,
    })
}

fn render(t: &str, vars: &[(&str, &str)]) -> String {
    let mut s = t.to_string();
    for (k, v) in vars {
        s = s.replace(&format!("{{{{{k}}}}}"), v);
    }
    s
}

pub fn init_port(
    root: &Path,
    name: &str,
    lang: Option<&str>,
    module: Option<&Path>,
    function: Option<&str>,
) -> Result<(PathBuf, String)> {
    if !valid_name(name) {
        bail!("port name `{name}` must be lowercase letters, digits, `-` or `_`, starting with a letter");
    }
    let lang = match (lang, module) {
        (Some(l), _) => l.to_string(),
        (None, Some(m)) => m
            .extension()
            .and_then(|e| e.to_str())
            .and_then(lang_of_ext)
            .filter(|l| SHIMS.iter().any(|s| s.0 == *l))
            .unwrap_or("other")
            .to_string(),
        (None, None) => bail!("give --lang (python, node, ruby, php, other) or --module FILE"),
    };
    if lang != "other" && !SHIMS.iter().any(|s| s.0 == lang) {
        bail!(
            "no ready-made shim for `{lang}`: use --lang other (the Rust side and the JSON protocol are the same; TASK.md explains it)"
        );
    }
    let module_rel = match module {
        Some(m) => {
            let abs = if m.is_absolute() {
                m.to_path_buf()
            } else {
                root.join(m)
            };
            if !abs.is_file() {
                bail!("--module {} is not a file", abs.display());
            }
            match abs.strip_prefix(root) {
                Ok(r) => r.to_string_lossy().into_owned(),
                Err(_) => abs.to_string_lossy().into_owned(),
            }
        }
        None => String::new(),
    };
    let dir = root.join("ports").join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let bin = format!("{name}-port");
    let ext = SHIMS
        .iter()
        .find(|s| s.0 == lang)
        .map(|s| s.1)
        .unwrap_or("");
    let vars = [
        ("NAME", name),
        ("BIN", bin.as_str()),
        ("LANG", lang.as_str()),
        ("FUNCTION", function.unwrap_or("FUNCTION")),
        (
            "MODULE",
            if module_rel.is_empty() {
                "MODULE"
            } else {
                module_rel.as_str()
            },
        ),
        ("EXT", ext),
    ];
    let put = |rel: &str, content: String| -> Result<()> {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, content).with_context(|| format!("writing {rel}"))
    };
    put("Cargo.toml", render(T_CARGO, &vars))?;
    put("src/lib.rs", render(T_LIB, &vars))?;
    put("src/port.rs", render(T_PORT, &vars))?;
    put("src/bridge.rs", render(T_BRIDGE, &vars))?;
    put("src/main.rs", render(T_MAIN, &vars))?;
    put("tests/golden.rs", render(T_GOLDEN, &vars))?;
    put("golden/cases.jsonl", String::new())?;
    put("TASK.md", render(T_TASK, &vars))?;
    put(".gitignore", "target/\ngolden/candidates.jsonl\n".into())?;
    put(
        "port.toml",
        format!(
            "name = {:?}\nlang = {:?}\nfunction = {:?}\nmodule = {:?}\n",
            name,
            lang,
            function.unwrap_or(""),
            module_rel
        ),
    )?;
    if let Some((_, ext, src)) = SHIMS.iter().find(|s| s.0 == lang) {
        put(&format!("shims/rcargo_port.{ext}"), render(src, &vars))?;
    }
    let mut out = String::new();
    for p in init::install_skills(root)? {
        out += &format!("  + agent skill: {}\n", p.display());
    }
    out += &format!(
        "Created ports/{name}  ({lang}, function `{}`)\n",
        function.unwrap_or("?")
    );
    out += "\nNext steps (builds run on the remote machine, never on this computer):\n";
    out += &format!("  1. Put the recorder on the function. See the top of ports/{name}/shims/rcargo_port.{ext}\n");
    out += "     (one decorator or wrapper line; it does nothing until recording or a built port exists).\n";
    out += &format!("  2. rcargo port record {name} -- <the command that runs your app>\n");
    out += &format!(
        "  3. rcargo port expand {name}        (edge cases, answered by your original code)\n"
    );
    out += &format!("  4. Implement `call` in ports/{name}/src/port.rs (an AI agent can do this from ports/{name}/TASK.md)\n");
    out += &format!("  5. rcargo port verify {name}        (repeat until every case matches)\n");
    out += &format!("  6. rcargo port build {name}  then  rcargo port bench {name}\n");
    Ok((dir, out))
}

// ------------------------------------------------------------------ record / expand / bench

fn interpreter(lang: &str) -> Result<(String, String)> {
    let (cands, ext): (&[&str], &str) = match lang {
        "python" => (&["python3", "python"], "py"),
        "node" => (&["node"], "cjs"),
        "ruby" => (&["ruby"], "rb"),
        "php" => (&["php"], "php"),
        _ => bail!("`{lang}` has no ready-made shim: run the original over golden/candidates.jsonl yourself and append the answers to golden/cases.jsonl (see TASK.md)"),
    };
    for c in cands {
        if Command::new(c)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Ok((c.to_string(), ext.to_string()));
        }
    }
    bail!(
        "`{}` is not installed on this computer; it is needed to run your original code",
        cands[0]
    )
}

fn record(from: &Path, name: &str, command: &[String]) -> Result<i32> {
    let p = find_port(from, name)?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .env("RCARGO_PORT_RECORD", p.cases())
        .status()
        .with_context(|| format!("starting `{}`", command[0]))?;
    let (n, ..) = case_stats(&p.cases());
    println!(
        "rcargo port: {n} recorded call(s) in {}",
        p.cases().display()
    );
    if n == 0 {
        println!("Nothing recorded. Is the decorator/wrapper on the function, and does your command reach it with plain data (numbers, strings, lists, maps)?");
    }
    Ok(status.code().unwrap_or(1))
}

fn read_cases(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// (cases, error cases, total original nanoseconds)
fn case_stats(path: &Path) -> (usize, usize, u64) {
    let c = read_cases(path);
    let errs = c
        .iter()
        .filter(|c| c["expect"].get("err").is_some())
        .count();
    let ns = c.iter().map(|c| c["ns"].as_u64().unwrap_or(0)).sum();
    (c.len(), errs, ns)
}

/// (exit code, text for the user)
pub fn expand(from: &Path, name: &str, limit: usize) -> Result<(i32, String)> {
    let p = find_port(from, name)?;
    let cases = read_cases(&p.cases());
    if cases.is_empty() {
        bail!("no recorded calls yet: run `rcargo port record {name} -- <your command>` first (edge cases are derived from real ones)");
    }
    let seeds: Vec<Vec<Value>> = cases
        .iter()
        .filter_map(|c| c["args"].as_array().cloned())
        .collect();
    let existing: HashSet<String> = seeds
        .iter()
        .map(|a| serde_json::to_string(a).unwrap_or_default())
        .collect();
    let cands = derive_candidates(&seeds, &existing, limit);
    let cand_path = p.dir.join("golden/candidates.jsonl");
    let mut text = String::new();
    for a in &cands {
        text += &json!({"args": a}).to_string();
        text.push('\n');
    }
    std::fs::write(&cand_path, text)?;
    let mut out = format!(
        "derived {} candidate input(s) from {} recorded call(s)\n",
        cands.len(),
        seeds.len()
    );

    let ready = !p.meta.module.is_empty() && !p.meta.function.is_empty();
    let supported = SHIMS.iter().any(|s| s.0 == p.meta.lang);
    if !ready || !supported {
        out += &format!(
            "The original must answer them. Run it over {} and append one line per call to {} (format in TASK.md).\n",
            cand_path.display(),
            p.cases().display()
        );
        return Ok((0, out));
    }
    let (code, shim_out) = run_shim(
        &p,
        "oracle",
        &[&cand_path.to_string_lossy(), &p.cases().to_string_lossy()],
    )?;
    out += &shim_out;
    let _ = std::fs::remove_file(&cand_path);
    let (n, e, _) = case_stats(&p.cases());
    out += &format!("golden/cases.jsonl now has {n} case(s), {e} of them error cases\n");
    Ok((code, out))
}

fn run_shim(p: &Port, verb: &str, tail: &[&str]) -> Result<(i32, String)> {
    let (interp, ext) = interpreter(&p.meta.lang)?;
    if p.meta.module.is_empty() || p.meta.function.is_empty() {
        bail!("port.toml has no `module` / `function`; set them (ports/{}/port.toml) so the original can be loaded", p.meta.name);
    }
    let module = {
        let m = Path::new(&p.meta.module);
        if m.is_absolute() {
            m.to_path_buf()
        } else {
            p.root.join(m)
        }
    };
    let shim = p.dir.join(format!("shims/rcargo_port.{ext}"));
    let o = Command::new(interp)
        .arg(&shim)
        .arg(verb)
        .arg(&module)
        .arg(&p.meta.function)
        .args(tail)
        .current_dir(&p.root)
        .env("RCARGO_PORT_BIN", p.binary())
        .output()?;
    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
    text += &String::from_utf8_lossy(&o.stderr);
    Ok((o.status.code().unwrap_or(1), text))
}

pub fn bench(from: &Path, name: &str) -> Result<(i32, String)> {
    let p = find_port(from, name)?;
    let bin = p.binary();
    if !bin.is_file() {
        bail!(
            "{} does not exist: run `rcargo port build {name}` first",
            bin.display()
        );
    }
    run_shim(&p, "bench", &[&p.cases().to_string_lossy()])
}

// ------------------------------------------------------------------ edge case derivation

fn variants(v: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    match v {
        Value::Number(n) if n.is_f64() => {
            let x = n.as_f64().unwrap_or(0.0);
            for f in [0.0, -0.0, 1.0, -1.0, x * 2.0, -x, 1e-9, 1e12] {
                out.push(json!(f));
            }
        }
        Value::Number(n) => {
            let x = n.as_i64().unwrap_or(0);
            for i in [
                0,
                1,
                -1,
                x.saturating_add(1),
                x.saturating_sub(1),
                x.saturating_neg(),
                255,
                65536,
                i32::MAX as i64,
                i32::MIN as i64,
            ] {
                out.push(json!(i));
            }
        }
        Value::String(s) => {
            let chars: Vec<char> = s.chars().collect();
            for t in [
                "",
                " ",
                "a",
                "é",
                "héllo wörld",
                "日本語テキスト",
                "🙂 emoji 👍",
                "\n\t\r",
                "  padded  ",
            ] {
                out.push(json!(t));
            }
            if !chars.is_empty() {
                out.push(json!(chars[..1].iter().collect::<String>()));
                out.push(json!(chars[..chars.len() / 2].iter().collect::<String>()));
                out.push(json!(chars[chars.len() / 2..].iter().collect::<String>()));
                out.push(json!(chars.iter().rev().collect::<String>()));
                out.push(json!(s.to_uppercase()));
                out.push(json!(s.to_lowercase()));
                out.push(json!(format!("é日🙂{s}")));
            }
            if s.len() <= 4096 {
                out.push(json!(s.repeat(16)));
            }
        }
        Value::Bool(b) => out.push(json!(!b)),
        Value::Array(a) => {
            out.push(json!([]));
            if let Some(first) = a.first() {
                out.push(json!([first]));
                out.push(json!(a[..a.len() / 2]));
                out.push(json!(a.iter().rev().collect::<Vec<_>>()));
                out.push(json!(a[..a.len() - 1]));
                out.push(json!(a.iter().chain(a.iter()).collect::<Vec<_>>()));
                for m in variants(first).into_iter().take(3) {
                    let mut b = a.clone();
                    b[0] = m;
                    out.push(Value::Array(b));
                }
            }
        }
        Value::Object(o) => {
            out.push(json!({}));
            for k in o.keys().take(3) {
                let mut b = o.clone();
                b.remove(k);
                out.push(Value::Object(b));
            }
            if let Some((k, first)) = o.iter().next() {
                for m in variants(first).into_iter().take(3) {
                    let mut b = o.clone();
                    b.insert(k.clone(), m);
                    out.push(Value::Object(b));
                }
            }
        }
        Value::Null => {}
    }
    out.retain(|x| x != v && x.to_string().len() <= 64 * 1024);
    out
}

/// Inputs worth asking the original about: each argument of each recorded call replaced by
/// edge-case variants, one argument at a time. Round-robin over calls so a small `limit` still
/// spreads across the recorded inputs. Deterministic.
pub fn derive_candidates(
    seeds: &[Vec<Value>],
    existing: &HashSet<String>,
    limit: usize,
) -> Vec<Vec<Value>> {
    let mut rounds: Vec<Vec<Vec<Value>>> = vec![];
    for seed in seeds {
        for i in 0..seed.len() {
            for (k, v) in variants(&seed[i]).into_iter().enumerate() {
                if rounds.len() <= k {
                    rounds.resize(k + 1, vec![]);
                }
                let mut a = seed.clone();
                a[i] = v;
                rounds[k].push(a);
            }
        }
    }
    let mut seen = existing.clone();
    let mut out = vec![];
    'all: for r in rounds {
        for a in r {
            if seen.insert(serde_json::to_string(&a).unwrap_or_default()) {
                out.push(a);
                if out.len() >= limit {
                    break 'all;
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ status / crates

pub fn status(from: &Path, only: Option<&str>) -> Result<String> {
    let mut names: Vec<String> = vec![];
    for a in from.ancestors() {
        let d = a.join("ports");
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.path().join("port.toml").is_file() {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
            break;
        }
    }
    names.sort();
    if let Some(n) = only {
        names.retain(|x| x == n);
        if names.is_empty() {
            find_port(from, n)?;
        }
    }
    if names.is_empty() {
        return Ok(
            "No ports yet. Start with `rcargo port start` (it asks what you want to do).\n".into(),
        );
    }
    let mut s = String::new();
    for n in names {
        let p = find_port(from, &n)?;
        let (cases, errs, ns) = case_stats(&p.cases());
        let built = p.binary().is_file();
        s += &format!(
            "{:<16} {:<7} {:<24} cases {:<4} ({} errors, original {:.1}ms)  binary {}\n",
            n,
            p.meta.lang,
            if p.meta.function.is_empty() {
                "(no function set)"
            } else {
                &p.meta.function
            },
            cases,
            errs,
            ns as f64 / 1e6,
            if built { "built" } else { "not built" }
        );
    }
    Ok(s)
}

pub fn crates(query: &str, lang: Option<&str>) -> String {
    let q = query.trim().to_lowercase();
    let mut rows: Vec<(&str, &str, &str, &str)> = vec![];
    for l in T_CRATES
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 3 {
            continue;
        }
        let (lg, from, to, note) = (f[0], f[1], f[2], f.get(3).copied().unwrap_or(""));
        if let Some(want) = lang {
            if lg != want && lg != "any" {
                continue;
            }
        }
        if !q.is_empty()
            && !q.split_whitespace().all(|w| {
                format!("{lg} {from} {to} {note}")
                    .to_lowercase()
                    .contains(w)
            })
        {
            continue;
        }
        rows.push((lg, from, to, note));
    }
    if rows.is_empty() {
        return format!("Nothing matches `{query}`. Try a library name, or search https://crates.io and https://lib.rs.\n");
    }
    let mut s = String::new();
    for (lg, from, to, note) in rows {
        s += &format!(
            "{lg:<7} {from:<32} -> {to}{}\n",
            if note.is_empty() {
                String::new()
            } else {
                format!("   ({note})")
            }
        );
    }
    s
}

// ------------------------------------------------------------------ start: look first, then ask

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    /// Nothing there (or only a README/LICENSE): the user probably wants a new Rust project.
    Empty,
    /// A Rust project and nothing else.
    Rust,
    /// Code in another language: candidates for porting.
    Foreign,
    /// Rust next to other languages.
    Mixed,
    /// Files, but no recognised source code.
    Unknown,
}

#[derive(Debug)]
pub struct Survey {
    pub dir: PathBuf,
    pub kind: Kind,
    /// (language, source files), most files first. Rust is listed separately in `rust_files`.
    pub langs: Vec<(String, usize)>,
    pub rust_files: usize,
    pub has_cargo: bool,
    /// Largest source files of the main foreign language, project-relative.
    pub candidates: Vec<String>,
    pub ports: Vec<String>,
    pub entries: usize,
}

const SKIP_DIRS: [&str; 12] = [
    "node_modules",
    "target",
    "venv",
    ".venv",
    "__pycache__",
    "vendor",
    "dist",
    "build",
    ".git",
    "site-packages",
    "ports",
    ".idea",
];

pub fn survey(dir: &Path) -> Result<Survey> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let top: Vec<String> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    let boring = |n: &str| {
        let u = n.to_uppercase();
        u.starts_with("README") || u.starts_with("LICENSE") || u == "LICENCE"
    };
    let has_cargo = dir.join("Cargo.toml").is_file();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut sizes: BTreeMap<&'static str, Vec<(u64, String)>> = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(dir)
        .max_depth(Some(5))
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && e.depth() > 0
                && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        })
        .build();
    for (seen, e) in walker.flatten().enumerate() {
        if seen > 20_000 {
            break;
        }
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = e.path();
        let Some(lang) = path
            .extension()
            .and_then(|x| x.to_str())
            .and_then(lang_of_ext)
        else {
            continue;
        };
        *counts.entry(lang).or_default() += 1;
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let lower = rel.to_lowercase();
        if !(lower.contains("test")
            || lower.contains("spec")
            || lower.contains("__init__")
            || lower.ends_with(".d.ts")
            || lower.ends_with(".min.js"))
        {
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            sizes.entry(lang).or_default().push((len, rel));
        }
    }
    let rust_files = counts.remove("rust").unwrap_or(0);
    let mut langs: Vec<(String, usize)> = counts.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let candidates = langs
        .first()
        .and_then(|(l, _)| sizes.get(l.as_str()))
        .map(|v| {
            let mut v = v.clone();
            v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            v.into_iter().take(8).map(|x| x.1).collect()
        })
        .unwrap_or_default();
    let kind = if top.iter().all(|n| boring(n)) {
        Kind::Empty
    } else if !langs.is_empty() && (has_cargo || rust_files > 0) {
        Kind::Mixed
    } else if !langs.is_empty() {
        Kind::Foreign
    } else if has_cargo || rust_files > 0 {
        Kind::Rust
    } else {
        Kind::Unknown
    };
    let mut ports = vec![];
    if let Ok(rd) = std::fs::read_dir(dir.join("ports")) {
        for e in rd.flatten() {
            if e.path().join("port.toml").is_file() {
                ports.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        ports.sort();
    }
    Ok(Survey {
        dir: dir.to_path_buf(),
        kind,
        langs,
        rust_files,
        has_cargo,
        candidates,
        ports,
        entries: top.len(),
    })
}

fn profile_hint(lang: &str) -> &'static str {
    match lang {
        "python" => "python -m cProfile -s cumtime app.py | head -30",
        "node" => "node --cpu-prof app.js   (then open the .cpuprofile in Chrome DevTools > Performance)",
        "ruby" => "ruby -r stackprof ... or `gem install ruby-prof` then ruby-prof app.rb",
        "php" => "php -d xdebug.mode=profile app.php   (or the excimer extension)",
        "go" => "go test -bench . -cpuprofile cpu.out && go tool pprof -top cpu.out",
        "java" => "java -XX:StartFlightRecording=duration=30s,filename=rec.jfr App   (open with JDK Mission Control)",
        "csharp" => "dotnet-trace collect -- dotnet run -c Release",
        "c" | "cpp" => "perf record ./app && perf report",
        _ => "use your language's profiler on a real run",
    }
}

fn shim_note(lang: &str) -> &'static str {
    if SHIMS.iter().any(|s| s.0 == lang) {
        "ready-made recorder and bridge"
    } else {
        "Rust side and JSON protocol are ready; the recorder is a few lines you add (TASK.md explains)"
    }
}

impl Survey {
    pub fn main_lang(&self) -> Option<&str> {
        self.langs.first().map(|l| l.0.as_str())
    }

    /// The questions to put to the human. Same list for the CLI, MCP and the agent skill.
    pub fn questions(&self) -> Vec<Value> {
        let q = |id: &str, text: &str, options: &[&str]| json!({"id": id, "text": text, "options": options});
        match self.kind {
            Kind::Empty => vec![q(
                "goal",
                "This folder is empty. What do you want to do?",
                &[
                    "Start a new Rust project here (tell me what it should do)",
                    "Port slow code that lives in another folder to Rust (give me the path)",
                ],
            )],
            Kind::Rust => vec![
                q(
                    "goal",
                    "This is already a Rust project. What do you want to do?",
                    &[
                        "Just set up remote builds for it (rcargo init)",
                        "Port code from another folder/language into it (give me the path)",
                    ],
                ),
            ],
            Kind::Foreign | Kind::Mixed => vec![
                q(
                    "goal",
                    "I found existing code here. What do you want to do?",
                    &[
                        "Keep the app and port only its slow functions to Rust (recommended, fast to prove)",
                        "Start a separate new Rust project in a sub-folder (a full rewrite)",
                        "Nothing yet, I only wanted to look",
                    ],
                ),
                q(
                    "target",
                    "Which function is slow, in which file, and what command runs the app? Say \"I don't know\" and I will show you how to profile it.",
                    &[],
                ),
            ],
            Kind::Unknown => vec![q(
                "goal",
                "This folder has files but no source code I recognise. What is in it, and what do you want to build or port?",
                &[],
            )],
        }
    }

    pub fn render(&self) -> String {
        let mut s = format!("Looked at {}\n\n", self.dir.display());
        match self.kind {
            Kind::Empty => s += "It is empty (apart from README/LICENSE): a fresh start.\n",
            Kind::Rust => {
                s += &format!(
                    "A Rust project ({} .rs files{}).\n",
                    self.rust_files,
                    if self.has_cargo {
                        ", Cargo.toml present"
                    } else {
                        ""
                    }
                )
            }
            Kind::Unknown => {
                s += &format!(
                    "{} entries, but no source code I recognise.\n",
                    self.entries
                )
            }
            Kind::Foreign | Kind::Mixed => {
                s += "Source code found:\n";
                for (l, n) in &self.langs {
                    s += &format!("  {l:<7} {n:>4} file(s)   {}\n", shim_note(l));
                }
                if self.rust_files > 0 || self.has_cargo {
                    s += &format!(
                        "  rust    {:>4} file(s)   (already a Rust project too)\n",
                        self.rust_files
                    );
                }
                if !self.candidates.is_empty() {
                    s += "Biggest files (usual suspects for slow code):\n";
                    for c in &self.candidates {
                        s += &format!("  {c}\n");
                    }
                }
            }
        }
        if !self.ports.is_empty() {
            s += &format!(
                "Ports already here: {}  (see `rcargo port status`)\n",
                self.ports.join(", ")
            );
        }
        s
    }

    pub fn to_json(&self) -> Value {
        json!({
            "dir": self.dir,
            "kind": format!("{:?}", self.kind).to_lowercase(),
            "languages": self.langs.iter().map(|(l, n)| json!({"language": l, "files": n, "ready_made_shim": SHIMS.iter().any(|s| s.0 == l)})).collect::<Vec<_>>(),
            "rust_files": self.rust_files,
            "cargo_toml": self.has_cargo,
            "candidate_files": self.candidates,
            "ports": self.ports,
            "ask_user": self.questions(),
        })
    }

    /// What an AI agent must do with the survey.
    pub fn agent_text(&self) -> String {
        let mut s = self.render();
        s += "\nASK THE USER before doing anything else (do not choose for them):\n";
        for (i, q) in self.questions().iter().enumerate() {
            s += &format!("  {}. {}\n", i + 1, q["text"].as_str().unwrap_or(""));
            for o in q["options"].as_array().into_iter().flatten() {
                s += &format!("       - {}\n", o.as_str().unwrap_or(""));
            }
        }
        s += "\nThen:\n";
        s += "  * new Rust project: create it (Cargo.toml + src/main.rs), run `rcargo init`, build with `rcargo` only.\n";
        s += "  * port: `rcargo port init NAME --module FILE --function FUNC`, then follow ports/NAME/TASK.md and the rust-port skill.\n";
        if let Some(l) = self.main_lang() {
            s += &format!("  * to find the slow function: {}\n", profile_hint(l));
        }
        s += "  * Rust replacements for libraries: `rcargo port crates QUERY`.\n";
        s
    }
}

fn start(dir: Option<PathBuf>, as_json: bool) -> Result<i32> {
    let dir = match dir {
        Some(d) => d
            .canonicalize()
            .with_context(|| format!("cannot open {}", d.display()))?,
        None => std::env::current_dir()?,
    };
    let s = survey(&dir)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&s.to_json())?);
        return Ok(0);
    }
    print!("{}", s.render());
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        println!("\n(no terminal here: an AI agent should put these questions to the user)");
        print!(
            "{}",
            s.agent_text()
                .split_once("\nASK THE USER")
                .map(|x| format!("\nASK THE USER{}", x.1))
                .unwrap_or_default()
        );
        return Ok(0);
    }
    interactive(s)
}

fn prompt(text: &str, default: &str) -> String {
    if default.is_empty() {
        print!("{text}: ");
    } else {
        print!("{text} [{default}]: ");
    }
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    let t = line.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

fn choose(text: &str, options: &[&str]) -> usize {
    println!("\n{text}");
    for (i, o) in options.iter().enumerate() {
        println!("  {}) {o}", i + 1);
    }
    loop {
        let a = prompt("Your choice", "1");
        if let Ok(n) = a.parse::<usize>() {
            if (1..=options.len()).contains(&n) {
                return n - 1;
            }
        }
        println!("Please type a number from 1 to {}.", options.len());
    }
}

fn interactive(s: Survey) -> Result<i32> {
    let qs = s.questions();
    let q0 = &qs[0];
    let options: Vec<&str> = q0["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if options.is_empty() {
        println!("\n{}", q0["text"].as_str().unwrap_or(""));
        println!("Tell your AI agent, or run `rcargo port init --help` once you know.");
        return Ok(0);
    }
    let pick = choose(q0["text"].as_str().unwrap_or(""), &options);
    match (s.kind, pick) {
        (Kind::Empty, 0) => new_project(&s.dir, true),
        (Kind::Rust, 0) => {
            std::env::set_current_dir(&s.dir)?;
            init::init()
        }
        (Kind::Empty | Kind::Rust, _) => {
            let other = prompt("Path of the folder that holds the code to port", "");
            if other.is_empty() {
                return Ok(0);
            }
            start(Some(PathBuf::from(other)), false)
        }
        (_, 0) => port_wizard(&s),
        (_, 1) => {
            let sub = prompt("Name of the new sub-folder", "rust-app");
            new_project(&s.dir.join(sub), false)
        }
        _ => Ok(0),
    }
}

fn port_wizard(s: &Survey) -> Result<i32> {
    let default_lang = s.main_lang().unwrap_or("other");
    let lang = prompt(
        "Language of the code to port (python, node, ruby, php, other)",
        default_lang,
    );
    let module = if s.candidates.is_empty() {
        prompt("Source file with the slow function", "")
    } else {
        println!("\nSource file with the slow function (a number, or type a path):");
        for (i, c) in s.candidates.iter().enumerate() {
            println!("  {}) {c}", i + 1);
        }
        let a = prompt("File", "");
        match a.parse::<usize>() {
            Ok(n) if (1..=s.candidates.len()).contains(&n) => s.candidates[n - 1].clone(),
            _ => a,
        }
    };
    let function = prompt("Function name (empty = I do not know yet)", "");
    if function.is_empty() {
        println!(
            "\nFind the slow function first, from a real run:\n  {}",
            profile_hint(&lang)
        );
        println!("Then run `rcargo port start` again, or: rcargo port init NAME --module FILE --function FUNC");
        return Ok(0);
    }
    let base: String = function
        .rsplit(['.', ':'])
        .next()
        .unwrap_or("port")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let name = prompt("Name for this port", &base);
    let (_, text) = init_port(
        &s.dir,
        &name,
        Some(&lang),
        if module.is_empty() {
            None
        } else {
            Some(Path::new(&module))
        },
        Some(&function),
    )?;
    print!("{text}");
    Ok(0)
}

/// A minimal binary crate. Never overwrites: refuses when a Cargo.toml exists.
pub fn new_project(dir: &Path, offer_init: bool) -> Result<i32> {
    if dir.join("Cargo.toml").exists() {
        bail!("{} already has a Cargo.toml", dir.display());
    }
    std::fs::create_dir_all(dir.join("src"))?;
    let raw = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "app".into());
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        name = format!("app-{name}");
    }
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n"),
    )?;
    let main = dir.join("src/main.rs");
    if !main.exists() {
        std::fs::write(main, "fn main() {\n    println!(\"Hello from Rust\");\n}\n")?;
    }
    let gi = dir.join(".gitignore");
    if !gi.exists() {
        std::fs::write(gi, "/target\n")?;
    }
    println!("Created a Rust project `{name}` in {}", dir.display());
    if offer_init {
        std::env::set_current_dir(dir)?;
        return init::init();
    }
    println!("Next: cd {} && rcargo init", dir.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rcargo-port-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn empty_and_readme_only_folders_are_a_fresh_start() {
        let d = tmp("empty");
        assert_eq!(survey(&d).unwrap().kind, Kind::Empty);
        fs::write(d.join("README.md"), "hi").unwrap();
        fs::write(d.join("LICENSE"), "mit").unwrap();
        assert_eq!(survey(&d).unwrap().kind, Kind::Empty);
    }

    #[test]
    fn existing_python_project_is_inspected_not_assumed_new() {
        let d = tmp("py");
        fs::create_dir_all(d.join("app")).unwrap();
        fs::create_dir_all(d.join("node_modules/x")).unwrap();
        fs::write(
            d.join("app/fuzzy.py"),
            "def f(a):\n    return a\n".repeat(50),
        )
        .unwrap();
        fs::write(d.join("app/small.py"), "x = 1\n").unwrap();
        fs::write(
            d.join("app/test_fuzzy.py"),
            "def test(): pass\n".repeat(200),
        )
        .unwrap();
        fs::write(d.join("node_modules/x/index.js"), "1").unwrap();
        let s = survey(&d).unwrap();
        assert_eq!(s.kind, Kind::Foreign);
        assert_eq!(s.langs, vec![("python".to_string(), 3)]);
        assert_eq!(s.candidates[0], "app/fuzzy.py");
        assert!(!s.candidates.iter().any(|c| c.contains("test_")));
        assert!(s.agent_text().contains("ASK THE USER"));
        assert!(s.agent_text().contains("cProfile"));
    }

    #[test]
    fn rust_only_and_mixed_are_told_apart() {
        let d = tmp("rs");
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
        assert_eq!(survey(&d).unwrap().kind, Kind::Rust);
        fs::write(d.join("tool.rb"), "puts 1").unwrap();
        assert_eq!(survey(&d).unwrap().kind, Kind::Mixed);
    }

    #[test]
    fn new_project_never_overwrites() {
        let d = tmp("new");
        new_project(&d.join("hello app"), false).unwrap();
        let toml = fs::read_to_string(d.join("hello app/Cargo.toml")).unwrap();
        assert!(toml.contains("name = \"hello-app\""));
        assert!(new_project(&d.join("hello app"), false).is_err());
    }

    #[test]
    fn port_names_are_validated() {
        assert!(valid_name("fuzzy_dedupe"));
        assert!(!valid_name("Fuzzy"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(""));
    }

    #[test]
    fn derived_cases_cover_edges_and_are_deterministic() {
        let seeds = vec![
            vec![json!("héllo"), json!(3)],
            vec![json!(["a", "b"]), json!(1.5)],
        ];
        let a = derive_candidates(&seeds, &HashSet::new(), 1000);
        assert_eq!(a, derive_candidates(&seeds, &HashSet::new(), 1000));
        let keys: HashSet<String> = a
            .iter()
            .map(|x| serde_json::to_string(x).unwrap())
            .collect();
        assert_eq!(keys.len(), a.len(), "no duplicates");
        assert!(keys.contains("[\"\",3]"), "empty string");
        assert!(keys.contains("[\"héllo\",0]"), "zero");
        assert!(keys.contains("[\"héllo\",-3]"), "negation");
        assert!(keys.contains("[[],1.5]"), "empty list");
        assert!(keys.iter().any(|k| k.contains("日本語")), "non-ascii");
        assert!(
            !keys.contains("[\"héllo\",3]"),
            "the original input is not repeated"
        );
        let small = derive_candidates(&seeds, &HashSet::new(), 4);
        assert_eq!(small.len(), 4);
        assert!(small
            .iter()
            .any(|x| x[0] != json!("héllo") && x[0].is_string()));
        assert!(
            small.iter().any(|x| x[0].is_array()),
            "limit still reaches the second recorded call"
        );
    }

    #[test]
    fn crates_lookup_filters_by_language_and_words() {
        let r = crates("requests", None);
        assert!(r.contains("python") || r.contains("any"), "{r}");
        let py = crates("pandas", Some("python"));
        assert!(py.contains("polars"));
        assert!(crates("express", Some("python")).starts_with("Nothing matches"));
        assert!(crates("http client", Some("node")).contains("reqwest"));
        assert!(crates("zzzz-nothing", None).starts_with("Nothing matches"));
    }

    #[test]
    fn init_creates_a_standalone_crate_and_shim() {
        let d = tmp("init");
        fs::write(d.join("fuzzy.py"), "def lev(a, b):\n    return 0\n").unwrap();
        let dir = init_port(&d, "lev", None, Some(Path::new("fuzzy.py")), Some("lev"))
            .unwrap()
            .0;
        for f in [
            "Cargo.toml",
            "src/lib.rs",
            "src/port.rs",
            "src/bridge.rs",
            "src/main.rs",
            "tests/golden.rs",
            "golden/cases.jsonl",
            "TASK.md",
            "port.toml",
            "shims/rcargo_port.py",
            ".gitignore",
        ] {
            assert!(dir.join(f).is_file(), "missing {f}");
        }
        let toml = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(toml.contains("name = \"lev-port\"") && toml.contains("[workspace]"));
        let shim = fs::read_to_string(dir.join("shims/rcargo_port.py")).unwrap();
        assert!(shim.contains("lev-port") && !shim.contains("{{"));
        let p = find_port(&d, "lev").unwrap();
        assert_eq!(p.meta.lang, "python");
        assert_eq!(p.meta.module, "fuzzy.py");
        assert!(
            init_port(&d, "lev", Some("python"), None, None).is_err(),
            "never overwrites"
        );
        assert!(init_port(&d, "x", Some("cobol"), None, None).is_err());
    }
}
