//! Orchestration: pick instance → make it reachable → delta sync → run cargo/shell remotely,
//! streaming output with remote paths mapped back to local paths.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Instant;

use crate::backend;
use crate::config::{Instance, Loaded, RemoteCfg, SyncCfg};
use crate::project::{self, Project};
use crate::sync::{self, Root, SyncStats};
use crate::transport::{exit_code, Ssh};
use crate::util::{self, shq, strip_all};

pub struct Session {
    pub inst_name: String,
    pub inst: Instance,
    pub sync_cfg: SyncCfg,
    pub remote_cfg: RemoteCfg,
    pub project: Project,
    pub ssh: Ssh,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Out,
    Err,
}

pub enum Cmd {
    Cargo(Vec<String>),
    Shell(String),
}

pub struct ExecSpec {
    pub cmd: Cmd,
    pub sync: bool,
    pub color: bool,
}

pub struct ExecResult {
    pub code: i32,
    pub sync: SyncStats,
    pub sync_secs: f64,
    pub total_secs: f64,
}

/// Resolve the instance, make it reachable, scan the project.
pub fn open(loaded: &Loaded, requested: Option<&str>, cwd: &Path) -> Result<Session> {
    let (inst_name, inst) = loaded.resolve(requested)?;
    let project = project::discover(cwd)?;
    let backend = backend::make(&inst_name, &inst)?;
    info!("instance `{inst_name}` ({})", inst.backend);
    let ssh = backend.ensure()?;
    ssh.ensure_master();
    let mut warnings = vec![];
    let roots = project::collect_roots(&project, &loaded.cfg.sync, &mut warnings)?;
    Ok(Session {
        inst_name,
        inst,
        sync_cfg: loaded.cfg.sync.clone(),
        remote_cfg: loaded.cfg.remote.clone(),
        project,
        ssh,
        roots,
        warnings,
    })
}

impl Session {
    pub fn base_expr(&self) -> String {
        sync::base_expr(self.inst.base.as_deref(), &self.project.id)
    }

    fn mirror_cwd(&self) -> String {
        let m = sync::mirror_of(&self.project.root).unwrap_or_default();
        if self.project.cwd_rel.is_empty() {
            m
        } else {
            format!("{m}/{}", self.project.cwd_rel)
        }
    }

    fn env_block(&self, color: bool, extra: &[(String, String)]) -> Result<String> {
        let mut vars: Vec<(String, String)> = vec![("RCARGO".into(), "1".into())];
        vars.push((
            "CARGO_TERM_COLOR".into(),
            if color { "always" } else { "never" }.into(),
        ));
        for (k, v) in self.inst.env.iter().chain(self.remote_cfg.env.iter()) {
            vars.push((k.clone(), v.clone()));
        }
        for k in self
            .inst
            .forward_env
            .iter()
            .chain(self.remote_cfg.forward_env.iter())
        {
            match std::env::var(k) {
                Ok(v) => vars.push((k.clone(), v)),
                Err(_) => info!("forward_env `{k}` is not set locally; skipping"),
            }
        }
        vars.extend(extra.iter().cloned());
        let mut s = String::new();
        for (k, v) in vars {
            if !util::valid_env_key(&k) {
                bail!("invalid environment variable name `{k}`");
            }
            s += &format!("export {k}={}\n", shq(&v));
        }
        Ok(s)
    }

    fn remote_script(&self, cmd: &Cmd) -> String {
        let origin = shq(&self.project.root.to_string_lossy());
        let exec_line = match cmd {
            Cmd::Cargo(args) => {
                let cargo = self.remote_cfg.cargo.as_deref().unwrap_or("cargo");
                let a: Vec<String> = args.iter().map(|a| shq(a)).collect();
                format!("exec {} {}", shq(cargo), a.join(" "))
            }
            Cmd::Shell(c) => format!("exec bash -lc {}", shq(c)),
        };
        format!(
            r#"set -e
IFS= read -r __n
eval "$(head -c "$__n")"
B={base}
mkdir -p "$B/.state"
exec 9>"$B/.state/lock"
if ! flock -w 1800 9; then echo "rcargo: gave up waiting for the remote workspace lock" >&2; exit 75; fi
cd "$B"
tar -xf - --no-same-owner
if [ -f .state/deletions ]; then
  while IFS= read -r -d '' p; do rm -f -- "$B/$p"; done < .state/deletions
  rm -f .state/deletions
fi
printf '%s\n' {origin} > .state/origin
touch .state/last-used
cd "$B"/{cwd}
{exec_line}
"#,
            base = self.base_expr(),
            cwd = shq(&self.mirror_cwd()),
        )
    }

    /// Install a C toolchain and Rust on the remote if missing. Idempotent.
    pub fn bootstrap(&self) -> Result<()> {
        let (code, out, err) = self.ssh.capture(SETUP_SCRIPT, None)?;
        if code != 0 {
            info!("{}", String::from_utf8_lossy(&err).trim());
            return Err(crate::guidance::toolchain_missing(&self.inst_name).into_err());
        }
        for l in String::from_utf8_lossy(&out).lines() {
            info!("remote: {l}");
        }
        Ok(())
    }

    /// Ask the remote what it already has (one ssh round trip).
    fn query(&self, with_find: bool) -> Result<sync::RemoteState> {
        let cargo = self.remote_cfg.cargo.as_deref().unwrap_or("cargo");
        let script = sync::query_script(
            &self.base_expr(),
            cargo,
            &self.roots,
            self.sync_cfg.include_git,
            with_find,
        );
        let (code, out, err) = self.ssh.capture(&script, None)?;
        if code != 0 {
            bail!(
                "remote query failed (exit {code}): {}{}",
                String::from_utf8_lossy(&err).trim(),
                hint_for(code)
            );
        }
        sync::parse_query(&out)
    }

    pub fn sync_only(&self) -> Result<ExecResult> {
        self.exec(
            &ExecSpec {
                cmd: Cmd::Shell("true".into()),
                sync: true,
                color: false,
            },
            &mut |_, _| {},
        )
    }

    pub fn exec(
        &self,
        spec: &ExecSpec,
        on_line: &mut dyn FnMut(Stream, &[u8]),
    ) -> Result<ExecResult> {
        let t0 = Instant::now();
        for w in &self.warnings {
            info!("warning: {w}");
        }
        let remote = self.query(spec.sync)?;
        if matches!(spec.cmd, Cmd::Cargo(_)) && !remote.has_cargo {
            info!("first use: installing the Rust toolchain on `{}` (one time only, takes a few minutes)", self.inst_name);
            self.bootstrap()?;
        }
        let plan = if spec.sync {
            sync::plan(&self.roots, &remote, util::now_secs())
        } else {
            sync::Plan::default()
        };
        let script = self.remote_script(&spec.cmd);
        let env = self.env_block(spec.color, &[])?;
        let base_bytes = remote.base.clone().into_bytes();
        let sync_secs = t0.elapsed().as_secs_f64();
        if spec.sync {
            info!(
                "sync: {} of {} files ({}){} in {:.1}s",
                plan.stats.files_sent,
                plan.stats.files_total,
                util::human_bytes(plan.stats.bytes_sent),
                if plan.stats.deleted > 0 {
                    format!(", {} removed", plan.stats.deleted)
                } else {
                    String::new()
                },
                sync_secs
            );
        }

        let mut child = self
            .ssh
            .remote(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(crate::transport::spawn_err)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<(Stream, Vec<u8>)>();

        let roots = &self.roots;
        let plan_ref = &plan;
        let env_ref = &env;
        std::thread::scope(|s| {
            let writer = s.spawn(move || sync::write_stream(stdin, env_ref, roots, plan_ref));
            for (stream, pipe) in [
                (
                    Stream::Out,
                    Box::new(stdout) as Box<dyn std::io::Read + Send>,
                ),
                (
                    Stream::Err,
                    Box::new(stderr) as Box<dyn std::io::Read + Send>,
                ),
            ] {
                let tx = tx.clone();
                s.spawn(move || {
                    let mut r = BufReader::new(pipe);
                    loop {
                        let mut buf = Vec::new();
                        match r.read_until(b'\n', &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if tx.send((stream, buf)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            drop(tx);
            for (stream, line) in rx {
                let mapped = strip_all(&line, &base_bytes);
                on_line(stream, &mapped);
            }
            match writer.join() {
                Ok(Ok(())) => {}
                // remote exited before reading everything (e.g. cargo not found): not our error
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Ok(Err(e)) => info!("sync stream error: {e}"),
                Err(_) => info!("sync writer panicked"),
            }
        });
        let status = child.wait()?;
        Ok(ExecResult {
            code: exit_code(status),
            sync: plan.stats.clone(),
            sync_secs,
            total_secs: t0.elapsed().as_secs_f64(),
        })
    }

    /// Fetch files under the local `target/` directory from the remote workspace.
    pub fn pull(&self, local_paths: &[PathBuf]) -> Result<usize> {
        let target_dir = self.project.root.join("target");
        let mut rels = vec![];
        for p in local_paths {
            if !p.starts_with(&target_dir) {
                info!(
                    "not pulling {} (outside {})",
                    p.display(),
                    target_dir.display()
                );
                continue;
            }
            let s = p.to_str().context("non-UTF-8 artifact path")?;
            rels.push(shq(s.trim_start_matches('/')));
        }
        if rels.is_empty() {
            return Ok(0);
        }
        let script = format!("cd {} && tar -cf - -- {}", self.base_expr(), rels.join(" "));
        let mut child = self
            .ssh
            .remote(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut n = 0;
        {
            let mut ar = tar::Archive::new(child.stdout.take().unwrap());
            for entry in ar.entries()? {
                let mut entry = entry?;
                let rel = entry.path()?.into_owned();
                let dest = Path::new("/").join(&rel);
                if !dest.starts_with(&target_dir)
                    || rel
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                    || !entry.header().entry_type().is_file()
                {
                    continue;
                }
                if let Some(d) = dest.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let mode = entry.header().mode().unwrap_or(0o755);
                let _ = std::fs::remove_file(&dest);
                entry.unpack(&dest)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        &dest,
                        std::fs::Permissions::from_mode(mode & 0o7777),
                    )?;
                }
                info!("pulled {}", dest.display());
                n += 1;
            }
        }
        let st = child.wait()?;
        if !st.success() {
            bail!("remote tar failed (exit {})", exit_code(st));
        }
        Ok(n)
    }
}

pub fn hint_for(code: i32) -> &'static str {
    match code {
        127 => "\nhint: `cargo` was not found on the remote. Run `rcargo remote setup` to install a toolchain.",
        255 => "\nhint: ssh could not connect. Run `rcargo remote doctor`.",
        75 => "\nhint: another build holds this workspace's lock; retry shortly.",
        _ => "",
    }
}

/// Script that lists every workspace on the instance: id, origin, size, last-used epoch.
pub fn workspaces_script(base_cfg: Option<&str>) -> String {
    let root = match base_cfg {
        Some(b) => shq(b.trim_end_matches('/')),
        None => "\"$HOME/.rcargo/ws\"".into(),
    };
    format!(
        r#"R={root}
[ -d "$R" ] || exit 0
for d in "$R"/*/; do
  [ -d "$d" ] || continue
  id=$(basename "$d")
  origin=$(cat "$d/.state/origin" 2>/dev/null || echo '?')
  kb=$(du -sk "$d" 2>/dev/null | cut -f1)
  last=$(stat -c %Y "$d/.state/last-used" 2>/dev/null || echo 0)
  printf '%s\t%s\t%s\t%s\n' "$id" "$origin" "${{kb:-0}}" "$last"
done
"#
    )
}

pub struct WorkspaceInfo {
    pub id: String,
    pub origin: String,
    pub kib: u64,
    pub last_used: i64,
}

pub fn parse_workspaces(out: &str) -> Vec<WorkspaceInfo> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(4, '\t');
            Some(WorkspaceInfo {
                id: it.next()?.to_string(),
                origin: it.next()?.to_string(),
                kib: it.next()?.parse().ok()?,
                last_used: it.next()?.parse().ok()?,
            })
        })
        .collect()
}

pub const SETUP_SCRIPT: &str = r#"set -e
S=""; [ "$(id -u)" = 0 ] || S="sudo"
if ! command -v cc >/dev/null 2>&1; then
  echo "installing C toolchain..."
  $S apt-get update -y >/dev/null && $S apt-get install -y build-essential curl ca-certificates pkg-config >/dev/null
fi
for t in tar flock find; do
  command -v $t >/dev/null 2>&1 || { $S apt-get update -y >/dev/null; $S apt-get install -y tar util-linux findutils >/dev/null; break; }
done
if ! command -v cargo >/dev/null 2>&1; then
  echo "installing rustup + stable toolchain..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
fi
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
command -v rustup >/dev/null 2>&1 && rustup component add clippy rustfmt >/dev/null 2>&1 || true
cargo --version
rustc --version
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_listing_parses() {
        let w = parse_workspaces("a-1\t/home/x/a\t2048\t1731567890\nbad line\nb-2\t?\t10\t0\n");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].origin, "/home/x/a");
        assert_eq!(w[0].kib, 2048);
        assert_eq!(w[1].last_used, 0);
    }

    #[test]
    fn hints() {
        assert!(hint_for(127).contains("setup"));
        assert!(hint_for(255).contains("doctor"));
        assert_eq!(hint_for(1), "");
    }
}
