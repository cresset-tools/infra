//! What a change *does*, independent of what it sits on.
//!
//! An approval says "I read this". Keying it to a commit id makes that true but brittle: a
//! rebase gives every commit a new id while changing nothing anybody read, so main moving
//! underneath a change invalidated every approval on it. That is not a conservative gate, it is
//! a gate that cannot tell a rebase from an edit — and the cost is paid by asking a person to
//! re-read text they have already read, which is the surest way to make approval perfunctory.
//!
//! So an approval is keyed to the change id AND the patch id of its diff:
//!
//! - a clean rebase keeps both, so it stays approved with no copying step;
//! - an amend keeps the change id and changes the diff, so it needs a fresh approval;
//! - a conflict resolution changes the diff, so it needs one too.
//!
//! The change id alone would not do: it survives amends, which is the whole point of patch
//! sets, so on its own it would let unreviewed edits land.
//!
//! **`git patch-id --stable`, in a subprocess, on both sides.** The push gate computes this
//! inside `receive-pack` where there is no jj, so it must shell out; this service could instead
//! diff through jj-lib, and must not. Two implementations that agree today would be one commit
//! away from disagreeing, and the symptom of disagreement is an approval that silently stops
//! counting — or worse, one that counts when it should not. Being the same binary is the only
//! form of agreement that cannot drift.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

/// A commit with no diff at all.
///
/// Deliberately not 40 hex characters, so it can never collide with a real patch id. An empty
/// commit is reviewable (there is nothing in it to object to) and jj produces them routinely, so
/// refusing to approve one would be an odd place to draw a line.
pub const EMPTY: &str = "empty-diff";

/// Find the git object store behind a jj repository.
///
/// jj's git backend records its target in `.jj/repo/store/git_target`, as a path relative to
/// that `store` directory — `../../../.git` for a colocated repo. A workspace's `.jj/repo` is a
/// *file* holding the path of the real repo directory, so that indirection is followed first.
pub fn resolve_git_dir(repository: &Path) -> Result<PathBuf> {
    let mut repo_dir = repository.join(".jj/repo");
    if repo_dir.is_file() {
        let pointed = std::fs::read_to_string(&repo_dir)
            .with_context(|| format!("reading {}", repo_dir.display()))?;
        let pointed = PathBuf::from(pointed.trim());
        repo_dir = if pointed.is_absolute() {
            pointed
        } else {
            repository.join(".jj").join(pointed)
        };
    }

    let store = repo_dir.join("store");
    let target_file = store.join("git_target");
    let target = std::fs::read_to_string(&target_file)
        .with_context(|| format!("reading {}", target_file.display()))?;
    let target = PathBuf::from(target.trim());

    let git_dir = if target.is_absolute() {
        target
    } else {
        store.join(target)
    };
    git_dir
        .canonicalize()
        .with_context(|| format!("resolving the git directory at {}", git_dir.display()))
}

/// The patch id of a commit's diff against its parent.
///
/// Fails closed on a merge commit: `diff-tree` prints nothing for one without `-m`, and an empty
/// diff is indistinguishable from an empty commit. Review changes are linear and main is
/// fast-forward only, so a merge here means something unexpected is happening and "I cannot tell
/// what this changes" is the honest answer.
pub fn of_commit(git_dir: &Path, commit_id: &str) -> Result<String> {
    if !commit_id.chars().all(|c| c.is_ascii_hexdigit()) || commit_id.is_empty() {
        bail!("not a commit id: {commit_id:?}");
    }

    let parents = run(git_dir, &["rev-list", "--parents", "-1", commit_id], None)?;
    // `<commit> <parent>...` — one field is a root commit, two is ordinary, more is a merge.
    if parents.split_whitespace().count() > 2 {
        bail!("{commit_id} is a merge commit; cannot say what it changes on its own");
    }

    let diff = run(
        git_dir,
        &["diff-tree", "-p", "--no-commit-id", commit_id],
        None,
    )?;
    if diff.trim().is_empty() {
        return Ok(EMPTY.to_string());
    }

    let out = run(git_dir, &["patch-id", "--stable"], Some(diff.as_bytes()))?;
    out.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("git patch-id said nothing about {commit_id}"))
}

fn run(git_dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

    let mut child = command
        .spawn()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if let Some(bytes) = stdin {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(bytes)?;
        // Dropped so git sees EOF; without this, `patch-id` waits forever and so do we.
        drop(child.stdin.take());
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway git repository. Plain git rather than jj: `of_commit` only ever talks to the
    /// git object store, and the point of these tests is what patch ids do across a rebase.
    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = Self { dir };
            repo.git(&["init", "-q", "-b", "main"]);
            repo.git(&["config", "user.email", "t@example.invalid"]);
            repo.git(&["config", "user.name", "T"]);
            repo
        }

        fn path(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }

        fn git_dir(&self) -> PathBuf {
            self.path().join(".git")
        }

        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .current_dir(self.dir.path())
                .args(args)
                .output()
                .expect("git ran");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        fn commit(&self, file: &str, contents: &str, message: &str) -> String {
            std::fs::write(self.dir.path().join(file), contents).expect("wrote");
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    #[test]
    fn a_rebase_does_not_change_what_a_commit_does() {
        let repo = Repo::new();
        let base = repo.commit("f.txt", "base\n", "base");
        let original = repo.commit("f.txt", "base\nchange\n", "work");

        // Somebody else lands something untouched by this change, and the change moves on top.
        repo.git(&["checkout", "-q", "-b", "other", &base]);
        repo.commit("g.txt", "landed\n", "meanwhile");
        repo.git(&["cherry-pick", &original]);
        let rebased = repo.git(&["rev-parse", "HEAD"]);

        assert_ne!(original, rebased, "the rebase must produce a new commit id");
        assert_eq!(
            of_commit(&repo.git_dir(), &original).expect("original"),
            of_commit(&repo.git_dir(), &rebased).expect("rebased"),
            "a rebase that changes nothing anybody read must keep its approval"
        );
    }

    #[test]
    fn an_amend_does_change_what_a_commit_does() {
        let repo = Repo::new();
        repo.commit("f.txt", "base\n", "base");
        let original = repo.commit("f.txt", "base\nchange\n", "work");
        let before = of_commit(&repo.git_dir(), &original).expect("original");

        std::fs::write(repo.path().join("f.txt"), "base\nchange, revised\n").expect("wrote");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "--amend", "--no-edit"]);
        let amended = repo.git(&["rev-parse", "HEAD"]);

        // This is the property the whole gate rests on. If an amend kept its patch id, editing a
        // change after approval would land without anybody reading the edit.
        assert_ne!(
            before,
            of_commit(&repo.git_dir(), &amended).expect("amended"),
            "an amend must need a fresh approval"
        );
    }

    #[test]
    fn moving_a_change_onto_an_edit_of_the_same_lines_needs_rereading() {
        let repo = Repo::new();
        let base = repo.commit("f.txt", "one\ntwo\nthree\n", "base");
        let original = repo.commit("f.txt", "one\ntwo\nthree\nfour\n", "append four");

        // The base now differs in the lines surrounding the change, so the diff that lands is
        // not the diff that was read. Stricter than it has to be, and in the safe direction.
        repo.git(&["checkout", "-q", "-b", "other", &base]);
        repo.commit("f.txt", "one\nTWO\nthree\n", "meanwhile, nearby");
        repo.git(&["cherry-pick", &original]);
        let rebased = repo.git(&["rev-parse", "HEAD"]);

        assert_ne!(
            of_commit(&repo.git_dir(), &original).expect("original"),
            of_commit(&repo.git_dir(), &rebased).expect("rebased"),
            "a rebase over an edit to the same region is not a trivial one"
        );
    }

    #[test]
    fn an_empty_commit_gets_a_patch_id_that_cannot_collide() {
        let repo = Repo::new();
        repo.commit("f.txt", "base\n", "base");
        repo.git(&["commit", "-q", "--allow-empty", "-m", "nothing"]);
        let empty = repo.git(&["rev-parse", "HEAD"]);

        let id = of_commit(&repo.git_dir(), &empty).expect("empty commit");
        assert_eq!(id, EMPTY);
        assert!(
            id.len() != 40 || !id.chars().all(|c| c.is_ascii_hexdigit()),
            "the sentinel must be impossible to confuse with a real patch id: {id}"
        );
    }

    #[test]
    fn a_merge_is_refused_rather_than_called_empty() {
        let repo = Repo::new();
        let base = repo.commit("f.txt", "base\n", "base");
        repo.commit("f.txt", "base\nleft\n", "left");
        repo.git(&["checkout", "-q", "-b", "right", &base]);
        repo.commit("g.txt", "right\n", "right");
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        let merge = repo.git(&["rev-parse", "HEAD"]);

        // `diff-tree` prints nothing for a merge, which would otherwise read as EMPTY — and an
        // empty approval would then match a merge carrying anything at all.
        let error = of_commit(&repo.git_dir(), &merge).expect_err("must refuse");
        assert!(
            error.to_string().contains("merge"),
            "the refusal should say why: {error}"
        );
    }

    #[test]
    fn a_commit_id_is_checked_before_it_reaches_git() {
        let repo = Repo::new();
        for hostile in ["", "--help", "HEAD; rm -rf /", "../../etc/passwd"] {
            assert!(
                of_commit(&repo.git_dir(), hostile).is_err(),
                "{hostile:?} should not reach git"
            );
        }
    }

    #[test]
    fn the_git_store_behind_a_jj_repository_is_found() {
        // The shape jj leaves on disk for a colocated repo: .jj/repo/store/git_target holding a
        // path relative to the store directory. Worth a test because getting it wrong means the
        // service cannot compute patch ids at all, and the failure is at approval time.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".jj/repo/store")).expect("store");
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::write(root.join(".jj/repo/store/git_target"), "../../../.git\n").expect("target");

        assert_eq!(
            resolve_git_dir(root).expect("resolved"),
            root.join(".git").canonicalize().expect("canonical")
        );
    }
}
