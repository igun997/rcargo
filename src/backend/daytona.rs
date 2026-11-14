use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{control_dir, ssh_cache_dir, Backend, Status};
use crate::config::Instance;
use crate::transport::Ssh;
use crate::util;

/// Daytona via its REST API (`curl`, key passed on stdin so it never shows in `ps`) and its SSH gateway.
/// Needs `DAYTONA_API_KEY`.
pub struct Daytona {
    sb: String,
    inst: Instance,
}

impl Daytona {
    pub fn new(name: &str, inst: &Instance) -> Self {
        Self {
            sb: inst.name.clone().unwrap_or_else(|| name.to_string()),
            inst: inst.clone(),
        }
    }

    fn api_url(&self) -> String {
        std::env::var("DAYTONA_API_URL")
            .ok()
            .or_else(|| self.inst.api_url.clone())
            .unwrap_or_else(|| "https://app.daytona.io/api".into())
            .trim_end_matches('/')
            .to_string()
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let key = std::env::var("DAYTONA_API_KEY")
            .map_err(|_| crate::guidance::daytona_key_missing().into_err())?;
        let mut cfg = format!("header = \"Authorization: Bearer {key}\"\nheader = \"Content-Type: application/json\"\n");
        let org = std::env::var("DAYTONA_ORGANIZATION_ID")
            .ok()
            .or_else(|| self.inst.organization_id.clone());
        if let Some(o) = org {
            cfg += &format!("header = \"X-Daytona-Organization-ID: {o}\"\n");
        }
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-m",
            "60",
            "-K",
            "-",
            "-X",
            method,
            "-w",
            "\n%{http_code}",
        ]);
        if method == "POST" || body.is_some() {
            cmd.args([
                "--data-binary",
                &body.map(|b| b.to_string()).unwrap_or_default(),
            ]);
        }
        cmd.arg(format!("{}{}", self.api_url(), path));
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    crate::guidance::curl_missing().into_err()
                } else {
                    anyhow::anyhow!("failed to run curl: {e}")
                }
            })?;
        child.stdin.take().unwrap().write_all(cfg.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "daytona API request failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (body_text, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
        let code: u16 = code.trim().parse().unwrap_or(0);
        let v = serde_json::from_str(body_text).unwrap_or(Value::Null);
        Ok((code, v))
    }

    fn get(&self) -> Result<Option<Value>> {
        let (code, v) = self.call("GET", &format!("/sandbox/{}", self.sb), None)?;
        match code {
            200 => Ok(Some(v)),
            404 => Ok(None),
            401 | 403 => Err(crate::guidance::daytona_key_rejected(code).into_err()),
            _ => bail!(
                "daytona GET sandbox failed (HTTP {code}): {}",
                v["message"].as_str().unwrap_or("")
            ),
        }
    }

    fn create(&self) -> Result<()> {
        info!("creating Daytona sandbox `{}` (first use)", self.sb);
        let mut b = json!({
            "name": self.sb,
            "labels": {"rcargo": "1"},
            "autoStopInterval": self.inst.auto_stop_minutes.unwrap_or(30),
        });
        for (k, v) in [
            ("cpu", self.inst.cpu),
            ("memory", self.inst.memory),
            ("disk", self.inst.disk),
        ] {
            if let Some(v) = v {
                b[k] = json!(v);
            }
        }
        if let Some(s) = &self.inst.snapshot {
            b["snapshot"] = json!(s);
        }
        let (code, v) = self.call("POST", "/sandbox", Some(&b))?;
        if !(200..300).contains(&code) {
            bail!(
                "daytona create failed (HTTP {code}): {}",
                v["message"].as_str().unwrap_or("")
            );
        }
        Ok(())
    }

    fn token(&self) -> Result<(String, String, u16)> {
        let dir = ssh_cache_dir();
        std::fs::create_dir_all(&dir)?;
        let cache = dir.join(format!("daytona-{}.json", self.sb));
        if let Ok(s) = std::fs::read_to_string(&cache) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if v["expires"].as_i64().unwrap_or(0) > util::now_secs() + 600 {
                    return Ok((
                        v["token"].as_str().unwrap_or_default().into(),
                        v["host"].as_str().unwrap_or_default().into(),
                        v["port"].as_u64().unwrap_or(22) as u16,
                    ));
                }
            }
        }
        let (code, v) = self.call(
            "POST",
            &format!("/sandbox/{}/ssh-access?expiresInMinutes=240", self.sb),
            None,
        )?;
        if !(200..300).contains(&code) {
            bail!(
                "daytona ssh-access failed (HTTP {code}): {}",
                v["message"].as_str().unwrap_or("")
            );
        }
        let token = v["token"]
            .as_str()
            .context("ssh-access response has no token")?
            .to_string();
        let mut host = self
            .inst
            .ssh_host
            .clone()
            .unwrap_or_else(|| "ssh.app.daytona.io".into());
        let mut port = 22u16;
        if let Some(cmd) = v["sshCommand"].as_str() {
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if let Some(i) = parts.iter().position(|p| *p == "-p") {
                port = parts.get(i + 1).and_then(|p| p.parse().ok()).unwrap_or(22);
            }
            if self.inst.ssh_host.is_none() {
                if let Some(h) = parts.iter().find_map(|p| p.split_once('@').map(|x| x.1)) {
                    host = h.to_string();
                }
            }
        }
        let rec = json!({"token": token, "host": host, "port": port, "expires": util::now_secs() + 240 * 60});
        std::fs::write(&cache, rec.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600));
        }
        Ok((token, host, port))
    }
}

impl Backend for Daytona {
    fn ensure(&self) -> Result<Ssh> {
        let mut v = match self.get()? {
            Some(v) => v,
            None => {
                self.create()?;
                self.get()?
                    .context("sandbox vanished right after creation")?
            }
        };
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let state = v["state"].as_str().unwrap_or("unknown").to_string();
            match state.as_str() {
                "started" => break,
                "stopped" | "archived" | "paused" => {
                    info!("starting Daytona sandbox `{}` (was {state})", self.sb);
                    let (c, r) = self.call("POST", &format!("/sandbox/{}/start", self.sb), None)?;
                    if !(200..300).contains(&c) {
                        bail!(
                            "daytona start failed (HTTP {c}): {}",
                            r["message"].as_str().unwrap_or("")
                        );
                    }
                }
                "error" | "build_failed" | "destroyed" | "destroying" => {
                    bail!("daytona sandbox `{}` is in state `{state}`", self.sb)
                }
                _ => {}
            }
            if Instant::now() > deadline {
                bail!(
                    "daytona sandbox `{}` did not start in time (last state `{state}`)",
                    self.sb
                );
            }
            std::thread::sleep(Duration::from_secs(3));
            v = self.get()?.context("sandbox disappeared")?;
        }
        let (token, host, port) = self.token()?;
        let known = ssh_cache_dir().join("daytona_known_hosts");
        Ok(Ssh {
            args: vec![
                "-o".into(),
                "StrictHostKeyChecking=accept-new".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", known.display()),
                "-p".into(),
                port.to_string(),
                format!("{token}@{host}"),
            ],
            control_dir: control_dir(),
        })
    }

    fn status(&self) -> Result<Status> {
        match self.get()? {
            None => Ok(Status {
                state: "missing".into(),
                detail: format!(
                    "sandbox `{}` not created yet (will be on first use)",
                    self.sb
                ),
            }),
            Some(v) => Ok(Status {
                state: match v["state"].as_str().unwrap_or("unknown") {
                    "started" => "running".to_string(),
                    s => s.to_string(),
                },
                detail: format!(
                    "sandbox `{}` {} vCPU / {} GB RAM / {} GB disk",
                    self.sb,
                    v["cpu"].as_u64().unwrap_or(0),
                    v["memory"].as_u64().unwrap_or(0),
                    v["disk"].as_u64().unwrap_or(0)
                ),
            }),
        }
    }

    fn stop(&self) -> Result<()> {
        let Some(v) = self.get()? else { return Ok(()) };
        if v["state"].as_str() != Some("started") {
            return Ok(());
        }
        let (c, r) = self.call("POST", &format!("/sandbox/{}/stop", self.sb), None)?;
        if !(200..300).contains(&c) {
            bail!(
                "daytona stop failed (HTTP {c}): {}",
                r["message"].as_str().unwrap_or("")
            );
        }
        Ok(())
    }
}

/// Every sandbox visible to the API key: (name, state).
pub fn list_sandboxes() -> Result<Vec<(String, String)>> {
    let d = Daytona::new("discover", &Instance::default());
    let (code, v) = d.call("GET", "/sandbox", None)?;
    if code != 200 {
        bail!(
            "daytona list failed (HTTP {code}): {}",
            v["message"].as_str().unwrap_or("")
        );
    }
    let items = v
        .as_array()
        .or_else(|| v["items"].as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .map(|s| {
            (
                s["name"]
                    .as_str()
                    .or_else(|| s["id"].as_str())
                    .unwrap_or("?")
                    .to_string(),
                s["state"].as_str().unwrap_or("?").to_string(),
            )
        })
        .collect())
}
