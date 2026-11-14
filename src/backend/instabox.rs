use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{control_dir, ssh_cache_dir, Backend, Status};
use crate::config::Instance;
use crate::transport::Ssh;

pub struct Instabox {
    name: String,
    box_name: String,
    inst: Instance,
}

fn cli(args: &[&str]) -> Result<(i32, String, String)> {
    let out = Command::new("instabox")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                crate::guidance::instabox_missing().into_err()
            } else {
                anyhow::anyhow!("failed to run instabox: {e}")
            }
        })?;
    if out.status.code() == Some(3) {
        return Err(crate::guidance::instabox_login().into_err());
    }
    Ok((
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Every box on the account: (name, state, "N vCPU / M MiB").
pub fn list_boxes() -> Result<Vec<(String, String, String)>> {
    let (code, out, err) = cli(&["ls", "--json"])?;
    if code != 0 {
        bail!("`instabox ls` failed ({code}): {}", err.trim());
    }
    let v: Value = serde_json::from_str(&out).context("parsing `instabox ls` output")?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .map(|b| {
                    (
                        b["name"].as_str().unwrap_or("?").to_string(),
                        b["state"].as_str().unwrap_or("?").to_string(),
                        format!(
                            "{} vCPU / {} MiB",
                            b["spec"]["vcpus"].as_u64().unwrap_or(0),
                            b["spec"]["memMiB"].as_u64().unwrap_or(0)
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default())
}

impl Instabox {
    pub fn new(name: &str, inst: &Instance) -> Self {
        Self {
            name: name.to_string(),
            box_name: inst.box_name.clone().unwrap_or_else(|| name.to_string()),
            inst: inst.clone(),
        }
    }

    fn get(&self) -> Result<Option<Value>> {
        let (code, out, err) = cli(&["get", &self.box_name, "--json"])?;
        match code {
            0 => Ok(Some(
                serde_json::from_str(&out).context("parsing `instabox get` output")?,
            )),
            4 => Ok(None),
            3 => Err(crate::guidance::instabox_login().into_err()),
            _ => bail!(
                "`instabox get {}` failed ({code}): {}",
                self.box_name,
                err.trim()
            ),
        }
    }

    fn create(&self) -> Result<()> {
        if self.inst.auto_create == Some(false) {
            bail!(
                "instabox box `{}` does not exist and auto_create=false for instance `{}`",
                self.box_name,
                self.name
            );
        }
        info!(
            "creating instabox box `{}` (first use; this is a one-time cost)",
            self.box_name
        );
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            self.box_name.clone(),
            "--json".into(),
        ];
        match (self.inst.vcpus, self.inst.mem_mib) {
            (Some(v), Some(m)) => args.extend([
                "--vcpus".into(),
                v.to_string(),
                "--mem-mib".into(),
                m.to_string(),
            ]),
            (None, None) => {}
            _ => bail!(
                "instance `{}`: set both `vcpus` and `mem_mib`, or neither",
                self.name
            ),
        }
        if let Some(t) = &self.inst.template {
            args.extend(["--template".into(), t.clone()]);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, err) = cli(&refs)?;
        if code != 0 {
            bail!("`instabox create` failed ({code}): {}", err.trim());
        }
        Ok(())
    }

    fn state_of(v: &Value) -> String {
        v["state"].as_str().unwrap_or("unknown").to_string()
    }

    fn ssh_config(&self) -> Result<Ssh> {
        let dir = ssh_cache_dir();
        std::fs::create_dir_all(&dir)?;
        let conf = dir.join(format!("instabox-{}.conf", self.box_name));
        let alias_file = dir.join(format!("instabox-{}.alias", self.box_name));
        let fresh = std::fs::metadata(&conf)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(25 * 60))
            && alias_file.is_file();
        if !fresh {
            let _ = cli(&["ssh-cert", &self.box_name]);
            let (code, out, err) = cli(&["ssh-config", &self.box_name, "--json"])?;
            if code != 0 {
                bail!("`instabox ssh-config` failed ({code}): {}", err.trim());
            }
            let v: Value =
                serde_json::from_str(&out).context("parsing `instabox ssh-config` output")?;
            let text = v["config"]
                .as_str()
                .context("ssh-config JSON has no `config`")?;
            let alias = v["host"]
                .as_str()
                .context("ssh-config JSON has no `host`")?;
            std::fs::write(&conf, text)?;
            std::fs::write(&alias_file, alias)?;
        }
        let alias = std::fs::read_to_string(&alias_file)?;
        Ok(Ssh {
            args: vec![
                "-F".into(),
                conf.to_string_lossy().into_owned(),
                alias.trim().to_string(),
            ],
            control_dir: control_dir(),
        })
    }
}

impl Backend for Instabox {
    fn ensure(&self) -> Result<Ssh> {
        let mut v = match self.get()? {
            Some(v) => v,
            None => {
                self.create()?;
                self.get()?.context("box vanished right after creation")?
            }
        };
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let state = Self::state_of(&v);
            match state.as_str() {
                "running" => break,
                s if s.contains("suspend") || s.contains("pause") => {
                    info!("resuming `{}`", self.box_name);
                    let (c, _, e) = cli(&["resume", &self.box_name])?;
                    if c != 0 {
                        bail!("`instabox resume` failed: {}", e.trim());
                    }
                }
                "stopped" => {
                    info!("starting `{}`", self.box_name);
                    let (c, _, e) = cli(&["start", &self.box_name])?;
                    if c != 0 {
                        bail!("`instabox start` failed: {}", e.trim());
                    }
                }
                "error" | "failed" => bail!(
                    "box `{}` is in state `{state}`; see `instabox logs {}`",
                    self.box_name,
                    self.box_name
                ),
                _ => std::thread::sleep(Duration::from_secs(3)),
            }
            if Instant::now() > deadline {
                bail!(
                    "box `{}` did not reach `running` in time (last state `{state}`)",
                    self.box_name
                );
            }
            v = self.get()?.context("box disappeared")?;
        }
        self.ssh_config()
    }

    fn status(&self) -> Result<Status> {
        match self.get()? {
            None => Ok(Status {
                state: "missing".into(),
                detail: format!(
                    "box `{}` not created yet (will be on first use)",
                    self.box_name
                ),
            }),
            Some(v) => {
                let vc = v["spec"]["vcpus"].as_u64().unwrap_or(0);
                let mem = v["spec"]["memMiB"].as_u64().unwrap_or(0);
                Ok(Status {
                    state: Self::state_of(&v),
                    detail: format!("box `{}` {} vCPU / {} MiB", self.box_name, vc, mem),
                })
            }
        }
    }

    fn stop(&self) -> Result<()> {
        let Some(v) = self.get()? else { return Ok(()) };
        if Self::state_of(&v) != "running" {
            return Ok(());
        }
        let (c, _, e) = cli(&["suspend", &self.box_name])?;
        if c != 0 {
            let (c2, _, e2) = cli(&["stop", &self.box_name])?;
            if c2 != 0 {
                bail!(
                    "could not suspend or stop `{}`: {} / {}",
                    self.box_name,
                    e.trim(),
                    e2.trim()
                );
            }
        }
        Ok(())
    }
}
