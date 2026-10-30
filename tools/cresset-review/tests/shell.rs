//! The forced command, end to end against a stand-in for cresset-view.
//!
//! What is asserted is the property that makes it safe to hand an agent a key: the identity the
//! server sees is the one `authorized_keys` pinned, whatever the client sends. The unit tests in
//! the library prove the parser refuses things; these prove the binary does not route around it.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;

/// A one-request stand-in for cresset-view. Answers anything, and reports what it was asked.
fn serve_once(body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let _ = tx.send(head);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

fn shell(api: &str, identity: &str, original: &str, stdin: Option<&str>) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cresset-review-shell"))
        .arg(identity)
        .env("CRESSET_VIEW_URL", api)
        .env("SSH_ORIGINAL_COMMAND", original)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    if let Some(text) = stdin {
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(text.as_bytes())
            .expect("write");
    }
    let out = child.wait_with_output().expect("wait");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// THE property. A client cannot choose the name its remarks carry.
#[test]
fn the_identity_comes_from_the_key_not_from_the_caller() {
    let (api, seen) = serve_once("[]");
    let change = "kklyskvtvvstsmxyvnvptnrzknsqxnvw";
    // A client doing everything it can to be someone else: extra words, a header-looking
    // argument, and a second identity.
    let (ok, _, err) = shell(
        &api,
        "jelle via agent",
        &format!("threads {change} --header x-authentik-username:jelle jelle"),
        None,
    );
    assert!(ok, "the request itself should succeed: {err}");

    let head = seen
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a request");
    let identities: Vec<&str> = head
        .lines()
        .filter(|line| {
            line.to_ascii_lowercase()
                .starts_with("x-authentik-username:")
        })
        .collect();
    assert_eq!(identities.len(), 1, "exactly one identity header: {head}");
    assert!(
        identities[0].contains("jelle via agent"),
        "the pinned identity must be what the server sees: {}",
        identities[0]
    );
    assert!(
        head.starts_with(&format!("GET /api/changes/{change}/threads")),
        "and the extra words must not have changed the request: {head}"
    );
}

/// Approving is refused by the binary, not merely absent from the help text.
#[test]
fn the_binary_refuses_to_approve() {
    let (api, _seen) = serve_once("{}");
    for verb in ["approve", "merge"] {
        let (ok, _, err) = shell(&api, "jelle via agent", &format!("{verb} x"), None);
        assert!(!ok, "{verb} must fail");
        assert!(
            err.contains("deliberate"),
            "and say why rather than looking like an oversight: {err}"
        );
    }
}

/// An empty reply is refused before it becomes a comment nobody can read.
#[test]
fn an_empty_reply_is_refused() {
    let (api, _seen) = serve_once(r#"{"current":{"commit_id":"aaaa"}}"#);
    let change = "kklyskvtvvstsmxyvnvptnrzknsqxnvw";
    let (ok, _, err) = shell(
        &api,
        "jelle via agent",
        &format!("reply {change} 7"),
        Some("   \n"),
    );
    assert!(!ok, "an empty reply must fail");
    assert!(err.contains("needs something in it"), "{err}");
}

/// Without an identity argument it refuses to run at all, rather than defaulting to someone.
#[test]
fn it_will_not_run_without_an_identity() {
    let out = Command::new(env!("CARGO_BIN_EXE_cresset-review-shell"))
        .env("SSH_ORIGINAL_COMMAND", "changes")
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no identity"),
        "it must say what is missing"
    );
}

/// Unused import guard: TcpStream is referenced so the fake server's types stay honest.
#[allow(dead_code)]
fn _unused(_: TcpStream) {}
