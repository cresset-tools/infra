//! Building a branch with magebuild and deploying it into a slot with
//! `magebuild deploy`, whose ssh is `preview ssh`: the deploy runs exactly as
//! it does against a real host, its agent inside the guest.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use crate::config::{Build, Config};

fn build_cfg(cfg: &Config) -> Result<&Build> {
    cfg.build
        .as_ref()
        .ok_or_else(|| anyhow!("no [build] section in the configuration"))
}

/// Runs a command with its output appended to `log`.
async fn logged(cmd: &mut Command, log: &Path, what: &str) -> Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let status = cmd
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .await
        .with_context(|| format!("starting {what}"))?;
    if !status.success() {
        bail!("{what} failed ({status}); see {}", log.display());
    }
    Ok(())
}

pub fn log_path(cfg: &Config, name: &str) -> PathBuf {
    cfg.state_dir.join("logs").join(format!("{name}.log"))
}

/// Checks out `sha` of `branch` and builds it; returns the project directory
/// and the artifact.
pub async fn build(
    cfg: &Config,
    name: &str,
    branch: &str,
    sha: &str,
) -> Result<(PathBuf, PathBuf)> {
    let b = build_cfg(cfg)?;
    let log = log_path(cfg, name);
    for dir in ["logs", "src", "artifacts", "cache"] {
        std::fs::create_dir_all(cfg.state_dir.join(dir))?;
    }
    let checkout = cfg.state_dir.join("src").join(name);
    let known_hosts = cfg.state_dir.join("known_hosts");
    let git_ssh = format!(
        "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
        b.ssh_key.display(),
        known_hosts.display()
    );
    let git = |args: &[&str]| {
        let mut c = Command::new("git");
        c.args(args).env("GIT_SSH_COMMAND", &git_ssh);
        c
    };

    if !checkout.join(".git").exists() {
        logged(
            git(&["init", "-q", &checkout.to_string_lossy()]).current_dir(&cfg.state_dir),
            &log,
            "git init",
        )
        .await?;
        logged(
            git(&["remote", "add", "origin", &b.repo]).current_dir(&checkout),
            &log,
            "git remote add",
        )
        .await?;
    }
    logged(
        git(&["fetch", "-q", "--depth", "1", "origin", sha]).current_dir(&checkout),
        &log,
        &format!("fetching {branch} at {sha}"),
    )
    .await?;
    logged(
        git(&["checkout", "-q", "--force", "--detach", sha]).current_dir(&checkout),
        &log,
        "git checkout",
    )
    .await?;
    // Untracked files go, ignored ones (vendor/, generated/) stay: they are
    // what makes the next build of this branch incremental.
    logged(
        git(&["clean", "-q", "-fd"]).current_dir(&checkout),
        &log,
        "git clean",
    )
    .await?;

    let project = match &b.subdir {
        Some(sub) => checkout.join(sub),
        None => checkout.clone(),
    };
    std::fs::copy(&b.composer_auth, project.join("auth.json"))
        .with_context(|| format!("copying {}", b.composer_auth.display()))?;

    let artifact = cfg
        .state_dir
        .join("artifacts")
        .join(format!("{name}.tar.zst"));
    let mut magebuild = Command::new(&b.magebuild);
    magebuild
        .current_dir(&project)
        .env(
            "MAGEBUILD_CACHE_DIR",
            cfg.state_dir.join("cache").join("composer-dist"),
        )
        .args(["--deployed-version", &crate::state::now().to_string()])
        .arg("--artifact")
        .arg(&artifact);
    if project.join("artifacts/excludes").exists() {
        magebuild.args(["--exclude-from", "artifacts/excludes"]);
    }
    logged(&mut magebuild, &log, "magebuild").await?;
    Ok((project, artifact))
}

/// magebuild.toml for deploying into a slot: the project's own, with its
/// `[deploy]` pointed at the guest. Everything the project says about shared
/// files, links and env.php is kept; cron and opcache are the guest's.
fn deploy_config(cfg: &Config, b: &Build, slot: u32, project: &Path) -> Result<String> {
    let path = project.join("magebuild.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut deploy = match doc.remove("deploy") {
        Some(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    deploy.insert("host".into(), format!("magento@{}", cfg.vm(slot)).into());
    deploy.insert("path".into(), "/data/app".into());
    deploy.insert("php".into(), b.guest_php.clone().into());
    deploy.insert("keep_releases".into(), 2.into());
    deploy.insert("maintenance".into(), "when-needed".into());
    deploy.remove("cron");
    let mut opcache = toml::Table::new();
    opcache.insert("fastcgi".into(), "/run/phpfpm/magento.sock".into());
    opcache.insert("tmp_dir".into(), "/data/app/shared/var/tmp".into());
    deploy.insert("opcache".into(), opcache.into());
    doc.insert("deploy".into(), deploy.into());
    Ok(toml::to_string(&doc)?)
}

/// Deploys an artifact into a running slot.
pub async fn deploy(
    cfg: &Config,
    name: &str,
    slot: u32,
    project: &Path,
    artifact: &Path,
) -> Result<()> {
    let b = build_cfg(cfg)?;
    let log = log_path(cfg, name);
    let dir = cfg.state_dir.join("deploy");
    std::fs::create_dir_all(&dir)?;
    let config = dir.join(format!("slot-{slot}.toml"));
    std::fs::write(&config, deploy_config(cfg, b, slot, project)?)?;
    let tmp = cfg.mountpoint(slot).join("app/shared/var/tmp");
    std::fs::create_dir_all(&tmp)?;
    std::os::unix::fs::chown(&tmp, Some(cfg.guest_uid), Some(cfg.guest_gid))?;

    // The guest may still be booting.
    let cid = cfg.cid(slot);
    tokio::task::spawn_blocking(move || crate::client::connect(cid, Duration::from_secs(30)))
        .await??;

    let me = std::env::current_exe()?;
    let mut magebuild = Command::new(&b.magebuild);
    magebuild
        .current_dir(project)
        .arg("--config")
        .arg(&config)
        .arg("deploy")
        .arg("--from")
        .arg(artifact)
        .arg("--agent")
        .arg(&b.magebuild)
        .arg("--ssh")
        .arg(format!("{} ssh", me.display()));
    logged(&mut magebuild, &log, "magebuild deploy").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deploy_section_points_at_the_guest_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("preview-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("magebuild.toml"),
            r#"
[nodes.di-compile]
fused = true

[deploy]
host = "magento@demo"
path = "/var/lib/magento"
shared_files = ["var/.maintenance.ip"]

[deploy.env_php]
source = "shared/app/etc/env.php"

[deploy.cron]
backend = "systemd"
stop = ["a"]
start = ["a"]
"#,
        )
        .unwrap();
        let cfg: Config = toml::from_str(include_str!("../tests/config.toml")).unwrap();
        let out = deploy_config(&cfg, cfg.build.as_ref().unwrap(), 2, &dir).unwrap();
        let doc: toml::Table = toml::from_str(&out).unwrap();
        let d = doc["deploy"].as_table().unwrap();
        assert_eq!(d["host"].as_str(), Some("magento@preview-2"));
        assert_eq!(d["path"].as_str(), Some("/data/app"));
        assert!(d.get("cron").is_none());
        assert_eq!(
            d["env_php"]["source"].as_str(),
            Some("shared/app/etc/env.php")
        );
        assert_eq!(d["shared_files"][0].as_str(), Some("var/.maintenance.ip"));
        assert!(doc["nodes"]["di-compile"]["fused"].as_bool().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
