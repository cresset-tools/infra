//! The ssh forced command: the server side of `cresset-review`.
//!
//! Invoked by sshd as `cresset-review-shell <identity>`, with the client's words in
//! `SSH_ORIGINAL_COMMAND`. The identity is an argument from `authorized_keys` and the client
//! cannot reach it; see the crate docs for why that is the whole security property.
//!
//! It talks to cresset-view over loopback rather than opening `review.db`. A second process
//! reading a WAL database owned by another user wants write access to the directory to create
//! the `-shm` index — precisely what leaves `/api/sync` reporting no projects today. One HTTP
//! call costs nothing and inherits every rule the service already enforces.

use std::io::Read;

use anyhow::{Context, Result, bail};
use cresset_review::{Command, USAGE};

fn main() {
    if let Err(error) = run() {
        // `{:#}` so the context chain is shown: "resolving the current patch set: 404" says
        // where to look, and "404" alone does not.
        eprintln!("cresset-review: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let identity = std::env::args()
        .nth(1)
        .filter(|identity| !identity.is_empty())
        .context("no identity: this binary is an ssh forced command and is not useful by hand")?;

    // Whatever the client sent. Split on whitespace and matched against a fixed set — never
    // evaluated, never passed to a shell.
    let original = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
    let words: Vec<&str> = original.split_whitespace().collect();

    let api =
        std::env::var("CRESSET_VIEW_URL").unwrap_or_else(|_| "http://127.0.0.1:9080".to_owned());
    let client = Client { api, identity };

    match Command::parse(&words)? {
        Command::Help => print!("{USAGE}"),
        Command::Changes => println!("{}", client.get("/api/changes")?),
        Command::Threads { change } => {
            println!("{}", client.get(&format!("/api/changes/{change}/threads"))?)
        }
        Command::Reply { change, thread } => {
            // Which patch set a reply was written against is resolved HERE, not taken from the
            // caller, so a stale client cannot attribute a remark to a version it never read.
            let detail: serde_json::Value =
                serde_json::from_str(&client.get(&format!("/api/changes/{change}"))?)
                    .context("reading the change")?;
            let patch_set = detail["current"]["commit_id"]
                .as_str()
                .context("the change has no current patch set to answer against")?;

            let mut body = String::new();
            std::io::stdin()
                .read_to_string(&mut body)
                .context("reading the comment from stdin")?;
            if body.trim().is_empty() {
                bail!("a reply needs something in it");
            }

            let payload = serde_json::json!({ "body": body, "patch_set_commit_id": patch_set });
            println!(
                "{}",
                client.post(&format!("/api/threads/{thread}/comments"), &payload)?
            );
        }
        Command::Resolve { thread, resolved } => {
            let payload = serde_json::json!({ "resolved": resolved });
            println!(
                "{}",
                client.post(&format!("/api/threads/{thread}/resolve"), &payload)?
            );
        }
    }
    Ok(())
}

struct Client {
    api: String,
    identity: String,
}

impl Client {
    fn get(&self, path: &str) -> Result<String> {
        self.send(ureq::get(&format!("{}{path}", self.api)), None, path)
    }

    fn post(&self, path: &str, payload: &serde_json::Value) -> Result<String> {
        self.send(
            ureq::post(&format!("{}{path}", self.api)),
            Some(payload),
            path,
        )
    }

    fn send(
        &self,
        request: ureq::Request,
        payload: Option<&serde_json::Value>,
        path: &str,
    ) -> Result<String> {
        // The identity this service was started with, not anything from the request. There is
        // no code path that puts a client's word in this header.
        let request = request
            .set("x-authentik-username", &self.identity)
            .timeout(std::time::Duration::from_secs(30));
        let response = match payload {
            Some(payload) => request.send_json(payload),
            None => request.call(),
        };
        match response {
            Ok(response) => response.into_string().context("reading the response"),
            // cresset-view puts its reason in the body, and the reason is the useful part:
            // "a comment needs something in it" explains, "400" does not.
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                bail!("{path}: {status} {}", body.trim())
            }
            Err(error) => Err(error).with_context(|| format!("calling {path}")),
        }
    }
}
