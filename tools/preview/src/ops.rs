//! Bringing a slot up and down.
//!
//! A slot that is up has, in this order of creation:
//!
//! 1. a ZFS clone of the template dataset, the guest's /data;
//! 2. a PostgreSQL database copied from the template database;
//! 3. an env.php in the clone pointing at that slot's taper;
//! 4. a taper (`preview-taper@<n>`) serving the database over MySQL, Redis
//!    and the search API;
//! 5. the VM (`microvm@preview-<n>`);
//! 6. a line in nginx's route map.
//!
//! `down` removes all of them, and does not stop at the first thing that is
//! already gone, so it also cleans up after an `up` that failed halfway.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use crate::config::Config;
use crate::state::{self, Slot, State};

pub async fn sh(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn lock(cfg: &Config) -> Result<state::Lock> {
    let dir = cfg.state_dir.clone();
    tokio::task::spawn_blocking(move || state::lock(&dir)).await?
}

async fn postgres(cfg: &Config) -> Result<tokio_postgres::Client> {
    let (client, conn) = tokio_postgres::Config::new()
        .host_path(&cfg.postgres.socket_dir)
        .user(&cfg.postgres.user)
        .dbname("postgres")
        .connect(tokio_postgres::NoTls)
        .await
        .context("connecting to PostgreSQL")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

async fn wait_for_port(port: u16, wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("nothing listens on 127.0.0.1:{port} after {wait:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Which slot a `down`/`exec` target means: a slot number or a preview name.
pub fn resolve(cfg: &Config, state: &State, target: &str) -> Result<u32> {
    if let Ok(n) = target.parse::<u32>()
        && (1..=cfg.slots).contains(&n)
    {
        return Ok(n);
    }
    let name = target.strip_prefix("preview-").unwrap_or(target);
    if let Ok(n) = name.parse::<u32>()
        && (1..=cfg.slots).contains(&n)
    {
        return Ok(n);
    }
    state
        .slot_of(target)
        .ok_or_else(|| anyhow!("no preview or slot {target:?}"))
}

fn render_env_php(cfg: &Config, slot: u32, name: &str) -> Result<()> {
    let root = cfg.mountpoint(slot);
    let template = root.join(&cfg.env_php.template);
    let text = std::fs::read_to_string(&template)
        .with_context(|| format!("reading {}", template.display()))?;
    let text = text
        .replace("{{name}}", name)
        .replace("{{base_url}}", &cfg.url(name))
        .replace(
            "{{mysql_host}}",
            &format!("{}:{}", cfg.guest_gateway, cfg.mysql_port(slot)),
        )
        .replace("{{redis_host}}", &cfg.guest_gateway)
        .replace("{{redis_port}}", &cfg.redis_port(slot).to_string())
        .replace("{{search_host}}", &cfg.guest_gateway)
        .replace("{{search_port}}", &cfg.search_port(slot).to_string());
    for target in &cfg.env_php.targets {
        let path = root.join(target);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        if !path.parent().is_some_and(Path::exists) {
            continue;
        }
        std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
        std::os::unix::fs::chown(&path, Some(cfg.guest_uid), Some(cfg.guest_gid))?;
    }
    Ok(())
}

async fn write_routes(cfg: &Config, state: &State) -> Result<()> {
    let mut map = String::new();
    for (slot, s) in &state.slots {
        map.push_str(&format!(
            "{}.{} {};\n",
            s.name,
            cfg.domain,
            cfg.http_port(*slot)
        ));
    }
    let old = std::fs::read_to_string(&cfg.routes_map).unwrap_or_default();
    if old != map {
        let tmp = cfg.routes_map.with_extension("tmp");
        std::fs::write(&tmp, &map)?;
        std::fs::rename(&tmp, &cfg.routes_map)?;
        sh("systemctl", &["reload", "nginx.service"]).await?;
    }
    Ok(())
}

/// Brings `name` up in `want` (or the first free slot) and returns the slot.
/// A name that is already up is left as it is.
pub async fn up(cfg: &Config, name: &str, want: Option<u32>, branch: Option<&str>) -> Result<u32> {
    crate::check_name(name)?;
    let _lock = lock(cfg).await?;
    let mut state = State::load(&cfg.state_dir)?;
    if let Some(slot) = state.slot_of(name) {
        return Ok(slot);
    }
    let slot = match want {
        Some(n) if !(1..=cfg.slots).contains(&n) => bail!("there is no slot {n}"),
        Some(n) => match state.slots.get(&n) {
            Some(s) => bail!("slot {n} holds {}", s.name),
            None => n,
        },
        None => (1..=cfg.slots)
            .find(|n| !state.slots.contains_key(n))
            .ok_or_else(|| anyhow!("all {} slots are in use", cfg.slots))?,
    };

    // Whatever an earlier failure left behind.
    let _ = teardown(cfg, slot).await;
    if let Err(e) = bring_up(cfg, slot, name).await {
        let _ = teardown(cfg, slot).await;
        return Err(e.context(format!("bringing {name} up in slot {slot}")));
    }
    state.slots.insert(
        slot,
        Slot {
            name: name.to_string(),
            branch: branch.map(str::to_string),
            sha: None,
            since: state::now(),
        },
    );
    state.save(&cfg.state_dir)?;
    write_routes(cfg, &state).await?;
    Ok(slot)
}

async fn bring_up(cfg: &Config, slot: u32, name: &str) -> Result<()> {
    let mount = cfg.mountpoint(slot);
    sh(
        "zfs",
        &[
            "clone",
            "-o",
            "sync=disabled",
            "-o",
            &format!("mountpoint={}", mount.display()),
            &cfg.template_snapshot,
            &cfg.dataset(slot),
        ],
    )
    .await?;

    postgres(cfg)
        .await?
        .batch_execute(&format!(
            "CREATE DATABASE {} TEMPLATE {}",
            ident(&cfg.database(slot)),
            ident(&cfg.postgres.template_database)
        ))
        .await
        .context("copying the template database")?;
    copy_database_settings(cfg, slot).await?;

    render_env_php(cfg, slot, name)?;

    sh("systemctl", &["start", &cfg.taper_unit(slot)]).await?;
    wait_for_port(cfg.mysql_port(slot), Duration::from_secs(10)).await?;
    sh(
        "systemctl",
        &["start", &format!("microvm@{}.service", cfg.vm(slot))],
    )
    .await?;
    Ok(())
}

/// `CREATE DATABASE ... TEMPLATE` copies the data but not `ALTER DATABASE ...
/// SET`, and taper's install sets the search_path that way.
async fn copy_database_settings(cfg: &Config, slot: u32) -> Result<()> {
    let pg = postgres(cfg).await?;
    let rows = pg
        .query(
            "SELECT unnest(s.setconfig) FROM pg_db_role_setting s \
             JOIN pg_database d ON d.oid = s.setdatabase \
             WHERE d.datname = $1 AND s.setrole = 0",
            &[&cfg.postgres.template_database],
        )
        .await
        .context("reading the template database's settings")?;
    for row in rows {
        let setting: String = row.get(0);
        let Some((key, value)) = setting.split_once('=') else {
            continue;
        };
        // The value as PostgreSQL stored it, unquoted: a list such as
        // search_path's must stay a list. It comes from our own template.
        pg.batch_execute(&format!(
            "ALTER DATABASE {} SET {} TO {value}",
            ident(&cfg.database(slot)),
            ident(key)
        ))
        .await
        .with_context(|| format!("setting {key} on the slot's database"))?;
    }
    Ok(())
}

/// Removes everything a slot can hold; the first error is returned after all
/// steps have been tried.
async fn teardown(cfg: &Config, slot: u32) -> Result<()> {
    let vm = cfg.vm(slot);
    let unit = format!("microvm@{vm}.service");
    let mut first: Option<anyhow::Error> = None;
    // Nothing in a preview is worth a clean shutdown: its disk is about to go.
    let _ = sh("systemctl", &["kill", "--signal=KILL", &unit]).await;
    if let Err(e) = sh(
        "systemctl",
        &["stop", &unit, &format!("microvm-virtiofsd@{vm}.service")],
    )
    .await
    {
        first.get_or_insert(e);
    }
    let _ = sh("systemctl", &["reset-failed", &unit]).await;
    if let Err(e) = sh("systemctl", &["stop", &cfg.taper_unit(slot)]).await {
        first.get_or_insert(e);
    }
    match postgres(cfg).await {
        Ok(pg) => {
            if let Err(e) = pg
                .batch_execute(&format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    ident(&cfg.database(slot))
                ))
                .await
            {
                first.get_or_insert(e.into());
            }
        }
        Err(e) => {
            first.get_or_insert(e);
        }
    }
    let dataset = cfg.dataset(slot);
    if sh("zfs", &["list", "-H", "-o", "name", &dataset])
        .await
        .is_ok()
        && let Err(e) = sh("zfs", &["destroy", &dataset]).await
    {
        first.get_or_insert(e);
    }
    match first {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub async fn down(cfg: &Config, target: &str) -> Result<(u32, Option<String>)> {
    let _lock = lock(cfg).await?;
    let mut state = State::load(&cfg.state_dir)?;
    let slot = resolve(cfg, &state, target)?;
    let result = teardown(cfg, slot).await;
    let removed = state.slots.remove(&slot).map(|s| s.name);
    state.save(&cfg.state_dir)?;
    write_routes(cfg, &state).await?;
    result.map(|()| (slot, removed))
}

/// Records the commit now deployed in a slot.
pub async fn set_deployed(cfg: &Config, slot: u32, sha: &str) -> Result<()> {
    let _lock = lock(cfg).await?;
    let mut state = State::load(&cfg.state_dir)?;
    if let Some(s) = state.slots.get_mut(&slot) {
        s.sha = Some(sha.to_string());
    }
    state.save(&cfg.state_dir)
}
