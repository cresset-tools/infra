//! The file the push gate reads.
//!
//! `refs/heads/main` is gated by a git `update` hook, which runs as the `git` user inside
//! `receive-pack` with no network, no jj, and a few milliseconds to make a decision. It needs to
//! know which commits have been approved.
//!
//! It reads a flat file rather than the review database, and that is a deliberate choice rather
//! than a shortcut:
//!
//! - **No cross-user SQLite.** A read-only connection to a WAL database wants to create the
//!   `-shm` index, which needs write access to the directory. That is precisely what leaves
//!   `/api/sync` reporting no projects today; repeating it here would mean a push failing
//!   because of a database lock.
//! - **No new failure mode on the push path.** `grep` on a small file cannot block, cannot
//!   deadlock, and cannot be mid-transaction.
//! - **Legible under pressure.** When someone is trying to work out why a push was refused at an
//!   awkward moment, `cat` answers the question.
//!
//! The database stays authoritative; this is a projection of it, rewritten whole on every
//! change and regenerated at startup so a lost or stale file repairs itself.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::review::ApprovalRow;

/// Readable by the owning service and by the `git` group that runs the hook.
///
/// The group half only works because the directory is setgid (see the tmpfiles rule in
/// hosts/internal/configuration.nix): without it, a file created here belongs to
/// cresset-view's own group and the hook cannot read the one file it exists to read. The
/// failure is closed and loud, but it looks exactly like correct configuration.
const MODE: u32 = 0o640;

/// Render the file's contents. Separated from writing it so the format can be tested without a
/// filesystem, and so the hook's expectations are stated in one place.
///
/// One `<change-id> <commit-id>` per line, deduplicated: two people approving the same patch set
/// is two rows in the database and one line here, because the gate asks whether a commit was
/// approved, not by how many people.
pub fn render(approvals: &[ApprovalRow]) -> String {
    let mut lines: Vec<String> = approvals
        .iter()
        .map(|approval| format!("{} {}", approval.change_id, approval.commit_id))
        .collect();
    lines.dedup();
    let mut out = String::from(
        "# Approved patch sets, written by cresset-view. Do not edit: it is rewritten\n\
         # from the review database on every approval and at startup.\n\
         # <jj change id> <git commit id>\n",
    );
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Write the file atomically: a temporary file in the same directory, then rename.
///
/// The gate reads this while pushes are happening. A partial file is worse than a stale one — it
/// would refuse a push that should have been allowed, and the reason would be invisible.
pub fn write(path: &Path, approvals: &[ApprovalRow]) -> Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let temporary = directory.join(format!(
        ".{}.new",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("approved")
    ));

    let mut file = fs::File::create(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(render(approvals).as_bytes())?;
    // Durable before the rename: the point of the rename is that the file is either the old
    // contents or the new ones, and that guarantee is empty if the new contents are still in
    // the page cache when the machine goes down.
    file.sync_all()?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(MODE))?;
    drop(file);

    fs::rename(&temporary, path)
        .with_context(|| format!("renaming {} into place", temporary.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(change: &str, commit: &str, who: &str) -> ApprovalRow {
        ApprovalRow {
            change_id: change.into(),
            commit_id: commit.into(),
            approved_by: who.into(),
            created_at: 0,
        }
    }

    #[test]
    fn the_format_is_what_the_hook_greps_for() {
        let rendered = render(&[approval("kkly", "abc123", "jelle")]);
        assert!(
            rendered.lines().any(|line| line == "kkly abc123"),
            "the hook matches a whole line, so the pair must be alone on one: {rendered}"
        );
        assert!(
            rendered
                .lines()
                .filter(|line| !line.starts_with('#'))
                .count()
                == 1,
            "only the pairs, plus comments: {rendered}"
        );
    }

    #[test]
    fn two_people_approving_one_patch_set_is_one_line() {
        // `all_approvals` orders by (change, commit, approver), so duplicates are adjacent and
        // `dedup` is enough. If that ordering is ever dropped this test is what notices.
        let rendered = render(&[
            approval("kkly", "abc123", "ann"),
            approval("kkly", "abc123", "jelle"),
        ]);
        assert_eq!(
            rendered
                .lines()
                .filter(|line| *line == "kkly abc123")
                .count(),
            1
        );
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("approved");
        write(&path, &[approval("kkly", "abc123", "jelle")]).expect("written");
        assert!(
            fs::read_to_string(&path)
                .expect("read")
                .contains("kkly abc123")
        );

        // Rewritten whole: an approval that was withdrawn must not survive in the file, or the
        // gate would keep letting a commit through after the person who read it changed their
        // mind.
        write(&path, &[]).expect("rewritten");
        let contents = fs::read_to_string(&path).expect("read");
        assert!(
            !contents.contains("abc123"),
            "stale line survived: {contents}"
        );

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("listed")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "approved")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn the_hook_can_read_it_but_the_world_cannot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("approved");
        write(&path, &[]).expect("written");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, MODE, "the git group must be able to read it");
    }
}
