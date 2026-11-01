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

/// Invoke the binary the way sshd does.
///
/// sshd runs a forced command through a shell, so `command="... jelle via agent"` arrives as
/// SEPARATE arguments. Passing the identity as one argument containing spaces — which is what
/// this helper used to do — is the one thing the real path never does, and it hid a bug that
/// recorded every agent comment under a person's name.
fn shell(api: &str, identity: &str, original: &str, stdin: Option<&str>) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cresset-review-shell"))
        .args(identity.split_whitespace())
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

// ---------------------------------------------------------------------------------------------
// `wait`
//
// Driven against a stand-in that answers by path and can be changed WHILE the wait is running,
// because every interesting property of a watcher is about what it does when something moves.
// The loop itself is the real one: `CRESSET_REVIEW_POLL_MS` shortens its interval and changes
// nothing else, so these exercise the code that runs on the host rather than a copy of it.
// ---------------------------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

const CHANGE: &str = "kklyskvtvvstsmxyvnvptnrzknsqxnvw";
const COMMIT_1: &str = "1111111111111111111111111111111111111111";
const COMMIT_2: &str = "2222222222222222222222222222222222222222";

/// What the stand-in answers with. The queue is a script: each request takes the next entry and
/// the last one repeats, which is how a snapshot catching up with a push is reproduced.
struct Answers {
    queue: Vec<String>,
    approvals: String,
    threads: String,
}

fn queue(commit: &str, patch_id: Option<&str>, patch_sets: u32) -> String {
    let patch = match patch_id {
        Some(id) => format!(r#""patch_id":"{id}","#),
        None => String::new(),
    };
    format!(
        r#"{{"operation_id":"op","stacks":[{{"bookmark":"review/x","tip":"{commit}",
           "behind_main":false,"changes":[{{"change_id":"{CHANGE}","commit_id":"{commit}",
           {patch}"patch_sets":{patch_sets},"has_conflict":false,"description":"d",
           "author_name":"a","authored_at":"t"}}]}}]}}"#
    )
}

fn approvals(rows: &[(&str, &str, &str)]) -> String {
    let rows: Vec<String> = rows
        .iter()
        .map(|(commit, patch_id, who)| {
            format!(
                r#"{{"change_id":"{CHANGE}","commit_id":"{commit}","patch_id":"{patch_id}",
                   "approved_by":"{who}","created_at":1}}"#
            )
        })
        .collect();
    format!(
        r#"{{"change_id":"{CHANGE}","approvals":[{}],"gated":true}}"#,
        rows.join(",")
    )
}

fn threads(comments: &[(i64, &str)]) -> String {
    let comments: Vec<String> = comments
        .iter()
        .map(|(id, author)| {
            format!(
                r#"{{"id":{id},"body":"please rename this","author":"{author}",
                   "patch_set_commit_id":"{COMMIT_1}","created_at":1}}"#
            )
        })
        .collect();
    if comments.is_empty() {
        return "[]".to_owned();
    }
    format!(
        r#"[{{"id":7,"change_id":"{CHANGE}","path":"src/main.rs","side":"new","line":12,
           "fingerprint":"f","context":"[]","resolved":false,"created_by":"jelle",
           "created_at":1,"comments":[{}]}}]"#,
        comments.join(",")
    )
}

/// A stand-in for cresset-view: routes by path, serves whatever `Answers` currently says.
fn serve(answers: Answers) -> (String, Arc<Mutex<Answers>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let shared = Arc::new(Mutex::new(answers));
    let answers = Arc::clone(&shared);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("request line");
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
            let body = {
                let mut answers = answers.lock().expect("lock");
                if path.ends_with("/approvals") {
                    answers.approvals.clone()
                } else if path.ends_with("/threads") {
                    answers.threads.clone()
                } else if answers.queue.len() > 1 {
                    answers.queue.remove(0)
                } else {
                    answers.queue[0].clone()
                }
            };
            // `connection: close` so ureq does not pool a socket this server has hung up on.
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}"), shared)
}

/// Run `wait` against a stand-in, optionally changing what it answers partway through.
fn run_wait(
    api: &str,
    identity: &str,
    seconds: &str,
    poll_ms: u64,
) -> (Option<i32>, serde_json::Value, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cresset-review-shell"))
        .args(identity.split_whitespace())
        .env("CRESSET_VIEW_URL", api)
        .env("SSH_ORIGINAL_COMMAND", format!("wait {CHANGE} {seconds}"))
        .env("CRESSET_REVIEW_POLL_MS", poll_ms.to_string())
        .stdin(Stdio::null())
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let json = serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
    (
        out.status.code(),
        json,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Approved means the gate will accept it, and that is the only thing that exits 0.
#[test]
fn an_approval_of_the_current_patch_set_ends_the_wait() {
    let (api, _answers) = serve(Answers {
        queue: vec![queue(COMMIT_1, Some("p1"), 1)],
        approvals: approvals(&[(COMMIT_1, "p1", "jelle")]),
        threads: threads(&[]),
    });
    let (code, json, err) = run_wait(&api, "jelle via agent", "20", 50);
    assert_eq!(code, Some(0), "approval is the one success: {err}");
    assert_eq!(json["outcome"], "approved");
    assert_eq!(json["approvals"][0]["approved_by"], "jelle");
    assert_eq!(json["patch_id"], "p1");
    assert_eq!(json["bookmark"], "review/x");
    assert_eq!(json["gated"], true);
}

/// THE property this verb exists for, and the one a naive version gets wrong.
///
/// The viewer serves a snapshot refreshed when a push wakes it, so a wait started seconds after
/// pushing can see the previous patch set as current. If an approval of that one counted, an
/// agent would be told to land work nobody had read — and the push gate, which keys on the diff,
/// would refuse it. So: approval of patch set 1 plus a current patch set 2 is not approval.
#[test]
fn an_approval_of_the_previous_patch_set_does_not_count() {
    let (api, _answers) = serve(Answers {
        // The first reading is the stale one; every reading after it has the pushed revision.
        queue: vec![
            queue(COMMIT_1, Some("p1"), 1),
            queue(COMMIT_2, Some("p2"), 2),
        ],
        approvals: approvals(&[(COMMIT_1, "p1", "jelle")]),
        threads: threads(&[]),
    });
    let (code, json, err) = run_wait(&api, "jelle via agent", "1", 50);
    assert_eq!(code, Some(3), "not approved must not exit 0: {err} {json}");
    assert_eq!(json["outcome"], "pending");
    assert_eq!(json["patch_id"], "p2");
    assert!(
        json["approvals"].is_null(),
        "nothing approves the current diff: {json}"
    );
    assert_eq!(
        json["approvals_of_other_versions"][0]["patch_id"], "p1",
        "and the approval that does exist must still be reported: {json}"
    );
    assert!(
        err.contains("patch set 2 is current now"),
        "the move should be said out loud: {err}"
    );
}

/// A reviewer who asks a question is not going to approve until it is answered, so waiting for
/// approval would be waiting for the wrong thing.
#[test]
fn a_comment_that_was_not_there_before_ends_the_wait() {
    let (api, answers) = serve(Answers {
        queue: vec![queue(COMMIT_1, Some("p1"), 1)],
        approvals: approvals(&[]),
        // Already there when the wait starts, so not news.
        threads: threads(&[(10, "jelle")]),
    });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        answers.lock().expect("lock").threads = threads(&[(10, "jelle"), (11, "jelle")]);
    });
    let (code, json, err) = run_wait(&api, "jelle via agent", "20", 50);
    assert_eq!(code, Some(3), "a question is not an approval: {err}");
    assert_eq!(json["outcome"], "commented");
    let comments = json["comments"].as_array().expect("comments");
    assert_eq!(
        comments.len(),
        1,
        "only the new one: the thread it is in was already open when the wait started: {json}"
    );
    assert_eq!(comments[0]["id"], 11);
    assert_eq!(comments[0]["path"], "src/main.rs");
    assert_eq!(comments[0]["line"], 12);
}

/// An agent answering a thread must not wake its own watcher: it would return instantly, every
/// time, having learnt nothing.
#[test]
fn a_reply_from_the_waiting_identity_does_not_end_the_wait() {
    let (api, answers) = serve(Answers {
        queue: vec![queue(COMMIT_1, Some("p1"), 1)],
        approvals: approvals(&[]),
        threads: threads(&[(10, "jelle")]),
    });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        answers.lock().expect("lock").threads = threads(&[(10, "jelle"), (11, "jelle via agent")]);
    });
    let (code, json, err) = run_wait(&api, "jelle via agent", "1", 50);
    assert_eq!(code, Some(3), "{err}");
    assert_eq!(
        json["outcome"], "pending",
        "our own reply is not somebody asking us something: {json}"
    );
}

/// It landed, or its bookmark was deleted. Either way there is nothing left to wait for.
#[test]
fn a_change_that_has_left_the_queue_ends_the_wait() {
    let (api, _answers) = serve(Answers {
        queue: vec![r#"{"operation_id":"op","stacks":[]}"#.to_owned()],
        approvals: approvals(&[]),
        threads: threads(&[]),
    });
    let (code, json, err) = run_wait(&api, "jelle via agent", "20", 50);
    assert_eq!(code, Some(3), "{err}");
    assert_eq!(json["outcome"], "gone");
    assert!(
        json["note"].as_str().expect("note").contains("landed"),
        "and say what that probably means: {json}"
    );
}

/// The queue reader, against the response the running service actually gives.
///
/// `tests/live-queue.json` is a verbatim capture from <https://code.cresset.tools> rather than a
/// body written here to match what this file expects. Four of today's failures in this
/// repository came from a fixture that was not shaped like production — a flake check without
/// `--shared=group`, a unit's PATH, a binary invoked the way no caller invokes it — so the one
/// reading that has to survive a server-side change is pinned to a real one. It will go stale as
/// those changes land, and that is fine: what it proves is the SHAPE, including the fields this
/// deserializer deliberately ignores.
#[test]
fn the_queue_reader_handles_the_real_response() {
    let live = include_str!("live-queue.json").to_owned();
    let (api, _answers) = serve(Answers {
        queue: vec![live],
        approvals: approvals(&[]),
        threads: threads(&[]),
    });
    let read = |change: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_cresset-review-shell"))
            .args(["jelle"])
            .env("CRESSET_VIEW_URL", &api)
            .env("SSH_ORIGINAL_COMMAND", format!("wait {change} 0"))
            .stdin(Stdio::null())
            .output()
            .expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        serde_json::from_str::<serde_json::Value>(stdout.trim()).unwrap_or(serde_json::Value::Null)
    };

    let relay = read("orlrylqrpuwokysqotsysunnptltnmxu");
    assert_eq!(relay["outcome"], "pending");
    assert_eq!(relay["bookmark"], "review/relay-0.1.7");
    assert_eq!(
        relay["patch_id"],
        "72e577ea7d2ad4bd1e3e761594cdb183ccb168b9"
    );
    assert_eq!(relay["patch_sets"], 1);
    assert_eq!(relay["behind_main"], false);

    // The other stack in the same capture has fallen behind main, which an approved change still
    // has to be told about: approval does not make a push fast-forward.
    let sconce = read("tvvoxytqlzmylrotlsppzmqtossvourx");
    assert_eq!(sconce["bookmark"], "review/sconce-custom-domains");
    assert_eq!(sconce["behind_main"], true);

    // And a change the queue does not carry is gone, not an error.
    assert_eq!(read(CHANGE)["outcome"], "gone");
}

/// `wait <id> 0` is a status check: one read, no waiting, and it admits that is all it is.
#[test]
fn a_zero_deadline_reads_once_and_reports() {
    let (api, _answers) = serve(Answers {
        queue: vec![queue(COMMIT_1, Some("p1"), 1)],
        approvals: approvals(&[]),
        threads: threads(&[]),
    });
    let started = std::time::Instant::now();
    let (code, json, err) = run_wait(&api, "jelle", "0", 50);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "it must not wait at all"
    );
    assert_eq!(code, Some(3));
    assert_eq!(json["outcome"], "pending");
    assert_eq!(json["identity"], "jelle");
    assert!(
        err.contains("read once"),
        "a single read cannot be confirmed and should say so: {err}"
    );
}

/// Nothing can ever approve a patch set the service cannot describe, so the wait must refuse
/// rather than sit there for a day proving it.
#[test]
fn a_patch_set_that_cannot_be_read_fails_instead_of_waiting() {
    let (api, _answers) = serve(Answers {
        queue: vec![queue(COMMIT_1, None, 1)],
        approvals: approvals(&[]),
        threads: threads(&[]),
    });
    let started = std::time::Instant::now();
    let (code, _json, err) = run_wait(&api, "jelle via agent", "600", 50);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "and fail at once"
    );
    assert!(
        err.contains("merge commit") && err.contains("git store"),
        "naming both ways it happens: {err}"
    );

    // Including the one-shot check, which would otherwise answer "pending" — true, and an
    // answer to the wrong question.
    let (code, _json, err) = run_wait(&api, "jelle via agent", "0", 50);
    assert_eq!(code, Some(1), "a status check must refuse it too: {err}");
}
