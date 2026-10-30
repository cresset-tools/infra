//! The review store: comment threads on changes.
//!
//! Separate from cresset-sync's checkpoint database on purpose. One is authoritative
//! synchronization state that the fleet depends on; the other is discussion. They should not
//! share a failure domain, a backup policy, or a lock.
//!
//! Conventions follow `operations/sync/src/db.rs` deliberately, so someone who has read one
//! recognises the other: a `SCHEMA` of `CREATE TABLE IF NOT EXISTS`, a `migrate()` that is
//! idempotent and runs on every open, `NewX`/`XRow` pairs, and row mappers returning a nested
//! Result so a bad enum value is an error rather than a panic.
//!
//! Anchors are stored but never interpreted here. Relocating a comment onto a later patch set
//! happens in the browser (`web/src/anchor.ts`), against content it already has for rendering
//! — so this table holds the fingerprint and context as opaque text and the server never has
//! to fetch a file to decide where a comment goes.
#![allow(dead_code)]

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

const SCHEMA: &str = r#"
-- A comment thread, anchored to a line of a change.
--
-- Anchored to the CHANGE id, not a commit id: the change id survives amends, which is the
-- whole reason review here can anchor to it at all. `patch_set_commit_id` on the first comment
-- records which version the reader was looking at, so a stale anchor can say what it was
-- written against.
--
-- `fingerprint` and `context` are the anchor. They are opaque to this crate; the browser
-- relocates with them and decides whether the placement is exact, moved, or stale.
CREATE TABLE IF NOT EXISTS thread (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    change_id TEXT NOT NULL,
    path TEXT NOT NULL,
    side TEXT NOT NULL,
    line INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    context TEXT NOT NULL,
    resolved INTEGER NOT NULL DEFAULT 0,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS thread_change_idx ON thread (change_id);

-- One remark in a thread. Threads are never empty: creating one creates its first comment.
CREATE TABLE IF NOT EXISTS comment (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id INTEGER NOT NULL REFERENCES thread (id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    author TEXT NOT NULL,
    patch_set_commit_id TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS comment_thread_idx ON comment (thread_id, id);

-- An approval of one patch set by one person.
--
-- Keyed to the change id and the PATCH ID of what was read — not to the commit id.
--
-- An approval says "I read this". Keyed to a commit id that was true but brittle: a rebase
-- gives every commit a new id while changing nothing anybody read, so main moving underneath a
-- change silently invalidated every approval on it. A gate that cannot tell a rebase from an
-- edit is not a strict gate; it is one that makes people re-read text they have already read,
-- which is how approval becomes a reflex.
--
-- The change id alone would be worse than either: it survives amends, which is the whole point
-- of patch sets, so on its own it would let unreviewed edits land. The pair is what carries the
-- distinction — see src/patch_id.rs for how the diff is reduced to one id, and why the push
-- gate and this service must compute it with the same binary rather than two implementations.
--
-- commit_id stays, as the record of which patch set was actually in front of the reader.
CREATE TABLE IF NOT EXISTS approval (
    change_id TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    patch_id TEXT NOT NULL DEFAULT '',
    approved_by TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (change_id, commit_id, approved_by)
);
CREATE INDEX IF NOT EXISTS approval_change_idx ON approval (change_id);
"#;

/// Which side of a diff a thread hangs off, matching `@pierre/diffs`' `AnnotationSide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Deletions,
    Additions,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Deletions => "deletions",
            Side::Additions => "additions",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "deletions" => Ok(Side::Deletions),
            "additions" => Ok(Side::Additions),
            other => Err(anyhow!("thread.side holds an unrecognised value {other:?}")),
        }
    }
}

pub struct NewThread {
    pub change_id: String,
    pub path: String,
    pub side: Side,
    pub line: i64,
    pub fingerprint: String,
    pub context: String,
    pub created_by: String,
    pub body: String,
    pub patch_set_commit_id: String,
}

#[derive(Debug, Serialize)]
pub struct ThreadRow {
    pub id: i64,
    pub change_id: String,
    pub path: String,
    pub side: Side,
    pub line: i64,
    pub fingerprint: String,
    /// A JSON array of context lines, passed through to the browser untouched.
    pub context: String,
    pub resolved: bool,
    pub created_by: String,
    pub created_at: i64,
    pub comments: Vec<CommentRow>,
}

#[derive(Debug, Serialize)]
pub struct ApprovalRow {
    pub change_id: String,
    pub commit_id: String,
    /// The patch id of the diff this person read. Empty only for rows written before approvals
    /// were keyed this way; `republish_approvals` backfills those from the repository and the
    /// gate ignores them until it has, so a pre-existing approval fails closed rather than
    /// matching everything.
    pub patch_id: String,
    pub approved_by: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct CommentRow {
    pub id: i64,
    pub body: String,
    pub author: String,
    pub patch_set_commit_id: String,
    pub created_at: i64,
}

pub struct Review {
    conn: Connection,
}

impl Review {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn, true)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, false)
    }

    fn init(conn: Connection, require_wal: bool) -> Result<Self> {
        // SQLite answers with the mode it ended up in rather than erroring on one it cannot
        // honour, so the result has to be checked instead of assumed.
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if require_wal && !mode.eq_ignore_ascii_case("wal") {
            bail!("review database is not in WAL mode (found {mode})");
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let review = Review { conn };
        review.migrate()?;
        Ok(review)
    }

    /// Idempotent, and run on every open. There is no version table: tables and indexes are
    /// `IF NOT EXISTS`, and a new column would be added by probing `pragma_table_info` the way
    /// the worker's store does.
    pub fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        // Approvals were originally keyed to the commit id alone, so a database that predates
        // the change has no patch_id. `ALTER TABLE` is not idempotent, hence the check; the
        // DEFAULT in SCHEMA covers databases created fresh.
        if !self.has_column("approval", "patch_id")? {
            self.conn.execute_batch(
                "ALTER TABLE approval ADD COLUMN patch_id TEXT NOT NULL DEFAULT ''",
            )?;
        }
        Ok(())
    }

    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Fill in the patch id of an approval recorded before they were stored.
    pub fn backfill_patch_id(
        &self,
        change_id: &str,
        commit_id: &str,
        patch_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE approval SET patch_id = ?3
             WHERE change_id = ?1 AND commit_id = ?2 AND patch_id = ''",
            params![change_id, commit_id, patch_id],
        )?;
        Ok(())
    }

    /// Start a thread. A thread is never empty, so this writes its first comment too, in one
    /// transaction — a thread with no remark would render as an anchor with nothing to say.
    pub fn start_thread(&mut self, new: &NewThread) -> Result<ThreadRow> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO thread (change_id, path, side, line, fingerprint, context, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                new.change_id,
                new.path,
                new.side.as_str(),
                new.line,
                new.fingerprint,
                new.context,
                new.created_by,
            ],
        )?;
        let thread_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO comment (thread_id, body, author, patch_set_commit_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![thread_id, new.body, new.created_by, new.patch_set_commit_id],
        )?;
        tx.commit()?;
        self.thread(thread_id)?
            .ok_or_else(|| anyhow!("the thread vanished immediately after being written"))
    }

    /// Add a remark to an existing thread. Returns `None` if the thread is gone.
    pub fn reply(
        &self,
        thread_id: i64,
        body: &str,
        author: &str,
        patch_set_commit_id: &str,
    ) -> Result<Option<ThreadRow>> {
        let changed = self.conn.execute(
            "INSERT INTO comment (thread_id, body, author, patch_set_commit_id)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM thread WHERE id = ?1)",
            params![thread_id, body, author, patch_set_commit_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.thread(thread_id)
    }

    pub fn set_resolved(&self, thread_id: i64, resolved: bool) -> Result<Option<ThreadRow>> {
        let changed = self.conn.execute(
            "UPDATE thread SET resolved = ?2 WHERE id = ?1",
            params![thread_id, resolved],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.thread(thread_id)
    }

    pub fn thread(&self, thread_id: i64) -> Result<Option<ThreadRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, change_id, path, side, line, fingerprint, context, resolved, created_by,
                    created_at
             FROM thread WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![thread_id], thread_from_row)
            .optional()?
            .transpose()?;
        match row {
            Some(mut thread) => {
                thread.comments = self.comments_for(thread.id)?;
                Ok(Some(thread))
            }
            None => Ok(None),
        }
    }

    /// Every thread on a change, oldest first, each with its comments.
    pub fn threads_for_change(&self, change_id: &str) -> Result<Vec<ThreadRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, change_id, path, side, line, fingerprint, context, resolved, created_by,
                    created_at
             FROM thread WHERE change_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![change_id], thread_from_row)?;
        let mut threads = Vec::new();
        for row in rows {
            let mut thread = row??;
            thread.comments = self.comments_for(thread.id)?;
            threads.push(thread);
        }
        Ok(threads)
    }

    /// Record that `who` has read this exact patch set. Approving twice is not an error.
    pub fn approve(
        &self,
        change_id: &str,
        commit_id: &str,
        patch_id: &str,
        who: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO approval (change_id, commit_id, patch_id, approved_by)
             VALUES (?1, ?2, ?3, ?4)",
            params![change_id, commit_id, patch_id, who],
        )?;
        Ok(())
    }

    /// Withdraw one person's approval of one patch set.
    ///
    /// Withdrawing is deliberately possible: noticing a problem after approving is ordinary, and
    /// the alternative — an approval that can only be invalidated by pushing a new patch set —
    /// would make people hesitate before approving at all.
    pub fn withdraw(&self, change_id: &str, commit_id: &str, who: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM approval WHERE change_id = ?1 AND commit_id = ?2 AND approved_by = ?3",
            params![change_id, commit_id, who],
        )?;
        Ok(())
    }

    pub fn approvals_for_change(&self, change_id: &str) -> Result<Vec<ApprovalRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT change_id, commit_id, patch_id, approved_by, created_at
             FROM approval WHERE change_id = ?1 ORDER BY created_at, approved_by",
        )?;
        let rows = stmt.query_map(params![change_id], approval_from_row)?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(row?);
        }
        Ok(approvals)
    }

    /// Every approval, for projecting the file the push gate reads.
    ///
    /// Ordered so the file is byte-identical for identical state: it is rewritten on every
    /// change, and a file whose lines shuffle produces noise in anything watching it.
    pub fn all_approvals(&self) -> Result<Vec<ApprovalRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT change_id, commit_id, patch_id, approved_by, created_at
             FROM approval ORDER BY change_id, commit_id, approved_by",
        )?;
        let rows = stmt.query_map([], approval_from_row)?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(row?);
        }
        Ok(approvals)
    }

    fn comments_for(&self, thread_id: i64) -> Result<Vec<CommentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, body, author, patch_set_commit_id, created_at
             FROM comment WHERE thread_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            Ok(CommentRow {
                id: row.get(0)?,
                body: row.get(1)?,
                author: row.get(2)?,
                patch_set_commit_id: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        let mut comments = Vec::new();
        for row in rows {
            comments.push(row?);
        }
        Ok(comments)
    }
}

fn approval_from_row(row: &Row<'_>) -> rusqlite::Result<ApprovalRow> {
    Ok(ApprovalRow {
        change_id: row.get(0)?,
        commit_id: row.get(1)?,
        patch_id: row.get(2)?,
        approved_by: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// Nested Result so an unrecognised `side` is a typed error rather than a panic, matching the
/// worker's mappers.
fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<Result<ThreadRow>> {
    let side: String = row.get(3)?;
    let side = match Side::parse(&side) {
        Ok(side) => side,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(ThreadRow {
        id: row.get(0)?,
        change_id: row.get(1)?,
        path: row.get(2)?,
        side,
        line: row.get(4)?,
        fingerprint: row.get(5)?,
        context: row.get(6)?,
        resolved: row.get::<_, i64>(7)? != 0,
        created_by: row.get(8)?,
        created_at: row.get(9)?,
        comments: Vec::new(),
    }))
}
