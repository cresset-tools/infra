//! Reading and answering review comments from a terminal.
//!
//! Two binaries share this: `cresset-review`, which runs where a person or an agent is, and
//! `cresset-review-shell`, the ssh forced command it talks to on the host. The parsing lives
//! here because it is the security boundary — the shell binary's whole input is a string the
//! client chose — and because a boundary with no tests is a hope.
//!
//! ## Why this exists
//!
//! cresset-view's API trusts `x-authentik-username` from the proxy. From a terminal there is no
//! Authentik session, so the only way in was to set that header yourself — signing your remarks
//! with someone else's name in the one system whose job is recording who said what. An agent
//! tried exactly that, its permission classifier refused, and it asked for a real path instead.
//!
//! ## The property that makes it safe
//!
//! **The identity comes from the key, never from the caller.** Each `authorized_keys` entry
//! pins `command="cresset-review-shell <identity>"`, so a client picks the verb and never the
//! name attached to what it says. `Command::parse` is given only the client's words; the
//! identity is a separate argument it cannot reach.
//!
//! ## Waiting is a verb, because polling by hand is not
//!
//! An agent that submits work and then stops has handed the thread back to a person who may be
//! asleep; one that re-runs `threads` on a timer burns a session doing it. `wait` is the
//! watcher: one blocking call that ends when the change is approved — or when a reviewer says
//! something, which is the part a sleep would get wrong. It cannot approve anything, so it
//! changes nothing about the restriction below.
//!
//! ## What is deliberately missing
//!
//! There is no approve and no merge. An agent holding an identity here must not be able to
//! approve its own work — that is precisely what the review gate was built to stop. The
//! restriction is the design, not an unfinished edge, and [`Command::parse`] rejects both by
//! name so the refusal says something useful rather than "unknown command".

use anyhow::{Result, bail};

/// A change id as jj writes it: 32 letters of reversed hex.
fn is_change_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_lowercase())
}

/// A thread id: a plain positive integer, bounded so it cannot be a novel.
fn is_thread_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 18 && value.bytes().all(|b| b.is_ascii_digit())
}

/// How long `wait` keeps watching when no deadline is given, and the most it will accept.
///
/// Capped rather than unbounded because a wait is an ssh connection held open on the host, and
/// one left behind by a forgotten agent should expire on its own. A caller who wants longer runs
/// it again — which is also the only honest way to resume, since the baseline it compares
/// against is the moment it started.
pub const DEFAULT_WAIT_SECONDS: u64 = 24 * 60 * 60;

/// Seconds, as a deadline for `wait`. Zero is allowed and means "read once and say".
fn parse_seconds(value: &str) -> Result<u64> {
    if value.is_empty() || value.len() > 6 || !value.bytes().all(|b| b.is_ascii_digit()) {
        bail!("not a number of seconds: {}", describe(value));
    }
    let seconds: u64 = value.parse().expect("digits of bounded length");
    if seconds > DEFAULT_WAIT_SECONDS {
        bail!("a wait is capped at {DEFAULT_WAIT_SECONDS} seconds; run it again to keep waiting");
    }
    Ok(seconds)
}

/// What a caller asked for, once it has been proven to be one of the things on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Every change open for review.
    Changes,
    /// The comment threads on one change.
    Threads { change: String },
    /// Answer a thread. The body arrives on stdin, not here: prose on a command line is a
    /// quoting accident waiting to mangle someone's comment.
    Reply { change: String, thread: String },
    /// Mark a thread resolved, or reopen it.
    Resolve { thread: String, resolved: bool },
    /// Block until a change is approved, or until a reviewer says something that needs an
    /// answer. The verb an agent runs as a watcher after submitting work; see `shell.rs` for
    /// what ends the wait and why those are the terminal states.
    Wait { change: String, seconds: u64 },
    /// Say what the verbs are.
    Help,
}

impl Command {
    /// Parse the words a client sent.
    ///
    /// Everything here is attacker-controlled. Nothing is executed, nothing is interpolated
    /// before it has been checked, and an id that does not match its shape is refused before it
    /// can reach a URL — so `../../../etc/passwd` and `7;rm -rf /` fail on the pattern rather
    /// than on something downstream being careful.
    pub fn parse(words: &[&str]) -> Result<Command> {
        let Some(verb) = words.first() else {
            return Ok(Command::Help);
        };
        let arg = |n: usize| words.get(n).copied().unwrap_or_default();

        match *verb {
            "" | "help" | "-h" | "--help" => Ok(Command::Help),
            "changes" => Ok(Command::Changes),
            "threads" => {
                let change = arg(1);
                if !is_change_id(change) {
                    bail!("not a change id: {}", describe(change));
                }
                Ok(Command::Threads {
                    change: change.to_owned(),
                })
            }
            "reply" => {
                let (change, thread) = (arg(1), arg(2));
                if !is_change_id(change) {
                    bail!("not a change id: {}", describe(change));
                }
                if !is_thread_id(thread) {
                    bail!("not a thread id: {}", describe(thread));
                }
                Ok(Command::Reply {
                    change: change.to_owned(),
                    thread: thread.to_owned(),
                })
            }
            "wait" => {
                let change = arg(1);
                if !is_change_id(change) {
                    bail!("not a change id: {}", describe(change));
                }
                // The only verb that reads a THIRD word, so unlike the others it validates what
                // it finds there rather than ignoring it: a mistyped deadline must not silently
                // become the 24-hour default.
                let seconds = match words.get(2) {
                    Some(text) => parse_seconds(text)?,
                    None => DEFAULT_WAIT_SECONDS,
                };
                Ok(Command::Wait {
                    change: change.to_owned(),
                    seconds,
                })
            }
            verb @ ("resolve" | "unresolve") => {
                let thread = arg(1);
                if !is_thread_id(thread) {
                    bail!("not a thread id: {}", describe(thread));
                }
                Ok(Command::Resolve {
                    thread: thread.to_owned(),
                    resolved: verb == "resolve",
                })
            }
            // Named explicitly so the refusal explains itself. "unknown command: approve" would
            // read like an oversight; this says it is a decision.
            "approve" | "merge" | "submit" | "land" => bail!(
                "{verb} is not available here, and that is deliberate: approving and landing are \
                 decisions a person takes in cresset-view while looking at the change. A key \
                 that could take them would defeat the gate they exist for."
            ),
            other => bail!("unknown command: {} (try: help)", describe(other)),
        }
    }
}

/// Quote a rejected argument for an error message without letting it misrepresent itself.
///
/// Control characters in a refusal are how a crafted argument forges a second line of output,
/// so they are escaped rather than printed, and anything long is truncated.
fn describe(value: &str) -> String {
    if value.is_empty() {
        return "<missing>".to_owned();
    }
    let shown: String = value
        .chars()
        .take(40)
        .flat_map(char::escape_default)
        .collect();
    if value.chars().count() > 40 {
        format!("\"{shown}\"…")
    } else {
        format!("\"{shown}\"")
    }
}

pub const USAGE: &str = "\
cresset-review — read and answer review comments on the Cresset monorepo.

  changes                            every change open for review
  threads <change-id>                comment threads on a change
  reply <change-id> <thread-id>      add a comment; the body is read from stdin
  resolve <thread-id>                mark a thread resolved
  unresolve <thread-id>              reopen it
  wait <change-id> [seconds]         block until it is approved, or until a reviewer
                                     says something; 0 reads once and reports

`wait` prints one JSON object when it stops and exits 0 only if the change is approved,
so `cresset-review wait <id> && jj git push -b main` is a safe thing to write. It stops
early on a new comment, because a reviewer who asks a question is not going to approve
until it is answered.

Approving and merging are deliberately absent: they are decisions a person makes in
cresset-view, and a key that could make them would defeat the gate they exist for.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command> {
        Command::parse(&line.split_whitespace().collect::<Vec<_>>())
    }

    const CHANGE: &str = "kklyskvtvvstsmxyvnvptnrzknsqxnvw";

    #[test]
    fn the_verbs_on_offer_parse() {
        assert_eq!(parse("changes").unwrap(), Command::Changes);
        assert_eq!(
            parse(&format!("threads {CHANGE}")).unwrap(),
            Command::Threads {
                change: CHANGE.into()
            }
        );
        assert_eq!(
            parse(&format!("reply {CHANGE} 7")).unwrap(),
            Command::Reply {
                change: CHANGE.into(),
                thread: "7".into()
            }
        );
        assert_eq!(
            parse("resolve 7").unwrap(),
            Command::Resolve {
                thread: "7".into(),
                resolved: true
            }
        );
        assert_eq!(
            parse("unresolve 7").unwrap(),
            Command::Resolve {
                thread: "7".into(),
                resolved: false
            }
        );
        assert_eq!(parse("").unwrap(), Command::Help);
        assert_eq!(
            parse(&format!("wait {CHANGE}")).unwrap(),
            Command::Wait {
                change: CHANGE.into(),
                seconds: DEFAULT_WAIT_SECONDS
            }
        );
        assert_eq!(
            parse(&format!("wait {CHANGE} 90")).unwrap(),
            Command::Wait {
                change: CHANGE.into(),
                seconds: 90
            }
        );
        // Zero is a status check, not a missing argument.
        assert_eq!(
            parse(&format!("wait {CHANGE} 0")).unwrap(),
            Command::Wait {
                change: CHANGE.into(),
                seconds: 0
            }
        );
    }

    /// A deadline is checked rather than assumed, so a typo is a refusal and not a day of
    /// waiting. This is the one place a third word means something, which is why it is the one
    /// place a third word can be wrong.
    #[test]
    fn a_deadline_that_is_not_seconds_is_refused() {
        for bad in ["5m", "-1", "1e3", "90s", &"9".repeat(7)] {
            assert!(
                parse(&format!("wait {CHANGE} {bad}")).is_err(),
                "{bad:?} must be refused as a deadline"
            );
        }
        // Not reachable through SSH_ORIGINAL_COMMAND, which is split on whitespace and so never
        // yields an empty word — but `cresset-review wait <id> ""` reaches the same parser from
        // argv, where it is, so it is refused rather than read as "no deadline given".
        assert!(Command::parse(&["wait", CHANGE, ""]).is_err());
        let error = parse(&format!("wait {CHANGE} 999999"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("capped"),
            "and a deadline past the cap must say so: {error}"
        );
    }

    /// The restriction that makes it safe to give an agent a key at all.
    #[test]
    fn approving_and_landing_are_refused_by_name() {
        for verb in ["approve", "merge", "submit", "land"] {
            let error = parse(&format!("{verb} {CHANGE}")).unwrap_err().to_string();
            assert!(
                error.contains("deliberate"),
                "{verb} must explain itself rather than read as an oversight: {error}"
            );
        }
    }

    /// Ids are checked against their shape before anything downstream sees them.
    #[test]
    fn a_malformed_id_never_reaches_a_url() {
        for bad in [
            "../../../etc/passwd",
            "kklyskvtvvstsmxyvnvptnrzknsqxnv",   // 31: one short
            "kklyskvtvvstsmxyvnvptnrzknsqxnvwx", // 33: one long
            "KKLYSKVTVVSTSMXYVNVPTNRZKNSQXNVW",  // upper case is not a jj change id
            "kklyskvtvvstsmxyvnvptnrzknsqxn2w",  // digits are not either
            "",
        ] {
            assert!(
                parse(&format!("threads {bad}")).is_err(),
                "{bad:?} must be refused as a change id"
            );
        }
        for bad in ["7;rm -rf /", "-1", "7a", "", &"9".repeat(19)] {
            assert!(
                parse(&format!("resolve {bad}")).is_err(),
                "{bad:?} must be refused as a thread id"
            );
        }
    }

    /// Extra words cannot smuggle anything in: the verb and its arguments are read by position
    /// and everything beyond them is ignored, never appended to a request.
    #[test]
    fn trailing_words_are_ignored_rather_than_used() {
        assert_eq!(
            parse(&format!(
                "threads {CHANGE} --header x-authentik-username:someone"
            ))
            .unwrap(),
            Command::Threads {
                change: CHANGE.into()
            },
            "a client cannot append anything to the request it asked for"
        );
    }

    /// A refusal must not let the rejected text forge a second line of output.
    #[test]
    fn a_crafted_argument_cannot_fake_an_error_line() {
        let error = parse("threads bad\nerror: approved by jelle")
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains('\n'),
            "control characters must be escaped, not printed: {error:?}"
        );
    }
}
