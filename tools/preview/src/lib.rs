//! Preview environments: a branch of a Magento shop on its own URL, in its own
//! microvm, with its own database.
//!
//! - [`proto`] is the vsock protocol between the host and a guest's agent.
//! - [`client`] is the host side of it (`preview exec`, `preview ssh`).
//! - [`ops`] brings slots up and down: a ZFS clone of the template, a
//!   PostgreSQL database cloned from the template database, a taper serving
//!   it, the VM, and the nginx route.
//! - [`build`] builds a branch with magebuild and deploys it into a slot.
//! - [`webhook`] turns GitHub pushes into builds and branch deletions into
//!   `down`s.

pub mod build;
pub mod client;
pub mod config;
pub mod ops;
pub mod proto;
pub mod state;
pub mod webhook;

use anyhow::{Result, bail};

/// A preview's name is the first label of its host name.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!(
            "{name:?} is not a preview name: 1-40 of a-z, 0-9 and -, not starting or ending with -"
        );
    }
    Ok(())
}

/// The preview name for a branch: `feature/Big_Thing` → `feature-big-thing`.
pub fn name_for_branch(branch: &str) -> String {
    let mut out = String::new();
    for c in branch.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut out = out.trim_matches('-').to_string();
    out.truncate(40);
    out.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_become_host_labels() {
        assert_eq!(name_for_branch("feature/Big_Thing"), "feature-big-thing");
        assert_eq!(name_for_branch("--x--y--"), "x-y");
        assert!(check_name(&name_for_branch(&"a/".repeat(40))).is_ok());
        assert!(check_name("Hello").is_err());
        assert!(check_name("-x").is_err());
        assert!(check_name("x").is_ok());
    }
}
