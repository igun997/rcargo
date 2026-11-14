//! Project discovery: which directory is the cargo root, which extra directories
//! (`path = "../x"` dependencies, possibly private and never on GitHub) must travel with it.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::config::SyncCfg;
use crate::sync::{self, Root};
use crate::util::fnv64;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    /// Current directory relative to `root` ("" at the root).
    pub cwd_rel: String,
    /// Stable id of this project's remote workspace.
    pub id: String,
}

fn manifest_has_workspace(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .is_some_and(|t| t.contains_key("workspace"))
}

pub fn discover(cwd: &Path) -> Result<Project> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", cwd.display()))?;
    let mut nearest: Option<PathBuf> = None;
    let mut workspace: Option<PathBuf> = None;
    for dir in cwd.ancestors() {
        if dir.join("Cargo.toml").is_file() {
            nearest.get_or_insert_with(|| dir.to_path_buf());
            if manifest_has_workspace(dir) {
                workspace = Some(dir.to_path_buf());
            }
        }
    }
    let Some(root) = workspace.or(nearest) else {
        bail!(
            "{} is not inside a cargo project (no Cargo.toml found upwards)",
            cwd.display()
        );
    };
    let cwd_rel = cwd
        .strip_prefix(&root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string();
    let name: String = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let id = format!(
        "{}-{:012x}",
        name,
        fnv64(root.as_os_str().as_encoded_bytes()) & 0xffff_ffff_ffff
    );
    Ok(Project { root, cwd_rel, id })
}

#[derive(PartialEq)]
enum Ctx {
    Other,
    DepTable,
    PatchTable,
}

fn collect(v: &toml::Value, ctx: Ctx, out: &mut Vec<String>) {
    let toml::Value::Table(t) = v else { return };
    match ctx {
        Ctx::DepTable => {
            for spec in t.values() {
                if let Some(p) = spec.get("path").and_then(|p| p.as_str()) {
                    out.push(p.to_string());
                }
            }
        }
        Ctx::PatchTable => {
            for registry in t.values() {
                collect(registry, Ctx::DepTable, out);
            }
            return;
        }
        Ctx::Other => {}
    }
    for (k, val) in t {
        let c = if k == "patch" {
            Ctx::PatchTable
        } else if k == "replace" || k.ends_with("dependencies") {
            Ctx::DepTable
        } else {
            Ctx::Other
        };
        collect(val, c, out);
    }
}

/// Every `path = "..."` dependency declared in a Cargo.toml (dependencies, workspace.dependencies,
/// target-specific tables, [patch], [replace]).
pub fn path_deps(manifest: &str) -> Vec<String> {
    let Ok(v) = manifest.parse::<toml::Value>() else {
        return vec![];
    };
    let mut out = vec![];
    collect(&v, Ctx::Other, &mut out);
    out.sort();
    out.dedup();
    out
}

/// A path dependency's directory may inherit from a parent `[workspace]`; ship that whole workspace.
fn external_root_for(dep_dir: &Path) -> PathBuf {
    for anc in dep_dir.ancestors() {
        if anc.join("Cargo.toml").is_file() && manifest_has_workspace(anc) {
            return anc.to_path_buf();
        }
    }
    dep_dir.to_path_buf()
}

fn contains(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer)
}

/// Walk the project root plus every external path-dependency root, breadth first.
pub fn collect_roots(
    project: &Project,
    cfg: &SyncCfg,
    warnings: &mut Vec<String>,
) -> Result<Vec<Root>> {
    let follow = cfg.path_deps.unwrap_or(true);
    let mut roots: Vec<Root> = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::from([project.root.clone()]);
    while let Some(path) = queue.pop_front() {
        if roots.iter().any(|r| contains(&r.path, &path)) {
            continue;
        }
        let entries = sync::walk(&path, cfg, warnings)?;
        if follow {
            for e in &entries {
                if e.rel != "Cargo.toml" && !e.rel.ends_with("/Cargo.toml") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&e.abs) else {
                    continue;
                };
                let mdir = e.abs.parent().unwrap_or(&path);
                for dep in path_deps(&text) {
                    let target = match mdir.join(&dep).canonicalize() {
                        Ok(t) if t.is_dir() => t,
                        _ => {
                            warnings.push(format!(
                                "path dependency `{dep}` of {} does not exist locally",
                                e.abs.display()
                            ));
                            continue;
                        }
                    };
                    if contains(&path, &target) || roots.iter().any(|r| contains(&r.path, &target))
                    {
                        continue;
                    }
                    queue.push_back(external_root_for(&target));
                }
            }
        }
        roots.push(Root {
            mirror: sync::mirror_of(&path)?,
            key: sync::root_key(&path),
            path,
            entries,
        });
    }
    // a later root may contain an earlier one: keep the outermost only
    let paths: Vec<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
    roots.retain(|r| !paths.iter().any(|p| p != &r.path && contains(p, &r.path)));
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_path_deps_everywhere() {
        let m = r#"
[package]
name = "x"
[lib]
path = "src/lib.rs"
[dependencies]
a = { path = "../a" }
b = "1"
c = { version = "1", path = "../c/crates/c" }
[dev-dependencies]
d = { path = "../d" }
[target.'cfg(unix)'.build-dependencies]
e = { path = "../e" }
[workspace.dependencies]
f = { path = "crates/f" }
[patch.crates-io]
g = { path = "../g" }
[[bin]]
name = "p"
path = "src/bin/p.rs"
"#;
        assert_eq!(
            path_deps(m),
            vec!["../a", "../c/crates/c", "../d", "../e", "../g", "crates/f"]
        );
    }

    #[test]
    fn crate_named_path_is_not_confused() {
        assert_eq!(
            path_deps("[dependencies]\npath = \"1\"\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn follows_external_path_deps_and_mirrors_layout() {
        let base = std::env::temp_dir().join(format!("rcargo-proj-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("app/src")).unwrap();
        fs::create_dir_all(base.join("shared/crates/util/src")).unwrap();
        fs::write(
            base.join("app/Cargo.toml"),
            "[package]\nname='app'\n[dependencies]\nutil={path='../shared/crates/util'}\n",
        )
        .unwrap();
        fs::write(base.join("app/src/main.rs"), "fn main(){}").unwrap();
        fs::write(
            base.join("shared/Cargo.toml"),
            "[workspace]\nmembers=['crates/*']\n[workspace.package]\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(
            base.join("shared/crates/util/Cargo.toml"),
            "[package]\nname='util'\nversion.workspace=true\n",
        )
        .unwrap();
        fs::write(base.join("shared/crates/util/src/lib.rs"), "").unwrap();

        let proj = discover(&base.join("app/src")).unwrap();
        assert_eq!(proj.root, base.join("app").canonicalize().unwrap());
        assert_eq!(proj.cwd_rel, "src");
        let mut w = vec![];
        let roots = collect_roots(&proj, &SyncCfg::default(), &mut w).unwrap();
        let paths: Vec<_> = roots.iter().map(|r| r.path.clone()).collect();
        let shared = base.join("shared").canonicalize().unwrap();
        assert!(paths.contains(&proj.root), "{paths:?}");
        assert!(
            paths.contains(&shared),
            "workspace parent of the path dep ships whole: {paths:?}"
        );
        assert_eq!(roots.len(), 2);
        let off = SyncCfg {
            path_deps: Some(false),
            ..Default::default()
        };
        assert_eq!(collect_roots(&proj, &off, &mut w).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_root_wins_over_member() {
        let base = std::env::temp_dir().join(format!("rcargo-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("crates/m")).unwrap();
        fs::write(
            base.join("Cargo.toml"),
            "[workspace]\nmembers=['crates/m']\n",
        )
        .unwrap();
        fs::write(base.join("crates/m/Cargo.toml"), "[package]\nname='m'\n").unwrap();
        let p = discover(&base.join("crates/m")).unwrap();
        assert_eq!(p.root, base.canonicalize().unwrap());
        assert_eq!(p.cwd_rel, "crates/m");
        assert!(p.id.contains("rcargo-ws"));
        let _ = fs::remove_dir_all(&base);
    }
}
