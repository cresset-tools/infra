//! The open queue, against a real jj repository.
//!
//! What this pins is the one thing `::tip - ::main` cannot express. Review is anchored to the
//! jj change id everywhere it matters — patch sets are recorded per change, approvals are keyed
//! to `git patch-id` so that a clean rebase preserves them, and the push gate matches on the
//! pair — but the queue decided what was still open by subtracting *commit* ids. A change
//! rebased onto a newer main before it landed therefore landed as a different commit, the
//! review bookmark kept pointing at the pre-rebase one, and the stack never emptied.
//!
//! The fixture is three repositories because two cannot show the defect. The viewer's copy only
//! ever fetches, so it holds `review/x@origin` at whatever was last pushed to that ref and
//! `main` at what landed — two refs that moved independently. Rebase inside one repository and
//! jj moves the bookmark with the commit, which is precisely the state production is NOT in.

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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write file");
}

struct Fixture {
    /// The viewer's repository: fetches only, as in production.
    viewer: std::path::PathBuf,
    /// The change id that was rebased and landed.
    landed_change: String,
    /// The commit the review bookmark still points at on the remote.
    stale_commit: String,
    /// The commit that actually landed on main.
    landed_commit: String,
    /// A change on another bookmark that has NOT landed, so the test can tell "the queue is
    /// correct" apart from "the queue is empty".
    open_change: String,
}

/// Author pushes a review bookmark, rebases it onto a newer main, and pushes only `main`.
fn rebased_then_landed(root: &Path) -> Fixture {
    let upstream = root.join("upstream.git");
    let author = root.join("author");
    let viewer = root.join("viewer");

    git(root, &["init", "-q", "--bare", upstream.to_str().unwrap()]);
    // The hook that would gate main is not installed: this fixture is about what the viewer
    // reports, and the gate has its own tests.
    std::fs::create_dir_all(&author).expect("mkdir author");
    jj(&author, &["git", "init", "--colocate", "."]);
    jj(
        &author,
        &["git", "remote", "add", "origin", upstream.to_str().unwrap()],
    );

    // main: one commit, pushed.
    write(&author, "base.txt", "base\n");
    jj(&author, &["describe", "-m", "base"]);
    jj(&author, &["bookmark", "create", "main", "-r", "@"]);
    jj(&author, &["git", "push", "-b", "main"]);

    // The review change, on top of that main, pushed to its own bookmark.
    jj(&author, &["new", "main", "-m", "the change under review"]);
    write(&author, "feature.txt", "feature\n");
    let landed_change = jj(
        &author,
        &["log", "-r", "@", "--no-graph", "-T", "change_id"],
    );
    let stale_commit = jj(
        &author,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    );
    jj(
        &author,
        &["bookmark", "create", "review/rebased", "-r", "@"],
    );
    jj(&author, &["git", "push", "-b", "review/rebased"]);

    // A second review change that never lands, as a control.
    jj(&author, &["new", "main", "-m", "still open"]);
    write(&author, "other.txt", "other\n");
    let open_change = jj(
        &author,
        &["log", "-r", "@", "--no-graph", "-T", "change_id"],
    );
    jj(&author, &["bookmark", "create", "review/open", "-r", "@"]);
    jj(&author, &["git", "push", "-b", "review/open"]);

    // main moves on underneath the review change.
    jj(&author, &["new", "main", "-m", "someone else landed first"]);
    write(&author, "interloper.txt", "interloper\n");
    jj(&author, &["bookmark", "set", "main", "-r", "@"]);
    jj(&author, &["git", "push", "-b", "main"]);

    // The rebase, then the land. jj rewrites the commit and keeps the change id; the bookmark
    // follows it here, but nothing re-pushes `review/rebased`, so upstream keeps the old one.
    jj(&author, &["rebase", "-r", &landed_change, "-d", "main"]);
    let landed_commit = jj(
        &author,
        &["log", "-r", &landed_change, "--no-graph", "-T", "commit_id"],
    );
    assert_ne!(
        stale_commit, landed_commit,
        "the rebase must have produced a new commit id, or this test proves nothing"
    );
    jj(&author, &["bookmark", "set", "main", "-r", &landed_change]);
    jj(&author, &["git", "push", "-b", "main"]);

    // The viewer: a fetching-only clone, which is what gives it two refs that moved apart.
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            viewer.join(".git").to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(&viewer).expect("mkdir viewer");
    git(&viewer, &["config", "core.bare", "false"]);
    jj(&viewer, &["git", "init", "--colocate", "."]);
    jj(&viewer, &["git", "fetch"]);

    // The fixture is only interesting if the viewer really holds the stale ref.
    let refs = jj(
        &viewer,
        &[
            "bookmark",
            "list",
            "--all-remotes",
            "-T",
            r#"name ++ " " ++ if(remote, "@" ++ remote, "local") ++ " " ++ normal_target.commit_id() ++ "\n""#,
        ],
    );
    assert!(
        refs.contains(&stale_commit),
        "the viewer should still hold the pre-rebase commit {stale_commit}, got:\n{refs}"
    );

    Fixture {
        viewer,
        landed_change,
        stale_commit,
        landed_commit,
        open_change,
    }
}

#[test]
fn a_change_rebased_before_landing_leaves_the_open_queue() {
    let root = tempdir();
    let fx = rebased_then_landed(&root.0);
    let server = Server::start(&fx.viewer);
    let body = server.get("/api/changes").expect("GET /api/changes");

    // The landed change must be gone, by change id and by either commit id. Checking the
    // change id alone would pass against a response that still lists the stale commit under a
    // truncated id.
    assert!(
        !body.contains(&fx.landed_change),
        "the rebased-and-landed change {} is still in the queue:\n{body}",
        fx.landed_change
    );
    assert!(
        !body.contains(&fx.stale_commit),
        "the pre-rebase commit {} is still in the queue:\n{body}",
        fx.stale_commit
    );
    assert!(
        !body.contains(&fx.landed_commit),
        "the landed commit {} is still in the queue:\n{body}",
        fx.landed_commit
    );
    // Its bookmark goes with it: an empty stack is not reported at all.
    assert!(
        !body.contains("review/rebased"),
        "the emptied stack is still reported:\n{body}"
    );

    // And the queue still works. Without this the test passes just as well against a handler
    // that returns nothing at all.
    assert!(
        body.contains(&fx.open_change),
        "the genuinely open change {} is missing from the queue:\n{body}",
        fx.open_change
    );
    assert!(
        body.contains("review/open"),
        "the genuinely open stack is missing from the queue:\n{body}"
    );
}

/// A scratch directory removed on drop, including on panic.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    let dir = std::env::temp_dir().join(format!(
        "cresset-view-queue-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir temp");
    TempDir(dir)
}

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

    fn get(&self, path: &str) -> Option<String> {
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
        let (head, body) = response.split_once("\r\n\r\n")?;
        head.starts_with("HTTP/1.1 200").then(|| body.to_string())
    }
}
