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

use std::collections::HashSet;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cresset_review::{Command, USAGE};
use serde::{Deserialize, Serialize};

fn main() {
    match run() {
        Ok(code) => {
            // `exit` skips the flush that returning from main would do, and the one JSON object
            // `wait` prints is the whole point of running it.
            let _ = std::io::stdout().flush();
            std::process::exit(code);
        }
        Err(error) => {
            // `{:#}` so the context chain is shown: "resolving the current patch set: 404" says
            // where to look, and "404" alone does not.
            eprintln!("cresset-review: {error:#}");
            std::process::exit(1);
        }
    }
}

/// What `wait` exits with when it stops without an approval.
///
/// Distinct from 1 so that a failure to ask and an answer of "not yet" are not the same event:
/// `cresset-review wait <id> && jj git push -b main` must land only on approval, and a caller
/// that wants to know which of the two happened reads the JSON rather than guessing from 1.
const EXIT_UNAPPROVED: i32 = 3;

fn run() -> Result<i32> {
    // EVERY argument, joined — not argv[1].
    //
    // sshd runs a forced command through a shell, so `command="cresset-review-shell jelle via
    // agent"` reaches this binary as three separate arguments. Reading only the first recorded
    // comments as "jelle": the identity was pinned correctly and then truncated here, which is
    // worse than a missing feature because the attribution looked plausible.
    //
    // The integration test missed it by invoking the binary directly with one argument
    // containing spaces, which is the one thing the real path never does. It now passes the
    // words separately, the way sshd does.
    let identity = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let identity = Some(identity)
        .filter(|identity| !identity.trim().is_empty())
        .context("no identity: this binary is an ssh forced command and is not useful by hand")?;

    // Whatever the client sent. Split on whitespace and matched against a fixed set — never
    // evaluated, never passed to a shell.
    let original = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
    let words: Vec<&str> = original.split_whitespace().collect();

    let api =
        std::env::var("CRESSET_VIEW_URL").unwrap_or_else(|_| "http://127.0.0.1:9080".to_owned());
    let client = Client { api, identity };

    match Command::parse(&words)? {
        Command::Wait { change, seconds } => return wait(&client, &change, seconds),
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

            // Say which patch set this is being recorded against, on stderr so it cannot
            // corrupt the JSON on stdout.
            //
            // The viewer serves a SNAPSHOT of the repository, refreshed when a push wakes it.
            // Replying within a second or two of pushing a new patch set therefore records the
            // previous one — observed in practice, two replies a second apart landing on
            // different patch sets. There is nothing to wait for here that would be honest
            // (the server cannot know a push is in flight), so the answer is to say what was
            // recorded and let a caller who just pushed notice.
            eprintln!(
                "recorded against patch set {}",
                &patch_set[..12.min(patch_set.len())]
            );

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
    Ok(0)
}

/// How often the cheap half of the wait loop runs: two SQLite-backed reads over loopback.
///
/// Overridable so the integration tests drive the real loop rather than a copy of it with the
/// timings edited out — the recurring lesson in this repository is that the gap between what is
/// tested and what runs is where the bugs live. Not reachable from a client: sshd passes
/// `SSH_ORIGINAL_COMMAND` and nothing else into a forced command.
fn poll_interval() -> Duration {
    match std::env::var("CRESSET_REVIEW_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        Some(ms) if ms > 0 => Duration::from_millis(ms),
        _ => Duration::from_secs(10),
    }
}

/// Watch a change until there is something to do about it.
///
/// ## Why the loop is here and not in the client
///
/// One ssh connection held open for the whole wait, instead of a key exchange every ten seconds
/// for a day. The client `exec`s ssh, so this process's exit status is the command's, and when
/// the client goes away sshd closes the channel and this is signalled — a wait does not outlive
/// the caller that asked for it.
///
/// ## What ends it
///
/// - **approved** — an approval whose patch id is the one the current patch set would land.
///   Matched exactly the way `hooks/update` matches it, so `approved` here means "the gate will
///   let this through" rather than "somebody pressed a button at some point".
/// - **commented** — a comment appeared that was not there when the wait started and was not
///   written by this identity. A reviewer who asks a question is not going to approve until it
///   is answered, so going on waiting for approval would be waiting for the wrong thing. This
///   is the half of the feature that makes it a watcher rather than a sleep.
/// - **gone** — the change is no longer in the review queue: it landed, or its bookmark was
///   deleted.
/// - **pending** — the deadline passed with none of the above.
///
/// ## The first reading is a baseline, not an answer
///
/// The viewer serves a snapshot of the repository, refreshed when a push wakes it, so a wait
/// started seconds after pushing can still see the PREVIOUS patch set as current — and an
/// approval of that one would look like an approval of this one. The queue is therefore read
/// again one poll later, and an approval only counts once two readings agree on what the
/// current patch set is. `wait <id> 0` cannot do that and says so instead.
fn wait(client: &Client, change: &str, seconds: u64) -> Result<i32> {
    let poll = poll_interval();
    // The queue read loads the whole repository through jj, while approvals and comments are two
    // small SQLite queries. So they run at different rates: the signals a reviewer produces are
    // checked every poll, and the one thing only a push can change is re-read once a minute.
    let reload = poll * 6;
    let heartbeat = poll * 90;
    let started = Instant::now();
    let deadline = Duration::from_secs(seconds);

    let Some(mut open) = find_open(client, change)? else {
        // Nothing to read approvals of, so the empty answer is also the honest one.
        let none = Approvals {
            rows: Vec::new(),
            gated: false,
        };
        return finish(Ended::Gone, client, change, started, None, &none, &[]);
    };
    let mut approvals = read_approvals(client, change)?;
    // Comments that already existed are not news. An agent that has just pushed a revision and
    // answered every thread would otherwise be woken immediately by the remarks it had just
    // dealt with, which is the one behaviour that would make this verb useless.
    let mut known: HashSet<i64> = comment_ids(&read_threads(client, change)?);

    // Nothing can ever match, so waiting would be waiting for something that cannot happen.
    // Said now rather than in 24 hours' time.
    if open.patch_id.is_none() {
        bail!(
            "the viewer cannot say what this change's current patch set does, so nothing can \
             approve it: either it is a merge commit, which the gate refuses by name, or the \
             instance could not find the git store behind the repository when it started"
        );
    }

    if deadline.is_zero() {
        // A single read, so there is nothing to confirm it against. Reported as what it is.
        eprintln!(
            "read once; a patch set pushed in the last few seconds may not be in this snapshot yet"
        );
        let ended = if matching_approvals(&approvals.rows, open.patch_id.as_deref()).is_empty() {
            Ended::Pending
        } else {
            Ended::Approved
        };
        return finish(ended, client, change, started, Some(&open), &approvals, &[]);
    }

    eprintln!(
        "waiting for approval of {change} patch set {} on {} ({}s poll, giving up after {seconds}s)",
        open.patch_sets,
        open.bookmark,
        poll.as_secs_f32(),
    );

    let mut queue_read = Instant::now();
    // Whether the current patch set has been seen the same way twice; see the doc comment.
    let mut settled = false;
    let mut beat = Instant::now();

    loop {
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            return finish(
                Ended::Pending,
                client,
                change,
                started,
                Some(&open),
                &approvals,
                &[],
            );
        }
        std::thread::sleep(poll.min(left));

        if !settled || queue_read.elapsed() >= reload {
            let Some(fresh) = find_open(client, change)? else {
                return finish(Ended::Gone, client, change, started, None, &approvals, &[]);
            };
            settled = fresh.is_same_version_as(&open);
            if !settled {
                eprintln!(
                    "patch set {} is current now ({}); approvals of the previous one no longer count",
                    fresh.patch_sets,
                    short(&fresh.commit_id)
                );
            }
            open = fresh;
            queue_read = Instant::now();
        }

        approvals = read_approvals(client, change)?;
        if settled && !matching_approvals(&approvals.rows, open.patch_id.as_deref()).is_empty() {
            return finish(
                Ended::Approved,
                client,
                change,
                started,
                Some(&open),
                &approvals,
                &[],
            );
        }

        let threads = read_threads(client, change)?;
        let fresh: Vec<NewComment> = threads
            .iter()
            .flat_map(|thread| {
                thread.comments.iter().map(move |comment| NewComment {
                    thread: thread.id,
                    id: comment.id,
                    path: &thread.path,
                    line: thread.line,
                    resolved: thread.resolved,
                    author: &comment.author,
                    body: &comment.body,
                })
            })
            // Our own replies are not somebody asking us something. The identity is the one
            // `authorized_keys` pinned, so this compares against what the server recorded and
            // not against anything the client claimed.
            .filter(|comment| !known.contains(&comment.id) && comment.author != client.identity)
            .collect();
        if !fresh.is_empty() {
            return finish(
                Ended::Commented,
                client,
                change,
                started,
                Some(&open),
                &approvals,
                &fresh,
            );
        }
        // Comments we wrote ourselves are folded into the baseline too, so a later poll does not
        // have to filter them again.
        known.extend(comment_ids(&threads));

        if beat.elapsed() >= heartbeat {
            eprintln!(
                "still waiting: {}s elapsed, patch set {}, {} approval(s) on record{}",
                started.elapsed().as_secs(),
                open.patch_sets,
                approvals.rows.len(),
                if approvals.gated {
                    ""
                } else {
                    ", and this instance gates nothing"
                }
            );
            beat = Instant::now();
        }
    }
}

/// Why a wait stopped. Exactly one of these is printed, as `outcome`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    Approved,
    Commented,
    Gone,
    Pending,
}

impl Ended {
    fn name(self) -> &'static str {
        match self {
            Ended::Approved => "approved",
            Ended::Commented => "commented",
            Ended::Gone => "gone",
            Ended::Pending => "pending",
        }
    }

    /// Prose for a reader, next to the JSON for a program.
    fn note(self, gated: bool) -> &'static str {
        match self {
            // What `gated` is for: on an instance with no approvals file the approval is still
            // recorded and still means a person read this, but claiming the gate will accept it
            // would be describing a gate that is not there.
            Ended::Approved if gated => {
                "the diff this patch set lands is approved, so the push gate will accept it"
            }
            Ended::Approved => {
                "the diff this patch set lands is approved, but this instance enforces nothing"
            }
            Ended::Commented => {
                "a reviewer said something that was not there when the wait started; answer it \
                 with `cresset-review reply`, push a revision, and wait again"
            }
            Ended::Gone => {
                "no longer open for review: it landed, or its review bookmark was deleted"
            }
            Ended::Pending => "nothing happened before the deadline; run it again to keep waiting",
        }
    }
}

/// One JSON object on stdout, one line of prose on stderr, and the exit status.
fn finish(
    ended: Ended,
    client: &Client,
    change: &str,
    started: Instant,
    open: Option<&Open>,
    approvals: &Approvals,
    comments: &[NewComment],
) -> Result<i32> {
    let current = open.and_then(|open| open.patch_id.as_deref());
    let (matching, other): (Vec<&Approval>, Vec<&Approval>) = approvals
        .rows
        .iter()
        .filter(|approval| !approval.patch_id.is_empty())
        .partition(|approval| Some(approval.patch_id.as_str()) == current);

    println!(
        "{}",
        serde_json::to_string(&WaitResult {
            outcome: ended.name(),
            change_id: change,
            waited_seconds: started.elapsed().as_secs(),
            identity: &client.identity,
            // Worth saying rather than implying a gate that is not there: without an approvals
            // file the service records who read what and nothing enforces it.
            gated: approvals.gated,
            open,
            approvals: matching,
            // Deliberately reported rather than dropped: "approved at patch set 1, and you have
            // pushed 2" is the single most useful thing to say to someone whose wait has not
            // ended, and the gate's answer to it is no.
            approvals_of_other_versions: other,
            comments,
            note: ended.note(approvals.gated),
        })
        .context("rendering the result")?
    );
    eprintln!("{}: {}", ended.name(), ended.note(approvals.gated));
    Ok(if ended == Ended::Approved {
        0
    } else {
        EXIT_UNAPPROVED
    })
}

/// Approvals that answer for the diff a patch set lands.
///
/// The rule is `hooks/update`'s, which greps the projected file for `<change id> <patch id>`:
/// the change id is this endpoint, and an empty patch id is a row written before approvals were
/// keyed this way and is ignored, so one fails closed rather than matching everything.
fn matching_approvals<'a>(approvals: &'a [Approval], patch_id: Option<&str>) -> Vec<&'a Approval> {
    let Some(patch_id) = patch_id else {
        return Vec::new();
    };
    approvals
        .iter()
        .filter(|approval| !approval.patch_id.is_empty() && approval.patch_id == patch_id)
        .collect()
}

fn comment_ids(threads: &[Thread]) -> HashSet<i64> {
    threads
        .iter()
        .flat_map(|thread| thread.comments.iter().map(|comment| comment.id))
        .collect()
}

fn short(commit_id: &str) -> &str {
    &commit_id[..12.min(commit_id.len())]
}

/// The change as the review queue has it, or `None` if the queue does not have it at all.
fn find_open(client: &Client, change: &str) -> Result<Option<Open>> {
    let queue: Queue =
        serde_json::from_str(&client.get("/api/changes")?).context("reading the review queue")?;
    for stack in queue.stacks {
        for summary in stack.changes {
            if summary.change_id == change {
                return Ok(Some(Open {
                    bookmark: stack.bookmark,
                    behind_main: stack.behind_main,
                    commit_id: summary.commit_id,
                    patch_id: summary.patch_id,
                    patch_sets: summary.patch_sets,
                    has_conflict: summary.has_conflict,
                }));
            }
        }
    }
    Ok(None)
}

fn read_approvals(client: &Client, change: &str) -> Result<Approvals> {
    serde_json::from_str(&client.get(&format!("/api/changes/{change}/approvals"))?)
        .context("reading the approvals")
}

fn read_threads(client: &Client, change: &str) -> Result<Vec<Thread>> {
    serde_json::from_str(&client.get(&format!("/api/changes/{change}/threads"))?)
        .context("reading the comment threads")
}

/// What the review queue says about one change, flattened from the stack that carries it.
#[derive(Serialize)]
struct Open {
    /// The review bookmark carrying it, which is what gets landed.
    bookmark: String,
    /// Whether main has moved since it was pushed. Approval does not make a push fast-forward,
    /// so a caller that is about to land needs this as much as the approval.
    behind_main: bool,
    commit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_id: Option<String>,
    patch_sets: u32,
    has_conflict: bool,
}

impl Open {
    /// Whether two readings of the queue agree on what would land. Not an equality: the stack
    /// moving behind main changes nothing about the diff anybody read.
    fn is_same_version_as(&self, other: &Open) -> bool {
        self.commit_id == other.commit_id
            && self.patch_id == other.patch_id
            && self.patch_sets == other.patch_sets
    }
}

#[derive(Serialize)]
struct WaitResult<'a> {
    outcome: &'static str,
    change_id: &'a str,
    waited_seconds: u64,
    /// Whose wait this was, under the name the key is pinned to — the same name that decides
    /// which comments count as somebody else's.
    identity: &'a str,
    gated: bool,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    open: Option<&'a Open>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    approvals: Vec<&'a Approval>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    approvals_of_other_versions: Vec<&'a Approval>,
    #[serde(skip_serializing_if = "<[NewComment]>::is_empty")]
    comments: &'a [NewComment<'a>],
    note: &'static str,
}

#[derive(Serialize)]
struct NewComment<'a> {
    thread: i64,
    id: i64,
    path: &'a str,
    line: i64,
    resolved: bool,
    author: &'a str,
    body: &'a str,
}

#[derive(Deserialize)]
struct Queue {
    stacks: Vec<QueueStack>,
}

#[derive(Deserialize)]
struct QueueStack {
    bookmark: String,
    behind_main: bool,
    changes: Vec<QueueChange>,
}

#[derive(Deserialize)]
struct QueueChange {
    change_id: String,
    commit_id: String,
    /// Absent when the service could not work out what the commit changes, which is the one
    /// state in which nothing can ever be approved.
    #[serde(default)]
    patch_id: Option<String>,
    patch_sets: u32,
    has_conflict: bool,
}

/// `/api/changes/<id>/approvals`, as the service returns it.
#[derive(Deserialize)]
struct Approvals {
    #[serde(rename = "approvals")]
    rows: Vec<Approval>,
    /// False when this instance records approvals but nothing enforces them.
    gated: bool,
}

#[derive(Deserialize, Serialize)]
struct Approval {
    commit_id: String,
    patch_id: String,
    approved_by: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct Thread {
    id: i64,
    path: String,
    line: i64,
    resolved: bool,
    comments: Vec<ThreadComment>,
}

#[derive(Deserialize)]
struct ThreadComment {
    id: i64,
    author: String,
    body: String,
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
