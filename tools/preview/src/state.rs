//! Which preview is in which slot, in `<state_dir>/state.json`, and the lock
//! that serializes every change to it, the CLI's and the daemon's alike.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub slots: BTreeMap<u32, Slot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub name: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// The commit deployed into it, once one has been.
    #[serde(default)]
    pub sha: Option<String>,
    /// Unix seconds.
    pub since: u64,
}

impl State {
    pub fn load(dir: &Path) -> Result<State> {
        let path = dir.join("state.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join("state.json");
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    pub fn slot_of(&self, name: &str) -> Option<u32> {
        self.slots
            .iter()
            .find(|(_, s)| s.name == name)
            .map(|(n, _)| *n)
    }
}

/// Held for the duration of an operation; released on drop.
pub struct Lock(#[allow(dead_code)] File);

pub fn lock(dir: &Path) -> Result<Lock> {
    std::fs::create_dir_all(dir)?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    file.lock().context("locking the preview state")?;
    Ok(Lock(file))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
