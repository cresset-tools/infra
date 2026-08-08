//! The revision endpoints, against a real jj repository.
//!
//! This is the screen the sync worker's escalation path arrives at: the worker pauses every
//! project, Telegram carries a pointer to exactly this URL, and until 2026-08-08 the answer was
//! "This path contains an unresolved jj conflict." and nothing else. What makes the screen
//! worth anything is the sides, so that is what this pins.
//!
//! The fixture is built with the `jj` CLI rather than jj-lib, matching how cresset-sync's tests
//! drive `git`: a conflict constructed through the same interface a person uses is a conflict
//! of the shape they will actually meet.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Run `jj` in `dir`, returning stdout. Panics with stderr on failure, because a fixture that
/// half-built produces a test failure pointing at the assertion rather than at the cause.
fn jj(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Test")
        .env("JJ_EMAIL", "test@example.com")
        .output()
        .unwrap_or_else(|e| panic!("running jj {args:?}: {e}"));
    assert!(
        out.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The two revisions the tests need out of the fixture.
struct Fixture {
    /// The merge commit, conflicted on one line of `f.txt`.
    conflicted: String,
    /// The clean base commit.
    base: String,
}

/// A repository whose head is a merge conflicting on one line of `f.txt`.
fn conflicted_repo(dir: &Path) -> Fixture {
    jj(dir, &["git", "init", "--colocate", "."]);
    std::fs::write(dir.join("f.txt"), "line1\nbase\nline3\n").expect("write base");
    jj(dir, &["commit", "-m", "base"]);
    let base = jj(
        dir,
        &["log", "-r", "@-", "--no-graph", "-T", "change_id.short()"],
    );
    // Captured here rather than looked up by description later: a revset that quietly matches
    // nothing produces an empty id and a failure that points at the request, not the lookup.
    let base_commit = jj(dir, &["log", "-r", "@-", "--no-graph", "-T", "commit_id"]);

    jj(dir, &["new", &base, "-m", "ours"]);
    std::fs::write(dir.join("f.txt"), "line1\nOURS\nline3\n").expect("write ours");
    let ours = jj(
        dir,
        &["log", "-r", "@", "--no-graph", "-T", "change_id.short()"],
    );

    jj(dir, &["new", &base, "-m", "theirs"]);
    std::fs::write(dir.join("f.txt"), "line1\nTHEIRS\nline3\n").expect("write theirs");
    let theirs = jj(
        dir,
        &["log", "-r", "@", "--no-graph", "-T", "change_id.short()"],
    );

    // Rebase rather than merge, because that is how the worker makes conflicts: it replays
    // downstream commits onto `main`. It matters for more than realism — a MERGE commit that
    // only conflicts has an empty diff against its merged parents, so the Changes view has
    // nothing to show and a test built on one cannot see the defect it is meant to catch.
    jj(dir, &["rebase", "-s", &theirs, "-d", &ours]);
    let conflicted = jj(
        dir,
        &["log", "-r", "conflicts()", "--no-graph", "-T", "commit_id"],
    );
    assert!(
        !conflicted.is_empty(),
        "the fixture must actually conflict, or this test proves nothing"
    );
    Fixture {
        conflicted,
        base: base_commit,
    }
}

/// A port nobody is listening on. Bound and released, so there is a small race with anything
/// else on the machine — acceptable for a test, and far simpler than plumbing the bound port
/// back out of the server.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(repository: &Path) -> Self {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_cresset-view"))
            .arg("--repository")
            .arg(repository)
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            // The API needs no assets; ServeDir simply 404s for a directory that is not there.
            .arg("--assets")
            .arg(repository.join("no-assets"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cresset-view");
        let server = Server { child, port };
        server.wait_until_ready();
        server
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.get("/health").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("cresset-view did not become ready");
    }

    /// The status code and body of a GET, for cases where the failure is the point.
    fn get_status(&self, path: &str) -> (u16, String) {
        let raw = self.request(path).unwrap_or_default();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        (status, body.to_string())
    }

    /// A minimal HTTP/1.1 GET. Deliberately dependency-free: pulling in an HTTP client to make
    /// one request against our own server would be more moving parts than the request.
    fn get(&self, path: &str) -> Option<String> {
        let raw = self.request(path)?;
        let (head, body) = raw.split_once("\r\n\r\n")?;
        head.starts_with("HTTP/1.1 200").then(|| body.to_string())
    }

    fn request(&self, path: &str) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                     x-authentik-username: test\r\n\r\n"
                )
                .as_bytes(),
            )
            .ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        Some(response)
    }
}

#[test]
fn a_conflicted_path_serves_its_sides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let commit = conflicted_repo(dir.path()).conflicted;
    let server = Server::start(dir.path());

    let body = server
        .get(&format!("/api/revisions/{commit}/file?path=f.txt"))
        .expect("the conflicted file must be served");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");

    assert_eq!(value["conflicted"], true);
    let conflict = &value["conflict"];
    assert!(
        !conflict.is_null(),
        "a conflicted path must carry its sides, not just the fact that it conflicts; got {value}"
    );

    // One common ancestor, two competing versions — the ordinary 3-way shape.
    let bases = conflict["bases"].as_array().expect("bases");
    let sides = conflict["sides"].as_array().expect("sides");
    assert_eq!(bases.len(), 1, "one common ancestor");
    assert_eq!(sides.len(), 2, "two competing versions");

    // The CONTENT is the point. Without it this is the old dead end wearing more JSON.
    assert!(
        bases[0]["contents"].as_str().unwrap_or("").contains("base"),
        "the ancestor's content must be readable: {bases:?}"
    );
    let side_text: Vec<&str> = sides
        .iter()
        .map(|side| side["contents"].as_str().unwrap_or(""))
        .collect();
    assert!(
        side_text.iter().any(|text| text.contains("OURS")),
        "one side must carry OURS: {side_text:?}"
    );
    assert!(
        side_text.iter().any(|text| text.contains("THEIRS")),
        "the other side must carry THEIRS: {side_text:?}"
    );

    // Labels come from the COMMIT's own conflict shape. Without passing those through, every
    // side renders as an anonymous "side #1" — the difference between "two versions differ" and
    // "the monorepo says this, the downstream says that".
    for side in sides {
        let label = side["label"].as_str().unwrap_or("");
        assert!(
            !label.is_empty(),
            "each side must be labelled with where it came from; got {side:?}"
        );
    }

    // jj's own materialisation, which is what a resolver actually edits.
    let materialized = conflict["materialized"].as_str().unwrap_or("");
    assert!(
        materialized.contains("OURS") && materialized.contains("THEIRS"),
        "the materialised form must show both sides: {materialized}"
    );
}

/// A path with no conflict must not grow a conflict field.
#[test]
fn an_ordinary_path_carries_no_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = conflicted_repo(dir.path()).base;
    let server = Server::start(dir.path());

    let body = server
        .get(&format!("/api/revisions/{base}/file?path=f.txt"))
        .expect("the clean file must be served");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");

    assert_eq!(value["conflicted"], false);
    assert!(
        value["conflict"].is_null(),
        "a clean path must not carry a conflict: {value}"
    );
    assert!(
        value["contents"].as_str().unwrap_or("").contains("base"),
        "a clean path still serves its contents: {value}"
    );
}

/// A DIRECTORY is a legitimate thing to ask a diff for.
///
/// Asking for one used to fail with `400 requested path is not changed in this revision`, which
/// is both unhelpful and — for a directory that plainly does contain changes — untrue; it reads
/// as though the viewer disagrees with you about the repository. Worse, the frontend rethrew the
/// failure into a `void`-ed call, so the pane sat at "Loading comparison…" for ever while the
/// console collected an unhandled rejection.
#[test]
fn a_diff_can_start_at_a_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    jj(dir.path(), &["git", "init", "--colocate", "."]);
    std::fs::create_dir_all(dir.path().join("nested/deeper")).expect("mkdir");
    std::fs::write(dir.path().join("nested/deeper/a.txt"), "one\n").expect("write");
    jj(dir.path(), &["commit", "-m", "add a nested file"]);
    std::fs::write(dir.path().join("nested/deeper/a.txt"), "two\n").expect("write");
    jj(dir.path(), &["commit", "-m", "change it"]);
    let changed = jj(
        dir.path(),
        &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
    );

    let server = Server::start(dir.path());

    // The exact file still works.
    let (status, _) = server.get_status(&format!(
        "/api/revisions/{changed}/diff?path=nested%2Fdeeper%2Fa.txt"
    ));
    assert_eq!(status, 200, "an exact path must work");

    // ...and so must each directory above it.
    for directory in ["nested", "nested%2Fdeeper", "nested%2F"] {
        let (status, body) =
            server.get_status(&format!("/api/revisions/{changed}/diff?path={directory}"));
        assert_eq!(
            status, 200,
            "a diff starting at directory {directory:?} must be served, got {status}: {body}"
        );
    }

    // A path with genuinely nothing under it is still an error — and says so in those terms.
    let (status, body) = server.get_status(&format!("/api/revisions/{changed}/diff?path=absent"));
    assert_eq!(status, 400, "an unchanged path is still a client error");
    assert!(
        body.contains("nothing under absent changed"),
        "the message must name what was actually asked for: {body}"
    );
}

/// The Changes view must show a conflicted path AS a conflict, not as a deletion.
///
/// The diff stream reported an unresolved side as `null`, so a conflicted file rendered as
/// though its content had been removed. That is the one view where "what happened to this
/// file" is the whole question, and it answered with silence — while the Browse view, which
/// someone reaches second, showed the conflict in full.
#[test]
fn a_conflicted_path_shows_its_markers_in_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let commit = conflicted_repo(dir.path()).conflicted;
    let server = Server::start(dir.path());

    let body = server
        .get(&format!("/api/revisions/{commit}/diff?path=f.txt"))
        .expect("the diff must be served");

    // The stream is ndjson: metadata first, then one event per file.
    let file_event = body
        .lines()
        .find(|line| line.contains("\"type\":\"file\""))
        .unwrap_or_else(|| panic!("no file event in the stream: {body}"));
    let event: serde_json::Value = serde_json::from_str(file_event).expect("valid json");

    assert_eq!(
        event["conflicted"], true,
        "the event must say it is conflicted"
    );
    let after = event["after"]
        .as_str()
        .unwrap_or_else(|| panic!("`after` must not be null for a conflict: {event}"));
    assert!(
        after.contains("OURS") && after.contains("THEIRS"),
        "the diff's after-side must carry both versions, not a deletion: {after}"
    );
    assert!(
        after.contains("<<<<<<<"),
        "it must be jj's marker text, so the diff reads as a conflict: {after}"
    );
}
