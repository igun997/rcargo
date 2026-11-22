#[macro_use]
mod util;
mod backend;
mod cmds;
mod config;
mod guidance;
mod http;
mod init;
mod mcp;
mod port;
mod project;
mod run;
mod summary;
mod sync;
mod transport;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rcargo",
    version,
    about = "Run cargo on remote sandboxes (instabox, Daytona, any SSH host) instead of your own CPU",
    long_about = "Run cargo on remote sandboxes (instabox, Daytona, any SSH host) instead of your own CPU.\n\n\
Your working tree (uncommitted and private code included, plus `path = \"..\"` dependencies) is delta-synced \
over SSH, cargo runs remotely with a persistent target/ directory, and output streams back with paths \
rewritten to local ones. No GitHub access is involved.\n\n\
Any subcommand not listed below is passed to cargo and run remotely:\n  \
rcargo test --release\n  rcargo clippy --workspace -- -D warnings\n  rcargo nextest run\n  rcargo +nightly build",
    allow_external_subcommands = true,
    after_help = "Instances: you can configure several remote machines (`rcargo instances`) and pick one with -i NAME."
)]
struct Cli {
    /// Remote instance to use (see `rcargo instances`). Default: project `instance`, then `default_instance`.
    #[arg(short, long, global = true, env = "RCARGO_INSTANCE")]
    instance: Option<String>,
    /// Suppress rcargo's own progress lines on stderr.
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Copy executables produced by build/test back into ./target after a successful run.
    #[arg(long)]
    pull: bool,
    /// Skip syncing; run against whatever is already on the remote.
    #[arg(long)]
    no_sync: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List, add, remove and choose remote instances (machines rcargo can build on).
    Instances {
        #[command(subcommand)]
        action: Option<InstancesCmd>,
    },
    /// Manage the remote side: power, sync, shell, health, disk usage.
    Remote {
        #[command(subcommand)]
        action: RemoteCmd,
    },
    /// One-command setup for this project: config file, AI agent skill, MCP server, editor and a guard against local compiles.
    Init,
    /// Install agent/editor integration: skill files, rust-analyzer override, MCP config.
    Integrate(init::IntegrateArgs),
    /// Run as an MCP server over stdio (tools: cargo, shell, sync, instances, power, workspaces).
    Mcp,
    /// Show what is still missing to start building (instance, login, toolchain) and exactly how to fix it.
    Onboard {
        /// Also contact the provider and the instance (never creates or starts anything).
        #[arg(long)]
        live: bool,
        #[arg(long)]
        json: bool,
    },
    /// Claude Code PreToolUse hook: blocks local cargo build/test and points to rcargo (reads JSON on stdin).
    #[command(hide = true)]
    Hook,
    /// Port the slow part of a Python/Node/Ruby/PHP (or any) program to Rust and prove it behaves the same. Start with `rcargo port start`.
    Port {
        #[command(subcommand)]
        action: Option<port::PortCmd>,
    },
    #[command(external_subcommand)]
    Cargo(Vec<String>),
}

#[derive(Subcommand)]
pub enum InstancesCmd {
    /// Show configured instances with live state.
    List {
        #[arg(long)]
        json: bool,
        /// Do not contact providers; configuration only.
        #[arg(long)]
        offline: bool,
    },
    /// Register an instance in ~/.config/rcargo/config.toml.
    Add {
        name: String,
        #[arg(long)]
        backend: String,
        /// Shown to humans and AI agents; say what the machine is good for.
        #[arg(long)]
        description: Option<String>,
        /// Extra settings as key=value, repeatable (box, vcpus, mem_mib, host, port, identity, name, snapshot, cpu, memory, disk ...).
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Make it the default instance.
        #[arg(long)]
        default: bool,
    },
    /// Remove an instance from the config (the machine itself is not touched).
    Rm { name: String },
    /// Set the default instance.
    Use { name: String },
    /// Find existing instabox boxes / Daytona sandboxes not yet registered.
    Discover,
}

#[derive(Subcommand)]
pub enum RemoteCmd {
    /// Create/start/resume the instance and verify SSH works.
    Up,
    /// Pause/stop the instance to save money.
    Down,
    /// State of the instance and of this project's remote workspace.
    Status,
    /// Check local tools, the instance and the remote toolchain.
    Doctor,
    /// Install a Rust toolchain and C compiler on the remote if missing.
    Setup,
    /// Sync only; run nothing.
    Sync,
    /// Open a shell (or run a command) in this project's remote workspace.
    Shell {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List workspaces on the instance with disk usage.
    Ls,
    /// Delete stale workspaces (dry run unless --yes).
    Gc {
        #[arg(long, default_value_t = 14)]
        days: u64,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Delete this project's remote workspace, including its target/.
    Rm,
    /// Permanently delete the machine behind the instance (needs --yes). Local caches are cleaned too.
    Destroy {
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    util::set_quiet(cli.quiet);
    match dispatch(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if let Some(sr) = guidance::find(&e) {
                eprint!("\n{}", sr.render());
                std::process::exit(3);
            }
            eprintln!("rcargo: error: {e:#}");
            std::process::exit(2);
        }
    }
}

fn dispatch(cli: Cli) -> Result<i32> {
    let inst = cli.instance.clone();
    match cli.cmd {
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(0)
        }
        Some(Cmd::Cargo(args)) => cmds::cargo(inst.as_deref(), args, cli.pull, cli.no_sync),
        Some(Cmd::Instances { action }) => cmds::instances(action.unwrap_or(InstancesCmd::List {
            json: false,
            offline: false,
        })),
        Some(Cmd::Remote { action }) => cmds::remote(inst.as_deref(), action),
        Some(Cmd::Init) => init::init(),
        Some(Cmd::Integrate(a)) => init::integrate(a),
        Some(Cmd::Onboard { live, json }) => {
            let cwd = std::env::current_dir()?;
            let loaded = config::load(&cwd)?;
            let rep = guidance::onboard(&loaded, inst.as_deref(), live);
            if json {
                println!("{}", serde_json::to_string_pretty(&rep.to_json())?);
            } else {
                print!("{}", rep.render());
            }
            Ok(if rep.ready() { 0 } else { 3 })
        }
        Some(Cmd::Hook) => init::hook(),
        Some(Cmd::Port { action }) => port::run(
            inst.as_deref(),
            action.unwrap_or(port::PortCmd::Start {
                dir: None,
                json: false,
            }),
        ),
        Some(Cmd::Mcp) => {
            util::set_quiet(true);
            mcp::serve(inst)?;
            Ok(0)
        }
    }
}
