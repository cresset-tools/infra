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
