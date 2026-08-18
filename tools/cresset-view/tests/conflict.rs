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
        Self::start_with(repository, &[])
    }

    /// Start with extra flags, e.g. `--review-db` for the write endpoints.
    fn start_with(repository: &Path, extra: &[&str]) -> Self {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_cresset-view"))
            .arg("--repository")
            .arg(repository)
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            // The API needs no assets; ServeDir simply 404s for a directory that is not there.
            .arg("--assets")
            .arg(repository.join("no-assets"))
            .args(extra)
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

    /// A POST with a JSON body. `extra` carries headers a test needs to set, such as
    /// `Sec-Fetch-Site` for the cross-site refusal.
    fn post(&self, path: &str, body: &str, extra: &[(&str, &str)]) -> (u16, String) {
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             x-authentik-username: test\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n",
            body.len()
        );
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let raw = (|| {
            let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
            stream.write_all(head.as_bytes()).ok()?;
            stream.write_all(body.as_bytes()).ok()?;
            let mut response = String::new();
            stream.read_to_string(&mut response).ok()?;
            Some(response)
        })()
        .unwrap_or_default();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (status, body.to_string())
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
    let stacks = value["stacks"].as_array().expect("stacks array");

    assert_eq!(
        stacks.len(),
        1,
        "one review bookmark is open; `main` must not be listed: {value}"
    );
    assert_eq!(
        stacks[0]["bookmark"], "review/thing",
        "the review bookmark is REMOTE in a fetching repo, which is all the viewer ever has"
    );
    assert_eq!(
        stacks[0]["tip"], commit,
        "the tip is what merging would move main to"
    );
    let changes = stacks[0]["changes"].as_array().expect("changes array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["change_id"], change);
    assert_eq!(changes[0]["commit_id"], commit);
    assert_eq!(changes[0]["patch_sets"], 1);
}

/// A stack is grouped under its own bookmark, oldest first, and two stacks do not mix.
///
/// The flat queue could not do this: a commit partway up a stack carries no bookmark of its
/// own, and the fallback picked an arbitrary one — so with two review bookmarks open, a change
/// could be listed under the wrong one, and merging the wrong bookmark would land it anyway.
#[test]
fn a_stack_is_grouped_under_its_bookmark_in_landing_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (first_change, _) = review_repo(dir.path());
    let work = dir.path().join("work");

    // A second commit on the same bookmark: a relation chain.
    jj(&work, &["new", "review/thing", "-m", "and another"]);
    std::fs::write(work.join("h.txt"), "two\n").expect("write");
    jj(&work, &["bookmark", "set", "review/thing", "-r", "@"]);
    jj(&work, &["git", "push", "-b", "review/thing"]);
    let second_change = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "change_id"]);
    let tip = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);

    // And an unrelated bookmark off main, to prove the two do not bleed into each other.
    jj(&work, &["new", "main", "-m", "elsewhere"]);
    std::fs::write(work.join("i.txt"), "other\n").expect("write");
    jj(&work, &["bookmark", "set", "review/other", "-r", "@"]);
    jj(&work, &["git", "push", "-b", "review/other"]);
    let other_change = jj(&work, &["log", "-r", "@", "--no-graph", "-T", "change_id"]);

    let viewer = dir.path().join("viewer");
    let out = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&viewer)
        .env("JJ_CONFIG", "/dev/null")
        .env("HOME", dir.path())
        .output()
        .expect("fetch");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let server = Server::start(&viewer);
    let body = server.get("/api/changes").expect("queue");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let stacks = value["stacks"].as_array().expect("stacks");
    assert_eq!(stacks.len(), 2, "two bookmarks, two stacks: {value}");

    let thing = stacks
        .iter()
        .find(|s| s["bookmark"] == "review/thing")
        .expect("review/thing");
    let changes = thing["changes"].as_array().expect("changes");
    assert_eq!(
        changes
            .iter()
            .map(|c| c["change_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![first_change.as_str(), second_change.as_str()],
        "oldest first: the order they would land in"
    );
    assert_eq!(thing["tip"], tip, "merging lands the newest commit");

    let other = stacks
        .iter()
        .find(|s| s["bookmark"] == "review/other")
        .expect("review/other");
    let others = other["changes"].as_array().expect("changes");
    assert_eq!(others.len(), 1, "the unrelated stack carries only its own");
    assert_eq!(others[0]["change_id"], other_change);
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

/// Writing a comment: identity comes from the proxy header, and a cross-site write is refused.
#[test]
fn a_thread_records_its_author_and_refuses_cross_site_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, commit) = review_repo(dir.path());

    let viewer = dir.path().join("viewer");
    let review_db = dir.path().join("review.db");
    let server = Server::start_with(&viewer, &["--review-db", review_db.to_str().unwrap()]);

    let body = format!(
        r#"{{"path":"g.txt","side":"additions","line":1,"fingerprint":"one",
            "context":"[]","body":"why one?","patch_set_commit_id":"{commit}"}}"#
    );
    let (status, created) = server.post(&format!("/api/changes/{change}/threads"), &body, &[]);
    assert_eq!(status, 200, "creating a thread must succeed: {created}");
    let created: serde_json::Value = serde_json::from_str(&created).expect("valid json");

    // The author is whoever the proxy said, not anything the client sent — a client-supplied
    // author would let one reviewer sign another's name.
    assert_eq!(created["created_by"], "test");
    assert_eq!(created["comments"].as_array().expect("comments").len(), 1);
    assert_eq!(created["comments"][0]["body"], "why one?");

    // Authentication is a proxy header, so a tab on a hostile page is an authenticated actor.
    // An explicit cross-site label must be refused.
    let (status, refused) = server.post(
        "/api/threads/1/resolve",
        r#"{"resolved":true}"#,
        &[("sec-fetch-site", "cross-site")],
    );
    assert_eq!(status, 400, "a cross-site write must be refused");
    assert!(
        refused.contains("same-origin"),
        "the refusal must say why: {refused}"
    );

    // The same write from our own origin is fine.
    let (status, _) = server.post(
        "/api/threads/1/resolve",
        r#"{"resolved":true}"#,
        &[("sec-fetch-site", "same-origin")],
    );
    assert_eq!(status, 200, "a same-origin write must be allowed");

    // And the thread is listed against its change.
    let listed = server
        .get(&format!("/api/changes/{change}/threads"))
        .expect("threads listed");
    let listed: serde_json::Value = serde_json::from_str(&listed).expect("valid json");
    assert_eq!(listed.as_array().expect("array").len(), 1);
    assert_eq!(listed[0]["resolved"], true);
}

/// The anchor makes the round trip untouched, and a reply lands on the thread it answers.
///
/// The browser relocates comments using `fingerprint` and `context` (see web/src/threads.ts), so
/// the server storing them verbatim is load-bearing: anything that normalised, re-encoded, or
/// parsed-and-reserialised the context would move comments onto wrong lines, which is exactly
/// the failure the anchoring work exists to prevent.
#[test]
fn an_anchor_survives_the_round_trip_and_replies_append() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, commit) = review_repo(dir.path());

    let viewer = dir.path().join("viewer");
    let review_db = dir.path().join("review.db");
    let server = Server::start_with(&viewer, &["--review-db", review_db.to_str().unwrap()]);

    // Deliberately awkward: leading whitespace that matters, a quote, a backslash, and a tab.
    // Indentation is part of a line's identity, so a store that trimmed would relocate a comment
    // into a different scope.
    let context = r#"["  fn a() {","\tlet x = \"y\";","","  }"]"#;
    let fingerprint = "    if lane == -1 { lane = claim(); }";
    let body = format!(
        r#"{{"path":"g.txt","side":"deletions","line":42,
            "fingerprint":{fingerprint:?},"context":{context:?},
            "body":"why claim here?","patch_set_commit_id":"{commit}"}}"#
    );
    let (status, created) = server.post(&format!("/api/changes/{change}/threads"), &body, &[]);
    assert_eq!(status, 200, "creating a thread must succeed: {created}");
    let created: serde_json::Value = serde_json::from_str(&created).expect("valid json");
    let thread_id = created["id"].as_i64().expect("an id");

    assert_eq!(created["fingerprint"], fingerprint, "the line, verbatim");
    assert_eq!(created["context"], context, "the context, verbatim");
    assert_eq!(created["line"], 42);
    assert_eq!(created["side"], "deletions");
    assert_eq!(created["resolved"], false, "a new thread is open");

    let (status, replied) = server.post(
        &format!("/api/threads/{thread_id}/comments"),
        &format!(r#"{{"body":"because the lane is free","patch_set_commit_id":"{commit}"}}"#),
        &[],
    );
    assert_eq!(status, 200, "replying must succeed: {replied}");
    let replied: serde_json::Value = serde_json::from_str(&replied).expect("valid json");
    let comments = replied["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 2, "the reply appends rather than replacing");
    assert_eq!(comments[0]["body"], "why claim here?");
    assert_eq!(comments[1]["body"], "because the lane is free");

    // An empty comment is refused: a thread with nothing in it is an anchor nobody can answer.
    let (status, refused) = server.post(
        &format!("/api/threads/{thread_id}/comments"),
        &format!(r#"{{"body":"   ","patch_set_commit_id":"{commit}"}}"#),
        &[],
    );
    assert_eq!(status, 400, "an empty reply must be refused: {refused}");

    // Threads are scoped to their change. The queue shows several at once, so a leak here would
    // hang one change's comments off another's diff.
    let other = server
        .get("/api/changes/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz/threads")
        .expect("threads listed");
    let other: serde_json::Value = serde_json::from_str(&other).expect("valid json");
    assert_eq!(other.as_array().expect("array").len(), 0);
}

/// Approving publishes the file the push gate reads, and a new patch set is not approved by it.
///
/// This is the contract between the viewer and `hooks/update`. If the file stops being written,
/// or is written with a short commit id, or an approval of patch set 1 is allowed to satisfy the
/// gate for patch set 2, then either every push is refused or unreviewed code lands — and both
/// failures show up at the moment someone is trying to ship something.
#[test]
fn approving_publishes_the_file_the_gate_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, commit) = review_repo(dir.path());

    let viewer = dir.path().join("viewer");
    let review_db = dir.path().join("review.db");
    let approvals = dir.path().join("approved");
    let server = Server::start_with(
        &viewer,
        &[
            "--review-db",
            review_db.to_str().unwrap(),
            "--approvals-file",
            approvals.to_str().unwrap(),
        ],
    );

    // Written before anyone approves anything: the gate fails closed on a missing file, so an
    // instance that has never had an approval must still produce an (empty) one.
    let initial = std::fs::read_to_string(&approvals).expect("the file exists at startup");
    assert!(
        !initial.contains(&commit),
        "nothing is approved yet: {initial}"
    );

    let (status, body) = server.post(
        &format!("/api/changes/{change}/approvals"),
        &format!(r#"{{"commit_id":"{commit}","approved":true}}"#),
        &[],
    );
    assert_eq!(status, 200, "approving must succeed: {body}");
    let body: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(body["approvals"][0]["approved_by"], "test");
    assert_eq!(
        body["gated"], true,
        "an instance with the file gates pushes"
    );

    let published = std::fs::read_to_string(&approvals).expect("approvals file");
    assert!(
        published
            .lines()
            .any(|line| line == format!("{change} {commit}")),
        "the hook greps for the exact pair on its own line: {published}"
    );

    // A short id would be recorded and then never match `git rev-list`'s full ids, so the gate
    // would refuse a push the reviewer believes they approved.
    let (status, refused) = server.post(
        &format!("/api/changes/{change}/approvals"),
        r#"{"commit_id":"abc1234","approved":true}"#,
        &[],
    );
    assert_eq!(status, 400, "a short commit id must be refused: {refused}");
    assert!(refused.contains("40-character"), "and say why: {refused}");

    // Withdrawing removes the line. An approval that could only be invalidated by pushing again
    // would make people hesitate to approve at all.
    let (status, _) = server.post(
        &format!("/api/changes/{change}/approvals"),
        &format!(r#"{{"commit_id":"{commit}","approved":false}}"#),
        &[],
    );
    assert_eq!(status, 200);
    let after = std::fs::read_to_string(&approvals).expect("approvals file");
    assert!(
        !after.contains(&commit),
        "withdrawing must remove the line, or the gate keeps letting it through: {after}"
    );

    // And a cross-site approval is refused for the same reason a cross-site comment is: proxy
    // header authentication means any tab in the reviewer's browser is an authenticated actor,
    // and this button is the one that lets code reach 31 public repositories.
    let (status, refused) = server.post(
        &format!("/api/changes/{change}/approvals"),
        &format!(r#"{{"commit_id":"{commit}","approved":true}}"#),
        &[("sec-fetch-site", "cross-site")],
    );
    assert_eq!(
        status, 400,
        "a cross-site approval must be refused: {refused}"
    );
}

/// An instance with no approvals file records who read what, and says it gates nothing.
#[test]
fn an_instance_without_the_gate_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, commit) = review_repo(dir.path());
    let review_db = dir.path().join("review.db");
    let server = Server::start_with(
        &dir.path().join("viewer"),
        &["--review-db", review_db.to_str().unwrap()],
    );

    let (status, body) = server.post(
        &format!("/api/changes/{change}/approvals"),
        &format!(r#"{{"commit_id":"{commit}","approved":true}}"#),
        &[],
    );
    assert_eq!(status, 200, "approving still works: {body}");
    let body: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(
        body["gated"], false,
        "it must not imply an enforcement that is not there"
    );
}

/// Merging goes through receive-pack, so the update hook decides — and its refusal is shown.
///
/// This is the property the Merge button rests on. cresset-view pushes rather than writing a
/// ref, which is what keeps ONE gate: if this ever became a direct ref write, the button would
/// become a way around the approvals it exists to serve. The hook here is a stand-in — the real
/// one is exercised by `nix flake check` — because what matters at this layer is that a hook
/// runs at all and that its message reaches the caller.
#[test]
fn merging_is_a_push_and_the_hook_still_decides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_change, commit) = review_repo(dir.path());
    let bare = dir.path().join("canonical.git");
    let allow = dir.path().join("allow");

    std::fs::write(
        bare.join("hooks/update"),
        format!(
            "#!/bin/sh\n\
             [ -e {} ] && exit 0\n\
             echo 'refusing: not every commit has been approved' >&2\n\
             exit 1\n",
            allow.display()
        ),
    )
    .expect("write hook");
    let mut mode = std::fs::metadata(bare.join("hooks/update"))
        .expect("stat hook")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(bare.join("hooks/update"), mode).expect("chmod hook");

    let viewer = dir.path().join("viewer");
    let review_db = dir.path().join("review.db");
    let server = Server::start_with(
        &viewer,
        &[
            "--review-db",
            review_db.to_str().unwrap(),
            // A local path stands in for the ssh remote: the transport is not what is under
            // test, the fact that a push happens is.
            "--merge-remote",
            bare.to_str().unwrap(),
            "--merge-ssh-key",
            "/dev/null",
        ],
    );

    let head_of_main = || {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&bare)
            .args(["rev-parse", "refs/heads/main"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let before = head_of_main();

    let body = format!(r#"{{"bookmark":"review/thing","tip":"{commit}"}}"#);
    let (status, refused) = server.post("/api/merge", &body, &[]);
    assert_eq!(
        status, 400,
        "an unapproved merge must be refused: {refused}"
    );
    assert!(
        refused.contains("not every commit has been approved"),
        "the hook's own message must reach the reviewer, since it is the actionable part: {refused}"
    );
    assert_eq!(head_of_main(), before, "a refused merge must not move main");

    // The same merge, once the hook is satisfied.
    std::fs::write(&allow, "").expect("allow");
    let (status, merged) = server.post("/api/merge", &body, &[]);
    assert_eq!(status, 200, "an approved merge must land: {merged}");
    assert_eq!(
        head_of_main(),
        commit,
        "main must be at the tip that was merged"
    );

    // A short id would push something other than what was on screen.
    let (status, _) = server.post(
        "/api/merge",
        r#"{"bookmark":"review/thing","tip":"abc1234"}"#,
        &[],
    );
    assert_eq!(status, 400, "a merge names its tip in full");

    // And a cross-site merge is refused, for the reason every write here is: proxy-header
    // authentication makes any tab in the reviewer's browser an authenticated actor, and this
    // is the button that publishes.
    let (status, refused) = server.post("/api/merge", &body, &[("sec-fetch-site", "cross-site")]);
    assert_eq!(status, 400, "a cross-site merge must be refused: {refused}");
    assert!(refused.contains("same-origin"), "and say why: {refused}");
}

/// A change whose base has moved is told to rebase, not to `git pull`.
///
/// This happens for real and is not an error state: an import lands on main while a change is
/// open for review, and the change is then behind. git's own advice for a non-fast-forward is
/// `git pull`, which in a jj repository is wrong twice over -- so the raw output is exactly the
/// wrong thing to show, and this is the one push failure that is paraphrased.
#[test]
fn a_change_left_behind_by_main_is_told_to_rebase() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_change, commit) = review_repo(dir.path());
    let bare = dir.path().join("canonical.git");
    let work = dir.path().join("work");

    // Move main on the canonical repo, so the change under review is now behind it -- exactly
    // what an automated import landing mid-review does.
    jj(
        &work,
        &["new", "main", "-m", "landed while the change was open"],
    );
    std::fs::write(work.join("landed.txt"), "meanwhile\n").expect("write");
    jj(&work, &["bookmark", "set", "main", "-r", "@"]);
    jj(&work, &["git", "push", "-b", "main"]);

    let viewer = dir.path().join("viewer");
    let review_db = dir.path().join("review.db");
    let server = Server::start_with(
        &viewer,
        &[
            "--review-db",
            review_db.to_str().unwrap(),
            "--merge-remote",
            bare.to_str().unwrap(),
            "--merge-ssh-key",
            "/dev/null",
        ],
    );

    let (status, message) = server.post(
        "/api/merge",
        &format!(r#"{{"bookmark":"review/thing","tip":"{commit}"}}"#),
        &[],
    );
    assert_eq!(status, 400, "a stale change cannot land: {message}");
    assert!(
        message.contains("main has moved"),
        "it must say what happened: {message}"
    );
    assert!(
        message.contains("jj rebase"),
        "and give the command that fixes it: {message}"
    );
    assert!(
        !message.contains("git pull"),
        "and must not repeat git's advice, which is wrong for this repository: {message}"
    );
    assert!(
        message.contains("approving again"),
        "and warn that the rebase invalidates the approval: {message}"
    );
}

/// An instance with no merge remote says so rather than failing obscurely.
#[test]
fn an_instance_that_cannot_merge_explains_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_change, commit) = review_repo(dir.path());
    let server = Server::start(&dir.path().join("viewer"));
    let (status, message) = server.post(
        "/api/merge",
        &format!(r#"{{"bookmark":"review/thing","tip":"{commit}"}}"#),
        &[],
    );
    assert_eq!(status, 400);
    assert!(
        message.contains("not available"),
        "it must say merging is unavailable here: {message}"
    );
}

/// Without a review database the instance stays read-only, and says so.
#[test]
fn writing_without_a_review_database_explains_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (change, commit) = review_repo(dir.path());
    let server = Server::start(&dir.path().join("viewer"));

    let body = format!(
        r#"{{"path":"g.txt","side":"additions","line":1,"fingerprint":"one",
            "context":"[]","body":"x","patch_set_commit_id":"{commit}"}}"#
    );
    let (status, message) = server.post(&format!("/api/changes/{change}/threads"), &body, &[]);
    assert_eq!(status, 400);
    assert!(
        message.contains("read-only"),
        "it must say the instance is read-only rather than failing obscurely: {message}"
    );
}
