//! /etc/preview/config.toml, written by the NixOS module.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/preview/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Previews are served at `<name>.<domain>`.
    pub domain: String,
    /// State, locks, logs, checkouts and artifacts.
    pub state_dir: PathBuf,
    /// Slots are numbered 1..=slots.
    pub slots: u32,
    /// The ZFS pool; a slot's dataset is `<pool>/slot-<n>`.
    pub pool: String,
    /// What a slot's dataset is cloned from, `dataset@snapshot`.
    pub template_snapshot: String,
    /// A slot's dataset is mounted at `<slot_root>/slot-<n>/d`; the guest's
    /// share is `<slot_root>/slot-<n>`.
    pub slot_root: PathBuf,
    /// nginx's `map $host $preview_port` include.
    pub routes_map: PathBuf,
    /// The guest's view of the host, where its taper listens.
    pub guest_gateway: String,
    /// The guest user the shop's files belong to.
    pub guest_uid: u32,
    pub guest_gid: u32,
    /// Rendered per slot, relative to the dataset root.
    pub env_php: EnvPhp,
    pub ports: Ports,
    pub postgres: Postgres,
    pub taper: Taper,
    #[serde(default)]
    pub build: Option<Build>,
    #[serde(default)]
    pub webhook: Option<Webhook>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvPhp {
    /// The template, with `{{name}}`, `{{base_url}}`, `{{mysql_host}}`,
    /// `{{redis_host}}`, `{{redis_port}}`, `{{search_host}}` and
    /// `{{search_port}}` placeholders.
    pub template: String,
    /// Where the rendered file goes. A target that is a symlink is skipped.
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    /// A slot's vsock context id is `cid_base + n`.
    pub cid_base: u32,
    /// Each port is `<base> + n` on the host's loopback.
    pub http_base: u16,
    pub mysql_base: u16,
    pub redis_base: u16,
    pub search_base: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Postgres {
    pub socket_dir: PathBuf,
    pub user: String,
    /// The database every slot's database is a copy of. Nothing may be
    /// connected to it, or PostgreSQL refuses the copy.
    pub template_database: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Taper {
    pub binary: PathBuf,
    /// The MySQL database name the shop connects to.
    pub mysql_database: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// What to clone, over ssh.
    pub repo: String,
    /// A read-only deploy key for it.
    pub ssh_key: PathBuf,
    /// Copied into the checkout as auth.json for Composer.
    pub composer_auth: PathBuf,
    /// A static Linux magebuild: it builds here and is the agent in the guest.
    pub magebuild: PathBuf,
    /// The guest's PHP, for magebuild deploy's `php`.
    pub guest_php: String,
    /// The shop's directory inside the repository, if not the root.
    #[serde(default)]
    pub subdir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    pub listen: String,
    pub secret_file: PathBuf,
    /// Pushes to these branches build nothing.
    #[serde(default)]
    pub ignore_branches: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn vm(&self, slot: u32) -> String {
        format!("preview-{slot}")
    }
    pub fn dataset(&self, slot: u32) -> String {
        format!("{}/slot-{slot}", self.pool)
    }
    pub fn mountpoint(&self, slot: u32) -> PathBuf {
        self.slot_root.join(format!("slot-{slot}")).join("d")
    }
    pub fn database(&self, slot: u32) -> String {
        format!("slot_{slot}")
    }
    pub fn taper_unit(&self, slot: u32) -> String {
        format!("preview-taper@{slot}.service")
    }
    pub fn cid(&self, slot: u32) -> u32 {
        self.ports.cid_base + slot
    }
    pub fn http_port(&self, slot: u32) -> u16 {
        self.ports.http_base + slot as u16
    }
    pub fn mysql_port(&self, slot: u32) -> u16 {
        self.ports.mysql_base + slot as u16
    }
    pub fn redis_port(&self, slot: u32) -> u16 {
        self.ports.redis_base + slot as u16
    }
    pub fn search_port(&self, slot: u32) -> u16 {
        self.ports.search_base + slot as u16
    }
    pub fn url(&self, name: &str) -> String {
        format!("https://{name}.{}/", self.domain)
    }
}
