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
        .env("HOME", dir)
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

/// A repository shaped like production: a bare remote the viewer fetches from, so review
/// bookmarks arrive as REMOTE bookmarks and patch sets as `refs/changes/*`.
///
/// The shape matters. Reading only `local_bookmarks()` returns an empty queue against a real
/// viewer repository, because that repository only ever fetches and so never has a local
/// review bookmark. A fixture that pushed nowhere would have passed anyway.
fn review_repo(dir: &Path) -> (String, String) {
    let bare = dir.join("canonical.git");
    let work = dir.join("work");
    let out = Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(&bare)
        .output()
        .expect("init bare");
    assert!(out.status.success());

    std::fs::create_dir_all(&work).expect("mkdir work");
    jj(&work, &["git", "init", "--colocate", "."]);
    std::fs::write(work.join("f.txt"), "base\n").expect("write");
    jj(&work, &["commit", "-m", "base"]);
    jj(&work, &["bookmark", "set", "main", "-r", "@-"]);
    jj(
        &work,
        &["git", "remote", "add", "canonical", bare.to_str().unwrap()],
    );
    jj(&work, &["git", "push", "-b", "main"]);

    jj(&work, &["new", "main", "-m", "a change under review"]);
    std::fs::write(work.join("g.txt"), "one\n").expect("write");
    jj(&work, &["bookmark", "set", "review/thing", "-r", "@"]);
    jj(&work, &["git", "push", "-b", "review/thing"]);
    let change = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "change_id"]);
    let commit = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);

    // Pin the patch set the way the canonical repo's post-receive hook does. That hook is
    // exercised by `nix flake check`; here we only care that the viewer READS what it writes.
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["update-ref", &format!("refs/changes/{change}/1"), &commit])
        .output()
        .expect("pin patch set");
    assert!(out.status.success());

    // The viewer's repository: a fetching clone, exactly as cresset-view-refresh maintains.
    let viewer = dir.join("viewer");
    let out = Command::new("jj")
        .args(["git", "clone", "--colocate"])
        .arg(&bare)
        .arg(&viewer)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Test")
        .env("JJ_EMAIL", "test@example.com")
        // jj writes a per-repo config under HOME. The Nix sandbox provides none, and without
        // it the clone fails while printing output that reads like success.
        .env("HOME", dir)
        .output()
        .expect("clone viewer");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new("git")
        .arg("-C")
        .arg(&viewer)
        .args(["fetch", "-q", "origin", "+refs/changes/*:refs/changes/*"])
        .output()
        .expect("fetch patch sets");
    assert!(out.status.success());

    (change, commit)
}

#[test]
fn the_queue_lists_changes_that_have_not_landed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, commit) = review_repo(dir.path());
    let server = Server::start(&dir.path().join("viewer"));

    let body = server
        .get("/api/changes")
        .expect("the queue must be served");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let changes = value["changes"].as_array().expect("changes array");

    assert_eq!(
        changes.len(),
        1,
        "one change is open; `main` must not be listed: {value}"
    );
    assert_eq!(changes[0]["change_id"], change);
    assert_eq!(changes[0]["commit_id"], commit);
    assert_eq!(changes[0]["patch_sets"], 1);
    assert_eq!(
        changes[0]["bookmark"], "review/thing",
        "the review bookmark is REMOTE in a fetching repo, which is all the viewer ever has"
    );
}

#[test]
fn a_change_serves_every_patch_set_with_the_current_one_marked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, first) = review_repo(dir.path());

    // A second patch set, pinned as the hook would on re-push.
    let bare = dir.path().join("canonical.git");
    let work = dir.path().join("work");
    std::fs::write(work.join("g.txt"), "one\ntwo\n").expect("write");
    jj(&work, &["bookmark", "set", "review/thing", "-r", "@"]);
    jj(&work, &["git", "push", "-b", "review/thing"]);
    let second = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    assert_ne!(first, second, "the amend must produce a new commit id");
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["update-ref", &format!("refs/changes/{change}/2"), &second])
        .output()
        .expect("pin patch set 2");
    assert!(out.status.success());

    let viewer = dir.path().join("viewer");
    let out = Command::new("git")
        .arg("-C")
        .arg(&viewer)
        .args(["fetch", "-q", "origin", "+refs/changes/*:refs/changes/*"])
        .output()
        .expect("fetch patch sets");
    assert!(out.status.success());
    let out = Command::new("jj")
        .args(["git", "fetch"])
        .current_dir(&viewer)
        .env("JJ_CONFIG", "/dev/null")
        .env("HOME", dir.path())
        .output()
        .expect("jj fetch");
    assert!(out.status.success());

    let server = Server::start(&viewer);
    let body = server
        .get(&format!("/api/changes/{change}"))
        .expect("the change must be served");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let sets = value["patch_sets"].as_array().expect("patch_sets");

    assert_eq!(
        sets.len(),
        2,
        "both versions must be offered, not just the newest: {value}"
    );
    assert_eq!(sets[0]["number"], 1);
    assert_eq!(sets[0]["commit_id"], first);
    assert_eq!(sets[0]["current"], false, "patch set 1 has been superseded");
    assert_eq!(sets[1]["number"], 2);
    assert_eq!(
        sets[1]["current"], true,
        "the reviewer must know which one would land"
    );

    // The superseded commit is on no branch, and is still readable. That is the entire reason
    // patch sets are pinned.
    assert_eq!(value["change_id"], change);
}

/// A superseded patch set must be READABLE, not merely listed.
///
/// Patch sets live outside `refs/heads` so that importing them cannot make a change id
/// divergent. The cost of that choice is that jj does not consider those commits visible, so
/// the ordinary revset walk never finds them and the diff endpoint answered "revision does not
/// resolve to a visible jj commit" — for the exact commits the system pins on purpose. Being
/// able to read the version a comment was written against is the point of keeping it.
#[test]
fn a_superseded_patch_set_can_still_be_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, first) = review_repo(dir.path());

    // Revise the change so patch set 1 is superseded and on no branch.
    let work = dir.path().join("work");
    std::fs::write(work.join("g.txt"), "one\ntwo\n").expect("write");
    jj(&work, &["bookmark", "set", "review/thing", "-r", "@"]);
    jj(&work, &["git", "push", "-b", "review/thing"]);
    // Pin the new patch set as the post-receive hook does, so the change has both versions.
    let second = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir.path().join("canonical.git"))
        .args(["update-ref", &format!("refs/changes/{change}/2"), &second])
        .output()
        .expect("pin patch set 2");
    assert!(out.status.success());

    // Re-clone AFTER the revision, so this repository never saw patch set 1 as a bookmark and
    // jj therefore does not consider it visible. That is the ordinary case in production: the
    // viewer refreshes every two minutes, so a change amended between refreshes is only ever
    // reachable through its patch-set ref. The first version of this test cloned before the
    // amend, so jj still remembered the commit and the fallback was never exercised.
    let viewer = dir.path().join("viewer2");
    let out = Command::new("jj")
        .args(["git", "clone", "--colocate"])
        .arg(dir.path().join("canonical.git"))
        .arg(&viewer)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Test")
        .env("JJ_EMAIL", "test@example.com")
        .env("HOME", dir.path())
        .output()
        .expect("clone viewer");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new("git")
        .arg("-C")
        .arg(&viewer)
        .args(["fetch", "-q", "origin", "+refs/changes/*:refs/changes/*"])
        .output()
        .expect("fetch patch sets");
    assert!(out.status.success());

    let server = Server::start(&viewer);

    // The full id reaches it.
    let (status, _) = server.get_status(&format!("/api/revisions/{first}/diff"));
    assert_eq!(
        status, 200,
        "the superseded patch set {first} must be readable; listing it and refusing to serve \
         it makes pinning pointless"
    );

    // A PREFIX must not, because the fallback is an exact-id lookup by design — anything
    // looser could resolve to a commit the reader never asked for.
    let (status, _) = server.get_status(&format!("/api/revisions/{}/diff", &first[..12]));
    assert_eq!(status, 400, "a prefix must not reach an invisible commit");

    // And the change still lists both, so the two halves agree.
    let body = server
        .get(&format!("/api/changes/{change}"))
        .expect("change served");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(value["patch_sets"].as_array().expect("patch_sets").len(), 2);
}
