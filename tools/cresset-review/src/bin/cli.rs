//! `cresset-review`: read and answer review comments from a terminal.
//!
//! Shaped like git — it runs where you are and talks to the server over ssh. There is no token,
//! no config file and no local state: the ssh key is the whole of the authentication, and the
//! server decides what name your remarks carry.

use std::os::unix::process::CommandExt;
use std::process::Stdio;

use cresset_review::{Command, USAGE};

const DEFAULT_REMOTE: &str = "cresset-review-cli@internal.cresset.tools";

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let borrowed: Vec<&str> = words.iter().map(String::as_str).collect();

    // Parsed locally as well as on the server, so a typo answers instantly and a malformed id
    // never becomes a network round trip. The server parses it again regardless — this is a
    // convenience, and the check that matters is the one the client cannot skip.
    let command = match Command::parse(&borrowed) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("cresset-review: {error:#}");
            std::process::exit(2);
        }
    };
    if matches!(command, Command::Help) {
        print!("{USAGE}");
        return;
    }

    let remote =
        std::env::var("CRESSET_REVIEW_REMOTE").unwrap_or_else(|_| DEFAULT_REMOTE.to_owned());

    let mut ssh = std::process::Command::new("ssh");
    ssh.arg("-o").arg("BatchMode=yes");

    // The CLI's own key when it is there. Agents on this box run as the user and would
    // otherwise authenticate with their push key, so everything a machine wrote would carry a
    // person's name. With this key the server attributes it as "jelle via agent" instead.
    if let Some(home) = std::env::var_os("HOME") {
        let key = std::env::var_os("CRESSET_REVIEW_KEY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(&home).join(".ssh/cresset-review"));
        if key.is_file() {
            ssh.arg("-i").arg(&key).arg("-o").arg("IdentitiesOnly=yes");
        }
    }

    ssh.arg(&remote).args(&words);

    // Only `reply` reads a body, and it reads it from our stdin. Everything else gets /dev/null
    // so ssh does not sit holding the terminal open waiting for input that never comes.
    if !matches!(command, Command::Reply { .. }) {
        ssh.stdin(Stdio::null());
    }

    // exec rather than spawn-and-wait: ssh's exit status becomes ours, so a refusal from the
    // server is a failing command here too rather than a success with an error printed.
    let error = ssh.exec();
    eprintln!("cresset-review: could not run ssh: {error}");
    std::process::exit(127);
}
