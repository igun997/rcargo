use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::backend;
use crate::config::{self, Loaded};
use crate::run::{self, Cmd, ExecSpec, Stream};
use crate::summary;
use crate::util::{self, shq};
use crate::{InstancesCmd, RemoteCmd};

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().context("cannot determine current directory")
}

// ------------------------------------------------------------------ cargo passthrough

pub fn cargo(instance: Option<&str>, args: Vec<String>, pull: bool, no_sync: bool) -> Result<i32> {
    if args.is_empty() {
        bail!("nothing to run; try `rcargo test` or `rcargo --help`");
    }
    let cwd = cwd()?;
    let loaded = config::load(&cwd)?;
    let sess = run::open(&loaded, instance, &cwd)?;

    let want_artifacts = pull && summary::produces_artifacts(&args);
    let (args, hide_json) = if want_artifacts {
        summary::inject_message_format(args, "json-render-diagnostics")
    } else {
        (args, false)
    };
    let color = std::io::stderr().is_terminal() && !args.iter().any(|a| a.starts_with("--color"));
    let mut artifacts: Vec<PathBuf> = vec![];
    let res = sess.exec(
        &ExecSpec {
            cmd: Cmd::Cargo(args),
            sync: !no_sync,
            color,
        },
        &mut |stream, line| match stream {
            Stream::Out => {
                if want_artifacts {
                    if let Some(p) = summary::artifact_of_line(line) {
                        artifacts.push(p);
                    }
                    if hide_json && line.starts_with(b"{\"reason\":") {
                        return;
                    }
                }
                let _ = std::io::stdout().write_all(line);
            }
            Stream::Err => {
                let _ = std::io::stderr().write_all(line);
            }
        },
    )?;
    if res.code != 0 {
        let h = run::hint_for(res.code);
        if !h.is_empty() {
            info!("{}", h.trim_start());
        }
    }
    if res.code == 0 && want_artifacts {
        sess.pull(&artifacts)?;
    }
    info!(
        "exit {} in {:.1}s on `{}`",
        res.code, res.total_secs, sess.inst_name
    );
    Ok(res.code)
}

// ------------------------------------------------------------------ instances

pub struct Live {
    pub state: String,
    pub detail: String,
}

pub fn live_status(loaded: &Loaded) -> Vec<(config::InstanceRow, Live)> {
    let rows = loaded.rows();
    let mut out: Vec<Option<Live>> = (0..rows.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = rows
            .iter()
            .map(|r| {
                s.spawn(
                    move || match backend::make(&r.name, &r.inst).and_then(|b| b.status()) {
                        Ok(st) => Live {
                            state: st.state,
                            detail: st.detail,
                        },
                        Err(e) => Live {
                            state: "error".into(),
                            detail: format!("{e:#}").lines().next().unwrap_or("").to_string(),
                        },
                    },
                )
            })
            .collect();
        for (i, h) in handles.into_iter().enumerate() {
            out[i] = h.join().ok();
        }
    });
    rows.into_iter()
        .zip(out)
        .map(|(r, l)| {
            (
                r,
                l.unwrap_or(Live {
                    state: "error".into(),
                    detail: "status check panicked".into(),
                }),
            )
        })
        .collect()
}

pub fn instances(action: InstancesCmd) -> Result<i32> {
    let cwd = cwd()?;
    match action {
        InstancesCmd::List { json, offline } => {
            let loaded = config::load(&cwd)?;
            if loaded.cfg.instances.is_empty() {
                println!("No instances configured.\n\nAdd one, for example:\n  rcargo instances add main --backend instabox --description \"general purpose\"\n  rcargo instances add big  --backend ssh --set host=me@build.example.com --description \"32-core VPS for release builds\"\n  rcargo instances add dt   --backend daytona --description \"Daytona sandbox\"");
                return Ok(0);
            }
            let rows: Vec<(config::InstanceRow, Live)> = if offline {
                loaded
                    .rows()
                    .into_iter()
                    .map(|r| {
                        (
                            r,
                            Live {
                                state: "unknown".into(),
                                detail: String::new(),
                            },
                        )
                    })
                    .collect()
            } else {
                live_status(&loaded)
            };
            if json {
                let v: Vec<Value> = rows
                    .iter()
                    .map(|(r, l)| {
                        json!({"name": r.name, "backend": r.inst.backend, "default": r.is_default,
                               "description": r.inst.description, "state": l.state, "detail": l.detail})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_instances(&rows));
                if loaded.is_implicit() {
                    println!("\n(no config file: using the implicit instabox default. `rcargo instances add` to configure your own.)");
                }
            }
            Ok(0)
        }
        InstancesCmd::Add {
            name,
            backend,
            description,
            set,
            default,
        } => {
            if !config::BACKENDS.contains(&backend.as_str()) {
                bail!("backend must be one of: {}", config::BACKENDS.join(", "));
            }
            let mut t = toml::value::Table::new();
            t.insert("backend".into(), toml::Value::String(backend));
            if let Some(d) = description {
                t.insert("description".into(), toml::Value::String(d));
            }
            for kv in &set {
                let (k, v) = config::parse_kv(kv)?;
                t.insert(k, v);
            }
            toml::Value::Table(t.clone())
                .try_into::<config::Instance>()
                .context("invalid instance settings")?;
            let path = config::edit_global(|root| {
                let insts = root
                    .entry("instances")
                    .or_insert_with(|| toml::Value::Table(Default::default()))
                    .as_table_mut()
                    .context("`instances` is not a table")?;
                insts.insert(name.clone(), toml::Value::Table(t));
                if default {
                    root.insert("default_instance".into(), toml::Value::String(name.clone()));
                }
                Ok(())
            })?;
            println!(
                "Added instance `{name}` to {}{}",
                path.display(),
                if default { " (default)" } else { "" }
            );
            Ok(0)
        }
        InstancesCmd::Rm { name } => {
            let mut found = false;
            let path = config::edit_global(|root| {
                if let Some(i) = root.get_mut("instances").and_then(|v| v.as_table_mut()) {
                    found = i.remove(&name).is_some();
                }
                if root.get("default_instance").and_then(|v| v.as_str()) == Some(name.as_str()) {
                    root.remove("default_instance");
                }
                Ok(())
            })?;
            if !found {
                bail!("instance `{name}` is not in {}", path.display());
            }
            println!(
                "Removed `{name}` from {} (the machine itself was not touched).",
                path.display()
            );
            Ok(0)
        }
        InstancesCmd::Use { name } => {
            let loaded = config::load(&cwd)?;
            if !loaded.cfg.instances.contains_key(&name) {
                bail!("unknown instance `{name}`");
            }
            let path = config::edit_global(|root| {
                root.insert("default_instance".into(), toml::Value::String(name.clone()));
                Ok(())
            })?;
            println!("Default instance is now `{name}` ({}).", path.display());
            Ok(0)
        }
        InstancesCmd::Discover => {
            let loaded = config::load(&cwd)?;
            let known_box: Vec<String> = loaded
                .cfg
                .instances
                .iter()
                .filter(|(_, i)| i.backend == "instabox")
                .map(|(n, i)| i.box_name.clone().unwrap_or_else(|| n.clone()))
                .collect();
            let mut any = false;
            match backend::instabox::list_boxes() {
                Ok(boxes) => {
                    for (n, state, spec) in boxes {
                        if known_box.contains(&n) {
                            continue;
                        }
                        any = true;
                        println!("instabox  {n:<24} {state:<10} {spec}\n  rcargo instances add {n} --backend instabox --set box={n} --description \"...\"");
                    }
                }
                Err(e) => println!("instabox: {e:#}"),
            }
            if std::env::var_os("DAYTONA_API_KEY").is_some() {
                let known: Vec<String> = loaded
                    .cfg
                    .instances
                    .iter()
                    .filter(|(_, i)| i.backend == "daytona")
                    .map(|(n, i)| i.name.clone().unwrap_or_else(|| n.clone()))
                    .collect();
                match backend::daytona::list_sandboxes() {
                    Ok(sbs) => {
                        for (n, state) in sbs {
                            if known.contains(&n) {
                                continue;
                            }
                            any = true;
                            println!("daytona   {n:<24} {state:<10}\n  rcargo instances add {n} --backend daytona --description \"...\"");
                        }
                    }
                    Err(e) => println!("daytona: {e:#}"),
                }
            }
            if !any {
                println!("Nothing new found.");
            }
            Ok(0)
        }
    }
}

pub fn render_instances(rows: &[(config::InstanceRow, Live)]) -> String {
    let mut s = format!(
        "{:<14} {:<9} {:<12} {:<4} {}\n",
        "NAME", "BACKEND", "STATE", "DEF", "DETAILS"
    );
    for (r, l) in rows {
        s += &format!(
            "{:<14} {:<9} {:<12} {:<4} {}\n",
            r.name,
            r.inst.backend,
            l.state,
            if r.is_default { "*" } else { "" },
            l.detail
        );
        if let Some(d) = &r.inst.description {
            s += &format!("{:<14} {d}\n", "");
        }
    }
    s
}

// ------------------------------------------------------------------ remote

const DOCTOR_SCRIPT: &str = r#"
for t in cargo rustc cc tar flock find bash; do
  if command -v "$t" >/dev/null 2>&1; then printf '%s\tok\t%s\n' "$t" "$(command -v "$t")"; else printf '%s\tMISSING\t\n' "$t"; fi
done
printf 'cargo-version\t%s\n' "$(cargo --version 2>/dev/null || echo none)"
printf 'rustc-version\t%s\n' "$(rustc --version 2>/dev/null || echo none)"
printf 'cpus\t%s\n' "$(nproc 2>/dev/null || echo ?)"
printf 'mem-mib\t%s\n' "$(free -m 2>/dev/null | awk '/Mem:/{print $2}')"
printf 'disk-free\t%s\n' "$(df -h "$HOME" 2>/dev/null | awk 'NR==2{print $4" free of "$2}')"
"#;

fn instance_backend(
    loaded: &Loaded,
    requested: Option<&str>,
) -> Result<(String, Box<dyn backend::Backend>)> {
    let (name, inst) = loaded.resolve(requested)?;
    let b = backend::make(&name, &inst)?;
    Ok((name, b))
}

pub fn remote(instance: Option<&str>, action: RemoteCmd) -> Result<i32> {
    let cwd = cwd()?;
    let loaded = config::load(&cwd)?;
    match action {
        RemoteCmd::Up => {
            let (name, b) = instance_backend(&loaded, instance)?;
            let ssh = b.ensure()?;
            ssh.ensure_master();
            let st = b.status()?;
            println!("`{name}` is {} ({})", st.state, st.detail);
            Ok(0)
        }
        RemoteCmd::Down => {
            let (name, b) = instance_backend(&loaded, instance)?;
            b.stop()?;
            println!(
                "`{name}` stopped/suspended. It resumes automatically on the next rcargo command."
            );
            Ok(0)
        }
        RemoteCmd::Status => {
            let (name, b) = instance_backend(&loaded, instance)?;
            let st = b.status()?;
            println!(
                "instance:  {name}\nstate:     {}\ndetails:   {}",
                st.state, st.detail
            );
            if let Ok(p) = crate::project::discover(&cwd) {
                println!("project:   {} (workspace id {})", p.root.display(), p.id);
            }
            Ok(0)
        }
        RemoteCmd::Doctor => doctor(&loaded, instance, &cwd),
        RemoteCmd::Setup => {
            let (name, b) = instance_backend(&loaded, instance)?;
            let ssh = b.ensure()?;
            info!("bootstrapping `{name}`");
            let (code, out, err) = ssh.capture(run::SETUP_SCRIPT, None)?;
            print!("{}", String::from_utf8_lossy(&out));
            eprint!("{}", String::from_utf8_lossy(&err));
            Ok(code)
        }
        RemoteCmd::Sync => {
            let sess = run::open(&loaded, instance, &cwd)?;
            let r = sess.sync_only()?;
            println!(
                "synced {} of {} files ({}), {} removed, in {:.1}s",
                r.sync.files_sent,
                r.sync.files_total,
                util::human_bytes(r.sync.bytes_sent),
                r.sync.deleted,
                r.total_secs
            );
            Ok(r.code)
        }
        RemoteCmd::Shell { command } => {
            let sess = run::open(&loaded, instance, &cwd)?;
            if command.is_empty() {
                sess.sync_only()?;
                let script = format!(
                    "cd {}/{} && exec bash -l",
                    sess.base_expr(),
                    shq(&crate::sync::mirror_of(&sess.project.root)?)
                );
                return sess.ssh.interactive(&script);
            }
            let cmdline = command.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
            let res = sess.exec(
                &ExecSpec {
                    cmd: Cmd::Shell(cmdline),
                    sync: true,
                    color: std::io::stderr().is_terminal(),
                },
                &mut |s, l| {
                    let _ = match s {
                        Stream::Out => std::io::stdout().write_all(l),
                        Stream::Err => std::io::stderr().write_all(l),
                    };
                },
            )?;
            Ok(res.code)
        }
        RemoteCmd::Ls => {
            let (_, inst) = loaded.resolve(instance)?;
            let (_, b) = instance_backend(&loaded, instance)?;
            let ssh = b.ensure()?;
            let out = ssh.capture_ok(&run::workspaces_script(inst.base.as_deref()))?;
            let ws = run::parse_workspaces(&out);
            if ws.is_empty() {
                println!("No workspaces on this instance yet.");
            }
            let now = util::now_secs();
            let mut total = 0;
            for w in &ws {
                total += w.kib;
                let age = if w.last_used > 0 {
                    format!("{}d ago", (now - w.last_used) / 86400)
                } else {
                    "never".into()
                };
                println!(
                    "{:<10} {:<12} {}\n{:<10} {:<12} {}",
                    util::human_bytes(w.kib * 1024),
                    age,
                    w.origin,
                    "",
                    "",
                    w.id
                );
            }
            if !ws.is_empty() {
                println!("total {}", util::human_bytes(total * 1024));
            }
            Ok(0)
        }
        RemoteCmd::Gc { days, all, yes } => {
            let (_, inst) = loaded.resolve(instance)?;
            let (name, b) = instance_backend(&loaded, instance)?;
            let ssh = b.ensure()?;
            let out = ssh.capture_ok(&run::workspaces_script(inst.base.as_deref()))?;
            let now = util::now_secs();
            let victims: Vec<_> = run::parse_workspaces(&out)
                .into_iter()
                .filter(|w| all || w.last_used == 0 || now - w.last_used > days as i64 * 86400)
                .filter(|w| {
                    w.id.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                })
                .collect();
            if victims.is_empty() {
                println!("Nothing to remove on `{name}`.");
                return Ok(0);
            }
            let root = match inst.base.as_deref() {
                Some(b) => shq(b.trim_end_matches('/')),
                None => "\"$HOME/.rcargo/ws\"".into(),
            };
            for w in &victims {
                println!(
                    "{} {:<10} {}",
                    if yes { "removing" } else { "would remove" },
                    util::human_bytes(w.kib * 1024),
                    w.origin
                );
                if yes {
                    ssh.capture_ok(&format!("rm -rf -- {root}/{}", shq(&w.id)))?;
                }
            }
            if !yes {
                println!("\nRe-run with --yes to delete.");
            }
            Ok(0)
        }
        RemoteCmd::Rm => {
            let sess = run::open(&loaded, instance, &cwd)?;
            sess.ssh
                .capture_ok(&format!("rm -rf -- {}", sess.base_expr()))?;
            println!(
                "Removed remote workspace {} on `{}`.",
                sess.project.id, sess.inst_name
            );
            Ok(0)
        }
    }
}

fn doctor(loaded: &Loaded, instance: Option<&str>, cwd: &Path) -> Result<i32> {
    let mut bad = 0;
    let mut line = |ok: bool, what: &str, extra: &str| {
        println!(
            "[{}] {what}{}",
            if ok { " ok " } else { "FAIL" },
            if extra.is_empty() {
                String::new()
            } else {
                format!(" — {extra}")
            }
        );
        if !ok {
            bad += 1;
        }
    };
    line(util::on_path("ssh"), "local ssh client", "");
    line(util::on_path("tar"), "local tar", "");
    println!(
        "       config: user {} {}; project {}",
        loaded.global_file.display(),
        if loaded.global_file.is_file() {
            ""
        } else {
            "(not created yet)"
        },
        loaded
            .project_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none".into())
    );
    let (name, inst) = loaded.resolve(instance)?;
    match inst.backend.as_str() {
        "instabox" => line(
            util::on_path("instabox"),
            "instabox CLI",
            "npm install -g instabox && instabox login",
        ),
        "daytona" => {
            line(
                std::env::var_os("DAYTONA_API_KEY").is_some(),
                "DAYTONA_API_KEY set",
                "",
            );
            line(util::on_path("curl"), "curl", "");
        }
        _ => {}
    }
    if let Ok(p) = crate::project::discover(cwd) {
        let mut w = vec![];
        let roots = crate::project::collect_roots(&p, &loaded.cfg.sync, &mut w)?;
        let files: usize = roots.iter().map(|r| r.entries.len()).sum();
        let bytes: u64 = roots
            .iter()
            .flat_map(|r| r.entries.iter())
            .map(|e| e.size)
            .sum();
        line(
            true,
            "project",
            &format!(
                "{} ({} root(s), {files} files, {})",
                p.root.display(),
                roots.len(),
                util::human_bytes(bytes)
            ),
        );
        for r in roots.iter().skip(1) {
            println!("       + path dependency root synced: {}", r.path.display());
        }
        for x in w {
            println!("       warning: {x}");
        }
        if bytes > 200 * 1024 * 1024 {
            println!("       warning: more than 200 MiB of sources; add big data dirs to `sync.exclude` in rcargo.toml");
        }
        let cc = p.root.join(".cargo/config.toml");
        if let Ok(s) = std::fs::read_to_string(&cc) {
            if s.contains("linker") || s.contains("rustflags") || s.contains("target-dir") {
                println!("       warning: .cargo/config.toml sets linker/rustflags/target-dir; these also apply on the remote and may not exist there");
            }
        }
    }
    let b = backend::make(&name, &inst)?;
    let st = match b.status() {
        Ok(s) => s,
        Err(e) => {
            line(false, &format!("instance `{name}`"), &format!("{e:#}"));
            return Ok(1);
        }
    };
    line(
        st.state != "error",
        &format!("instance `{name}` ({}) is {}", inst.backend, st.state),
        &st.detail,
    );
    let ssh = match b.ensure() {
        Ok(s) => s,
        Err(e) => {
            line(false, "reach instance", &format!("{e:#}"));
            return Ok(1);
        }
    };
    line(true, "reach instance over ssh", "");
    let out = ssh.capture_ok(DOCTOR_SCRIPT)?;
    for l in out.lines() {
        let f: Vec<&str> = l.splitn(3, '\t').collect();
        match f.as_slice() {
            [t, "ok", p] => line(true, &format!("remote {t}"), p),
            [t, "MISSING", _] => line(
                false,
                &format!("remote {t}"),
                if *t == "cargo" || *t == "rustc" || *t == "cc" {
                    "run `rcargo remote setup`"
                } else {
                    "install it on the remote"
                },
            ),
            [k, v] => println!("       {k}: {v}"),
            _ => {}
        }
    }
    println!(
        "\n{}",
        if bad == 0 {
            "All good."
        } else {
            "Some checks failed."
        }
    );
    Ok(if bad == 0 { 0 } else { 1 })
}
