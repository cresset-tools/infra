//! The review store, exercised directly.
//!
//! The crate is a binary, so the module is pulled in by path — the same trick cresset-sync's
//! tests use.
#[path = "../src/review.rs"]
mod review;

use review::{NewThread, Review, Side};

fn thread(change: &str, body: &str) -> NewThread {
    NewThread {
        change_id: change.into(),
        path: "src/main.rs".into(),
        side: Side::Additions,
        line: 42,
        fingerprint: "    let limit = 100;".into(),
        context: r#"["a","b"]"#.into(),
        created_by: "jelle".into(),
        body: body.into(),
        patch_set_commit_id: "a".repeat(40),
    }
}

#[test]
fn a_thread_is_never_empty() {
    let mut db = Review::open_in_memory().expect("open");
    let created = db
        .start_thread(&thread("abc", "this looks wrong"))
        .expect("start");

    // Creating a thread creates its first comment, in one transaction. A thread with no
    // remark would render as an anchor with nothing to say.
    assert_eq!(created.comments.len(), 1);
    assert_eq!(created.comments[0].body, "this looks wrong");
    assert_eq!(created.comments[0].author, "jelle");
    assert!(!created.resolved);
}

#[test]
fn threads_are_listed_per_change_oldest_first() {
    let mut db = Review::open_in_memory().expect("open");
    db.start_thread(&thread("abc", "first")).expect("start");
    db.start_thread(&thread("abc", "second")).expect("start");
    db.start_thread(&thread("other", "elsewhere"))
        .expect("start");

    let threads = db.threads_for_change("abc").expect("list");
    assert_eq!(
        threads.len(),
        2,
        "a change must not see another change's threads"
    );
    assert_eq!(threads[0].comments[0].body, "first");
    assert_eq!(threads[1].comments[0].body, "second");
}

#[test]
fn replying_appends_and_records_which_version_it_was_written_against() {
    let mut db = Review::open_in_memory().expect("open");
    let created = db.start_thread(&thread("abc", "why?")).expect("start");
    let later = "b".repeat(40);

    let updated = db
        .reply(created.id, "because of X", "someone", &later)
        .expect("reply")
        .expect("thread exists");

    assert_eq!(updated.comments.len(), 2);
    assert_eq!(updated.comments[1].body, "because of X");
    // Which patch set a remark was written against is what lets a stale anchor say what the
    // reader was actually looking at.
    assert_eq!(updated.comments[1].patch_set_commit_id, later);
    assert_eq!(updated.comments[0].patch_set_commit_id, "a".repeat(40));
}

#[test]
fn replying_to_a_thread_that_does_not_exist_is_not_an_error() {
    let db = Review::open_in_memory().expect("open");
    let missing = db
        .reply(9999, "hello", "jelle", &"c".repeat(40))
        .expect("reply");
    assert!(
        missing.is_none(),
        "a missing thread is None, not a panic or a phantom row"
    );
}

#[test]
fn resolving_round_trips() {
    let mut db = Review::open_in_memory().expect("open");
    let created = db.start_thread(&thread("abc", "nit")).expect("start");
    let resolved = db
        .set_resolved(created.id, true)
        .expect("resolve")
        .expect("exists");
    assert!(resolved.resolved);
    let reopened = db
        .set_resolved(created.id, false)
        .expect("reopen")
        .expect("exists");
    assert!(!reopened.resolved, "resolving must be reversible");
}

#[test]
fn the_anchor_is_stored_untouched() {
    let mut db = Review::open_in_memory().expect("open");
    let created = db.start_thread(&thread("abc", "x")).expect("start");
    let read = db.thread(created.id).expect("read").expect("exists");

    // The server never interprets an anchor — the browser relocates with it. Storing it
    // verbatim is what keeps that division honest.
    assert_eq!(read.fingerprint, "    let limit = 100;");
    assert_eq!(read.context, r#"["a","b"]"#);
    assert_eq!(read.line, 42);
    assert_eq!(read.side, Side::Additions);
}

#[test]
fn migrate_is_idempotent_and_survives_reopening() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("review.db");
    {
        let mut db = Review::open(&path).expect("open");
        db.migrate().expect("again");
        db.migrate().expect("and again");
        db.start_thread(&thread("abc", "persisted")).expect("start");
    }
    let db = Review::open(&path).expect("reopen");
    let threads = db.threads_for_change("abc").expect("list");
    assert_eq!(threads.len(), 1, "data must survive a reopen");
    assert_eq!(threads[0].comments[0].body, "persisted");
}
