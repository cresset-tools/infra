//! `preview serve`: GitHub's push webhook in, previews out.
//!
//! A push to a branch builds that commit and deploys it into the branch's
//! preview, bringing one up first if there is none. A push that deletes a
//! branch takes its preview down. Jobs run one at a time, and pushes that
//! arrive while a branch is waiting collapse into the newest one.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State as AxState;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::{Mutex, Notify};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Deploy { branch: String, sha: String },
    Down { branch: String },
}

impl Job {
    fn branch(&self) -> &str {
        match self {
            Job::Deploy { branch, .. } | Job::Down { branch } => branch,
        }
    }
}

/// Waiting jobs, at most one per branch, in arrival order of their branch.
#[derive(Default)]
pub struct Queue {
    order: VecDeque<String>,
    jobs: BTreeMap<String, Job>,
}

impl Queue {
    pub fn push(&mut self, job: Job) {
        let branch = job.branch().to_string();
        if self.jobs.insert(branch.clone(), job).is_none() {
            self.order.push_back(branch);
        }
    }
    pub fn pop(&mut self) -> Option<Job> {
        let branch = self.order.pop_front()?;
        self.jobs.remove(&branch)
    }
}

struct App {
    cfg: Config,
    secret: Vec<u8>,
    queue: Mutex<Queue>,
    wake: Notify,
}

#[derive(Deserialize)]
struct Push {
    #[serde(rename = "ref")]
    git_ref: String,
    after: String,
    #[serde(default)]
    deleted: bool,
}

/// Checks GitHub's `X-Hub-Signature-256: sha256=<hex>` over the raw body.
pub fn signature_ok(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(hex_sig) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(sig) = hex::decode(hex_sig) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// What a webhook delivery asks for, if anything.
pub fn job_for(event: &str, body: &[u8], ignore: &[String]) -> Result<Option<Job>> {
    if event != "push" {
        return Ok(None);
    }
    let push: Push = serde_json::from_slice(body).context("parsing the push event")?;
    let Some(branch) = push.git_ref.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    if ignore.iter().any(|b| b == branch) || crate::name_for_branch(branch).is_empty() {
        return Ok(None);
    }
    Ok(Some(if push.deleted {
        Job::Down {
            branch: branch.to_string(),
        }
    } else {
        Job::Deploy {
            branch: branch.to_string(),
            sha: push.after,
        }
    }))
}

async fn hook(
    AxState(app): AxState<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !signature_ok(&app.secret, &body, header("x-hub-signature-256")) {
        return (StatusCode::UNAUTHORIZED, "bad signature\n".into());
    }
    let event = header("x-github-event").unwrap_or("");
    let ignore = app
        .cfg
        .webhook
        .as_ref()
        .map(|w| w.ignore_branches.clone())
        .unwrap_or_default();
    match job_for(event, &body, &ignore) {
        Ok(Some(job)) => {
            eprintln!("preview: queued {job:?}");
            app.queue.lock().await.push(job);
            app.wake.notify_one();
            (StatusCode::ACCEPTED, "queued\n".into())
        }
        Ok(None) => (StatusCode::OK, "nothing to do\n".into()),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}\n")),
    }
}

async fn status(AxState(app): AxState<Arc<App>>) -> (StatusCode, String) {
    match crate::state::State::load(&app.cfg.state_dir) {
        Ok(s) => (
            StatusCode::OK,
            serde_json::to_string_pretty(&s).unwrap_or_default(),
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")),
    }
}

pub async fn run_job(cfg: &Config, job: Job) -> Result<()> {
    match job {
        Job::Down { branch } => {
            let name = crate::name_for_branch(&branch);
            let state = crate::state::State::load(&cfg.state_dir)?;
            if state.slot_of(&name).is_some() {
                crate::ops::down(cfg, &name).await?;
                eprintln!("preview: {name} is down");
            }
        }
        Job::Deploy { branch, sha } => {
            let name = crate::name_for_branch(&branch);
            let started = std::time::Instant::now();
            let (project, artifact) = crate::build::build(cfg, &name, &branch, &sha).await?;
            let built = started.elapsed();
            let slot = crate::ops::up(cfg, &name, None, Some(&branch)).await?;
            crate::build::deploy(cfg, &name, slot, &project, &artifact).await?;
            crate::ops::set_deployed(cfg, slot, &sha).await?;
            eprintln!(
                "preview: {} at {} in slot {slot}, built in {:.1?}, live after {:.1?}",
                cfg.url(&name),
                &sha[..sha.len().min(12)],
                built,
                started.elapsed()
            );
        }
    }
    Ok(())
}

pub async fn serve(cfg: Config) -> Result<()> {
    let w = cfg
        .webhook
        .clone()
        .context("no [webhook] section in the configuration")?;
    let secret = std::fs::read(&w.secret_file)
        .with_context(|| format!("reading {}", w.secret_file.display()))?;
    let secret = String::from_utf8_lossy(&secret).trim().as_bytes().to_vec();
    let app = Arc::new(App {
        cfg,
        secret,
        queue: Mutex::new(Queue::default()),
        wake: Notify::new(),
    });

    let worker = Arc::clone(&app);
    tokio::spawn(async move {
        loop {
            let job = worker.queue.lock().await.pop();
            match job {
                Some(job) => {
                    let what = format!("{job:?}");
                    if let Err(e) = run_job(&worker.cfg, job).await {
                        eprintln!("preview: {what} failed: {e:#}");
                    }
                }
                None => worker.wake.notified().await,
            }
        }
    });

    let router = Router::new()
        .route("/hook", post(hook))
        .route("/status", get(status))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&w.listen)
        .await
        .with_context(|| format!("listening on {}", w.listen))?;
    eprintln!("preview: listening on {}", w.listen);
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signatures_are_checked() {
        let body = br#"{"ref":"refs/heads/x","after":"abc"}"#;
        assert!(signature_ok(b"s", body, Some(&sign(b"s", body))));
        assert!(!signature_ok(b"t", body, Some(&sign(b"s", body))));
        assert!(!signature_ok(b"s", body, None));
        assert!(!signature_ok(b"s", body, Some("sha256=zz")));
    }

    #[test]
    fn pushes_become_jobs() {
        let ignore = vec!["main".to_string()];
        let push = br#"{"ref":"refs/heads/feat/x","after":"abc","deleted":false}"#;
        assert_eq!(
            job_for("push", push, &ignore).unwrap(),
            Some(Job::Deploy {
                branch: "feat/x".into(),
                sha: "abc".into()
            })
        );
        let gone = br#"{"ref":"refs/heads/feat/x","after":"000","deleted":true}"#;
        assert_eq!(
            job_for("push", gone, &ignore).unwrap(),
            Some(Job::Down {
                branch: "feat/x".into()
            })
        );
        let main = br#"{"ref":"refs/heads/main","after":"abc"}"#;
        assert_eq!(job_for("push", main, &ignore).unwrap(), None);
        let tag = br#"{"ref":"refs/tags/v1","after":"abc"}"#;
        assert_eq!(job_for("push", tag, &ignore).unwrap(), None);
        assert_eq!(job_for("ping", b"{}", &ignore).unwrap(), None);
    }

    #[test]
    fn a_waiting_branch_keeps_only_its_newest_push() {
        let mut q = Queue::default();
        q.push(Job::Deploy {
            branch: "a".into(),
            sha: "1".into(),
        });
        q.push(Job::Deploy {
            branch: "b".into(),
            sha: "1".into(),
        });
        q.push(Job::Deploy {
            branch: "a".into(),
            sha: "2".into(),
        });
        assert_eq!(
            q.pop(),
            Some(Job::Deploy {
                branch: "a".into(),
                sha: "2".into()
            })
        );
        assert_eq!(
            q.pop(),
            Some(Job::Deploy {
                branch: "b".into(),
                sha: "1".into()
            })
        );
        assert_eq!(q.pop(), None);
    }
}
