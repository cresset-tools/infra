//! `preview`: preview environments on this host.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use preview::config::{self, Config};
use preview::proto::Request;
use preview::state::State;
use preview::{build, client, ops, webhook};

#[derive(Parser)]
#[command(
    name = "preview",
    about = "Preview environments: a branch of a shop on its own URL"
)]
struct Cli {
    #[arg(long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bring a preview up from the template, in the first free slot or the one given.
    Up {
        name: String,
        #[arg(long)]
        slot: Option<u32>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Take a preview down: VM, taper, database, dataset and route.
    Down {
        /// A preview name or a slot number.
        target: String,
    },
    /// The slots and what is in them.
    List,
    /// Run a command in a preview's guest; without one, a script from stdin.
    Exec {
        /// A preview name or a slot number.
        target: String,
        #[arg(long, default_value = "magento")]
        user: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// An ssh look-alike for `magebuild deploy --ssh`: `preview ssh
    /// [user@]preview-<n> [--] command...` runs the command with `sh -c`.
    Ssh {
        host: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Build a commit of a branch and deploy it into the branch's preview,
    /// as a webhook push would.
    Deploy { branch: String, sha: String },
    /// Deploy an artifact built elsewhere into a running preview.
    Release {
        /// A preview name or a slot number.
        target: String,
        /// The artifact (.tar.zst, .tar.gz or .tar), as `magebuild --artifact` writes it.
        artifact: PathBuf,
        /// The project directory whose magebuild.toml describes the deploy.
        #[arg(long)]
        project: PathBuf,
    },
    /// Serve GitHub's webhook and run the jobs it queues.
    Serve,
    /// Run the taper for a slot (the ExecStart of preview-taper@<n>).
    Taper { slot: u32 },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("preview: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    match cli.command {
        Cmd::Up { name, slot, branch } => {
            let slot = runtime()?.block_on(ops::up(&cfg, &name, slot, branch.as_deref()))?;
            println!("{} (slot {slot})", cfg.url(&name));
            Ok(0)
        }
        Cmd::Down { target } => {
            let (slot, name) = runtime()?.block_on(ops::down(&cfg, &target))?;
            match name {
                Some(n) => println!("{n} is down (slot {slot})"),
                None => println!("slot {slot} is clear"),
            }
            Ok(0)
        }
        Cmd::List => {
            let state = State::load(&cfg.state_dir)?;
            for n in 1..=cfg.slots {
                match state.slots.get(&n) {
                    Some(s) => println!(
                        "{n}  {:<24} {:<12} {}{}",
                        s.name,
                        s.sha
                            .as_deref()
                            .map(|s| &s[..s.len().min(10)])
                            .unwrap_or("-"),
                        cfg.url(&s.name),
                        s.branch
                            .as_deref()
                            .map(|b| format!("  ({b})"))
                            .unwrap_or_default()
                    ),
                    None => println!("{n}  -"),
                }
            }
            Ok(0)
        }
        Cmd::Exec {
            target,
            user,
            cwd,
            argv,
        } => {
            let state = State::load(&cfg.state_dir)?;
            let slot = ops::resolve(&cfg, &state, &target)?;
            let argv = if argv.is_empty() {
                vec!["sh".into(), "-s".into()]
            } else {
                argv
            };
            exec(&cfg, slot, user, cwd, argv)
        }
        Cmd::Ssh { host, command } => {
            let (user, host) = match host.split_once('@') {
                Some((u, h)) => (u.to_string(), h.to_string()),
                None => ("magento".to_string(), host),
            };
            let state = State::load(&cfg.state_dir)?;
            let slot = ops::resolve(&cfg, &state, &host)?;
            if command.is_empty() {
                bail!("preview ssh needs a command; there are no interactive sessions");
            }
            exec(
                &cfg,
                slot,
                user,
                None,
                vec!["sh".into(), "-c".into(), command.join(" ")],
            )
        }
        Cmd::Deploy { branch, sha } => {
            runtime()?.block_on(webhook::run_job(&cfg, webhook::Job::Deploy { branch, sha }))?;
            Ok(0)
        }
        Cmd::Release {
            target,
            artifact,
            project,
        } => {
            let state = State::load(&cfg.state_dir)?;
            let slot = ops::resolve(&cfg, &state, &target)?;
            let name = state
                .slots
                .get(&slot)
                .map(|s| s.name.clone())
                .context("that slot holds no preview")?;
            let artifact = std::fs::canonicalize(&artifact)?;
            let project = std::fs::canonicalize(&project)?;
            runtime()?.block_on(build::deploy(&cfg, &name, slot, &project, &artifact))?;
            println!("{} (slot {slot})", cfg.url(&name));
            Ok(0)
        }
        Cmd::Serve => {
            runtime()?.block_on(webhook::serve(cfg))?;
            Ok(0)
        }
        Cmd::Taper { slot } => {
            if !(1..=cfg.slots).contains(&slot) {
                bail!("there is no slot {slot}");
            }
            let socket = format!("/run/preview-taper-{slot}/mysql.sock");
            let url = format!(
                "postgres://{}@localhost/{}?host={}",
                cfg.postgres.user,
                cfg.database(slot),
                cfg.postgres.socket_dir.display()
            );
            let err = std::process::Command::new(&cfg.taper.binary)
                .args(["serve", "mysql", "search", "redis"])
                .env("TAPER_DATABASE_URL", url)
                .env("TAPER_MYSQL_SOCKET", socket)
                .env("TAPER_MYSQL_PORT", cfg.mysql_port(slot).to_string())
                // Only the host's loopback reaches it, and the guest behind it.
                .env("TAPER_MYSQL_INSECURE_TCP", "1")
                .env("TAPER_MYSQL_DATABASE", &cfg.taper.mysql_database)
                .env("TAPER_REDIS_PORT", cfg.redis_port(slot).to_string())
                .env("TAPER_SEARCH_PORT", cfg.search_port(slot).to_string())
                .exec();
            Err(err).with_context(|| format!("starting {}", cfg.taper.binary.display()))
        }
    }
}

fn exec(
    cfg: &Config,
    slot: u32,
    user: String,
    cwd: Option<String>,
    argv: Vec<String>,
) -> Result<i32> {
    let req = Request {
        user,
        argv,
        cwd,
        env: Default::default(),
    };
    client::exec(
        cfg.cid(slot),
        &req,
        Box::new(std::io::stdin()),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        Duration::from_secs(5),
    )
}
