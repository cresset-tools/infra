use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod approvals;
mod review;

use anyhow::{Context, Result, anyhow, bail};
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as AxumPath, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures::channel::mpsc;
use futures::{AsyncReadExt, SinkExt, StreamExt, TryStreamExt};
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::config::StackedConfig;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::conflicts::{
    ConflictMarkerStyle, ConflictMaterializeOptions, MaterializedTreeValue,
    materialize_merge_result_to_bytes, materialize_tree_value,
};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::MergedTreeValue;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo, StoreFactories};
use jj_lib::repo_path::RepoPath;
use jj_lib::revset::{RevsetExpression, RevsetStreamExt};
use jj_lib::settings::UserSettings;
use jj_lib::tree_merge::MergeOptions;
use jj_lib::workspace::{Workspace, default_working_copy_factories};
use serde::{Deserialize, Serialize};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "CRESSET_VIEW_REPOSITORY")]
    repository: PathBuf,

    #[arg(long, env = "CRESSET_VIEW_ASSETS", default_value = "web/dist")]
    assets: PathBuf,

    #[arg(long, env = "CRESSET_VIEW_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,

    /// Where review threads are stored. Absent means review is read-only: the queue and patch
    /// sets still work, and writing returns a clear error rather than silently discarding.
    #[arg(long, env = "CRESSET_VIEW_REVIEW_DB")]
    review_db: Option<PathBuf>,

    /// Where to push a merge, e.g. `git@localhost:cresset.git`.
    ///
    /// Absent means the Merge button is not offered: reviewing still works and landing is done
    /// from a terminal. Present, this service can push to the canonical repository — and ONLY
    /// push, over ssh as an ordinary client, so the same `update` hook decides whether the push
    /// is allowed. It has no filesystem access to that repository.
    #[arg(long, env = "CRESSET_VIEW_MERGE_REMOTE")]
    merge_remote: Option<String>,

    /// The private key for `--merge-remote`. Required with it, useless without it.
    #[arg(long, env = "CRESSET_VIEW_MERGE_SSH_KEY")]
    merge_ssh_key: Option<PathBuf>,

    /// Where to remember the merge remote's host key.
    #[arg(long, default_value = "/var/lib/cresset-view/known_hosts")]
    known_hosts: PathBuf,

    /// Where to project approvals for the canonical repository's push gate to read.
    ///
    /// Optional, and absent on any instance that is not the one beside the canonical repo. See
    /// `approvals.rs` for why the gate reads a file rather than the database.
    #[arg(long, env = "CRESSET_VIEW_APPROVALS_FILE")]
    approvals_file: Option<PathBuf>,

    /// The cresset-sync checkpoint database, read read-only to surface synchronization state.
    ///
    /// Optional on purpose. cresset-view is a repository viewer first; the worker may not be
    /// deployed, may not have run yet, or may be on another host. Its absence removes a panel,
    /// it does not break the service.
    #[arg(long, env = "CRESSET_VIEW_SYNC_DB")]
    sync_db: Option<PathBuf>,

    #[arg(long)]
    check: bool,

    /// Answer requests that carry no identity header as this user. Local development only.
    ///
    /// Deliberately has no environment variable: an env var lingers in a unit file or a shell
    /// profile long after anyone remembers setting it, and this one would linger as "the
    /// authentication is off". A flag has to be typed into the command line that starts the
    /// server, every time, by someone looking at it. For the same reason it refuses to start
    /// on a non-loopback listener.
    #[arg(long)]
    dev_identity: Option<String>,
}

#[derive(Clone)]
struct AppState {
    repository: Arc<PathBuf>,
    sync_db: Option<Arc<PathBuf>>,
    review_db: Option<Arc<PathBuf>>,
    approvals_file: Option<Arc<PathBuf>>,
    merge: Option<Arc<MergeConfig>>,
}

/// What is needed to land a stack. Both halves or neither: a remote with no key cannot
/// authenticate, and a key with no remote has nowhere to go.
struct MergeConfig {
    remote: String,
    ssh_key: PathBuf,
    /// Learned on first connect and kept, rather than disabling host key checking outright.
    /// The remote is the loopback address of this same machine, so the exposure is a host that
    /// already runs this service; recording the key still catches a later change.
    known_hosts: PathBuf,
}

#[derive(Debug)]
struct AppError(anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!(error = ?self.0, "request failed");
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: self.0.to_string(),
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Serialize)]
struct RepositoryResponse {
    operation_id: String,
    workspace: String,
}

#[derive(Serialize)]
struct RevisionResponse {
    operation_id: String,
    head_count: usize,
    revisions: Vec<Revision>,
    /// Whether another page exists after this one. Computed by reading one revision past the
    /// page rather than by counting the whole revset, so it costs one extra commit.
    has_more: bool,
}

#[derive(Serialize)]
struct Revision {
    change_id: String,
    commit_id: String,
    parent_commit_ids: Vec<String>,
    description: String,
    author_name: String,
    author_email: String,
    authored_at: String,
    has_conflict: bool,
    divergent: bool,
    working_copy: bool,
    is_head: bool,
    bookmarks: Vec<String>,
}

/// One change awaiting review: a commit carried by a `review/*` bookmark that has not landed.
#[derive(Serialize)]
struct ChangeSummary {
    /// Stable across amends — this is what a review thread will hang off.
    change_id: String,
    /// The commit realising the change right now, i.e. its latest patch set.
    commit_id: String,
    description: String,
    author_name: String,
    authored_at: String,
    /// How many versions have been pushed. 1 means it has not been revised yet.
    patch_sets: usize,
    has_conflict: bool,
}

/// One review bookmark and the changes it carries, which is Gerrit's relation chain.
///
/// Grouped rather than flattened because landing is a property of the STACK, not of a change:
/// advancing main to the tip lands everything beneath it, so the reader has to see what else
/// comes along. The previous flat list also mislabelled mid-stack commits — a commit partway up
/// carries no bookmark of its own, and the fallback picked an arbitrary one, so with two review
/// bookmarks open a change could be shown under the wrong one.
#[derive(Serialize)]
struct Stack {
    bookmark: String,
    /// The commit landing this stack would move main to.
    tip: String,
    /// Oldest first: the order they would land in, and the order the gate reports them in.
    changes: Vec<ChangeSummary>,
}

#[derive(Serialize)]
struct ChangesResponse {
    operation_id: String,
    stacks: Vec<Stack>,
}

#[derive(Serialize)]
struct PatchSet {
    number: u32,
    commit_id: String,
    /// False once a newer patch set exists. The reviewer needs to know whether they are
    /// reading the version that would land.
    current: bool,
}

#[derive(Serialize)]
struct ChangeDetail {
    operation_id: String,
    change_id: String,
    /// The change as it stands, in the same shape the revision endpoints return.
    current: Revision,
    #[serde(skip_serializing_if = "Option::is_none")]
    bookmark: Option<String>,
    patch_sets: Vec<PatchSet>,
}

#[derive(Serialize)]
struct BookmarkResponse {
    operation_id: String,
    bookmarks: Vec<Bookmark>,
}

#[derive(Serialize)]
struct Bookmark {
    name: String,
    added_commit_ids: Vec<String>,
    removed_commit_ids: Vec<String>,
    conflicted: bool,
}

#[derive(Serialize)]
struct TreeResponse {
    operation_id: String,
    change_id: String,
    commit_id: String,
    paths: Vec<TreePath>,
}

#[derive(Serialize)]
struct TreePath {
    path: String,
    kind: &'static str,
    conflicted: bool,
}

#[derive(Serialize)]
struct FileResponse {
    operation_id: String,
    change_id: String,
    commit_id: String,
    path: String,
    contents: Option<String>,
    conflicted: bool,
    binary: bool,
    /// The conflict, when the path has one and its terms are readable.
    ///
    /// Previously the API said only `conflicted: true` and returned no content, so the viewer
    /// could say a conflict existed and nothing more. That is the screen the sync worker's
    /// whole escalation path arrives at — Telegram carries a pointer here, and every project
    /// stays paused until someone acts on it — so "there is a conflict" was an answer that
    /// sent the reader to ssh and `jj` for the actual question.
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict: Option<ConflictView>,
}

/// A conflicted path, decomposed the way a person reads it.
#[derive(Serialize)]
struct ConflictView {
    /// The common ancestors. A normal 3-way merge has exactly one.
    bases: Vec<ConflictTerm>,
    /// The competing versions. A normal 3-way merge has two.
    sides: Vec<ConflictTerm>,
    /// jj's own materialisation: the conflict-marker text it would write into a working copy.
    ///
    /// Kept alongside the decomposed terms because it is what `jj` shows and what a resolver
    /// actually edits, so it is the form to compare against when checking whether a proposed
    /// resolution matches. `None` when a term is binary or oversized.
    #[serde(skip_serializing_if = "Option::is_none")]
    materialized: Option<String>,
}

#[derive(Serialize)]
struct ConflictTerm {
    /// jj's own label for this term, when it has one — e.g. the operation that produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// `None` for a term that is binary, oversized, or absent (a delete on one side).
    #[serde(skip_serializing_if = "Option::is_none")]
    contents: Option<String>,
    /// The term does not exist on this side — one side deleted the file.
    absent: bool,
    binary: bool,
}

#[derive(Serialize)]
struct FileDiff {
    index: usize,
    path: String,
    before: Option<String>,
    after: Option<String>,
    conflicted: bool,
    binary: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DiffEvent {
    Metadata {
        operation_id: String,
        change_id: String,
        commit_id: String,
        paths: Vec<String>,
    },
    File(FileDiff),
    Error {
        error: String,
    },
}

#[derive(Deserialize)]
struct RevisionQuery {
    limit: Option<usize>,
    /// How many matching revisions to skip. Paging is by offset within one operation id:
    /// the response carries the operation the page was read at, so a client can notice the
    /// repository moved underneath it rather than silently stitching two histories together.
    offset: Option<usize>,
    /// Case-insensitive filter over description, author name/email, and change/commit id
    /// prefixes. Matched against the commit itself, never interpolated into a revset — user
    /// input must not become revset syntax.
    q: Option<String>,
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

#[derive(Deserialize)]
struct DiffQuery {
    path: Option<String>,
}

/// Refuse any request that arrives without an authenticated identity.
///
/// This CANNOT be enforced in nginx, which is where it was first attempted. `if (...) return
/// 403` belongs to the rewrite phase, and `auth_request` to the access phase that runs after
/// it — so the guard evaluated the identity variable before the outpost had ever been called,
/// found it empty every time, and refused everyone unconditionally. Written as a fail-closed
/// gate, it was in fact a wall, and it was indistinguishable from working for as long as
/// nobody could get in.
///
/// Enforcing it here is phase-independent, and it is the better place regardless: the service
/// must not trust the proxy to have *authenticated* anyone, only to have reported who it is.
/// That trust is sound solely because this listener is bound to loopback behind that proxy —
/// if it is ever exposed directly, this header becomes attacker-controlled and the check is
/// worthless. Bind it to loopback.
///
/// `/health` is exempt so a local probe does not need an identity.
///
/// `dev_identity` (`--dev-identity`) stands in for the proxy when developing locally: a request
/// with no identity header is treated as that user instead of being refused. A header that IS
/// present still wins, so a dev instance behind a real proxy behaves normally.
async fn require_identity(
    dev_identity: Option<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }
    let identity = request
        .headers()
        .get("x-authentik-username")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if identity.is_empty() && dev_identity.is_none() {
        return (
            StatusCode::FORBIDDEN,
            "authentication is required to view this repository",
        )
            .into_response();
    }
    // Carry the identity to the handlers. It used to be checked and thrown away, which was
    // enough while everything was read-only; a comment has an author. The proxy header wins,
    // falling back to --dev-identity so local development behaves the same.
    let who = if identity.is_empty() {
        dev_identity.as_deref().unwrap_or_default().to_string()
    } else {
        identity.to_string()
    };
    let mut request = request;
    request.extensions_mut().insert(Identity(who));
    next.run(request).await
}

/// The authenticated user, as the proxy asserted them.
#[derive(Clone, Debug)]
struct Identity(String);

/// Refuse a write a browser was talked into making from another site.
///
/// Authentication here is a proxy header, so ANY request the user's browser makes carries it —
/// a tab on a hostile page is an authenticated actor regardless of intent. Browsers label
/// cross-site requests with `Sec-Fetch-Site`, so an explicit non-same-origin value is refused.
/// An absent header means a non-browser client (curl, the tests), which cannot be induced to
/// forge a request on someone else's behalf.
fn ensure_same_origin(headers: &axum::http::HeaderMap) -> Result<()> {
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        None | Some("same-origin") | Some("none") => Ok(()),
        Some(other) => bail!("refusing a {other} write; this endpoint is same-origin only"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    if args.check {
        let loaded = load_repository(&args.repository).await?;
        println!("{}", loaded.repo.op_id().hex());
        return Ok(());
    }
    let dev_identity: Option<Arc<str>> = args.dev_identity.as_deref().map(Arc::from);
    if let Some(identity) = &dev_identity {
        // The identity check is sound only while the header is unforgeable, and --dev-identity
        // removes the need to forge it at all — acceptable solely where "whoever can reach the
        // port" already means "whoever is at this keyboard".
        let loopback = args
            .listen
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|address| address.ip().is_loopback());
        if !loopback {
            anyhow::bail!(
                "--dev-identity disables authentication, so it refuses to serve on {}; \
                 bind a loopback address like 127.0.0.1:8080",
                args.listen
            );
        }
        tracing::warn!(identity = %identity, "authentication disabled: requests without an identity header are served as this user");
    }
    let state = AppState {
        repository: Arc::new(args.repository),
        sync_db: args.sync_db.map(Arc::new),
        review_db: args.review_db.map(Arc::new),
        approvals_file: args.approvals_file.map(Arc::new),
        merge: match (args.merge_remote, args.merge_ssh_key) {
            (Some(remote), Some(ssh_key)) => Some(Arc::new(MergeConfig {
                remote,
                ssh_key,
                known_hosts: args.known_hosts,
            })),
            (None, None) => None,
            _ => bail!("--merge-remote and --merge-ssh-key must be given together"),
        },
    };
    // Regenerate before serving. The file is a projection of the database, and the push gate
    // fails closed on a missing one — so a file lost to a redeployed state directory would
    // silently block every push until someone approved something new. Rebuilding it at startup
    // makes that self-repairing rather than a mystery.
    if let Err(error) = republish_approvals(&state) {
        tracing::error!(%error, "could not write the approvals file; pushes to main will be refused");
    }
    let index = args.assets.join("index.html");
    let static_files = ServeDir::new(&args.assets).not_found_service(ServeFile::new(index));

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/repository", get(repository))
        .route("/api/identity", get(identity))
        .route("/api/revisions", get(revisions))
        .route("/api/revisions/{id}", get(revision))
        .route("/api/bookmarks", get(bookmarks))
        .route("/api/changes", get(changes))
        .route("/api/changes/{id}", get(change))
        .route(
            "/api/changes/{id}/threads",
            get(list_threads).post(create_thread),
        )
        .route("/api/threads/{id}/comments", post(reply_to_thread))
        .route("/api/threads/{id}/resolve", post(resolve_thread))
        .route(
            "/api/changes/{id}/approvals",
            get(list_approvals).post(set_approval),
        )
        .route("/api/approvals", get(all_approvals))
        .route("/api/merge", post(merge))
        .route("/api/sync", get(sync_status))
        .route("/api/revisions/{id}/tree", get(tree))
        .route("/api/revisions/{id}/file", get(file))
        .route("/api/revisions/{id}/diff", get(diff))
        .fallback_service(static_files)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
        // Outermost, so it runs before anything else touches the request.
        .layer(middleware::from_fn(move |request, next| {
            require_identity(dev_identity.clone(), request, next)
        }));

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("failed to bind {}", args.listen))?;
    tracing::info!(address = %args.listen, "cresset-view listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------------------
// Synchronization status.
//
// The worker is a timer oneshot, so nothing is listening between passes and its state cannot be
// asked for over a socket — it lives in SQLite. Reading it here is what turns a Telegram
// notification's deep link into something worth following: the operator lands on the conflicted
// revision AND can see which projects are blocked, how stale the fleet is, and why.
//
// Read-only in every sense. This endpoint opens the database read-only, never writes, and never
// fails the service: if the worker is not deployed, has not run, or its database cannot be read,
// the panel reports itself unavailable and the viewer carries on being a repository viewer.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SyncResponse {
    /// False when there is no database to read. The frontend hides the panel rather than
    /// showing an alarming empty one.
    available: bool,
    /// Why it is unavailable, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    /// Seconds since a pass last completed, which is the fleet's liveness signal: it grows
    /// whether the worker is blocked, crashed, or simply never fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_pass_age_secs: Option<i64>,
    projects: Vec<SyncProject>,
    incomplete_operations: usize,
}

#[derive(Serialize)]
struct SyncProject {
    id: String,
    /// `ready`, `conflict` or `blocked` — what the worker discovered.
    status: String,
    /// Whether an operator has turned synchronization on for this project. Distinct from
    /// `status`: one is a decision, the other is a finding.
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    downstream_head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    monorepo_commit_id: Option<String>,
    /// For a blocked project this names the conflict bookmark and the `resolve` invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    /// The operation awaiting resolution, if any — what `cresset-sync resolve` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_operation_id: Option<String>,
    /// The conflicted commit, so the UI can link straight to it.
    ///
    /// A commit id specifically, not the `sync/conflict/*` bookmark: this viewer resolves
    /// revisions by change or commit id prefix and would reject a bookmark name outright. The
    /// Telegram escalation links the same way for the same reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_commit: Option<String>,
}

async fn sync_status(State(state): State<AppState>) -> Json<SyncResponse> {
    let Some(path) = state.sync_db.clone() else {
        return Json(unavailable("no synchronization database is configured"));
    };
    // Blocking SQLite work off the async runtime.
    let response = tokio::task::spawn_blocking(move || read_sync_state(path.as_ref()))
        .await
        .unwrap_or_else(|e| unavailable(&format!("reading synchronization state panicked: {e}")));
    Json(response)
}

fn unavailable(reason: &str) -> SyncResponse {
    SyncResponse {
        available: false,
        unavailable_reason: Some(reason.to_string()),
        last_pass_age_secs: None,
        projects: Vec::new(),
        incomplete_operations: 0,
    }
}

fn read_sync_state(path: &Path) -> SyncResponse {
    // Read-only: this process must never be able to alter the worker's authoritative state, and
    // opening it this way makes that structural rather than a matter of discipline.
    let conn = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(e) => return unavailable(&format!("cannot open {}: {e}", path.display())),
    };

    // Wait briefly for a contended read, then give up. Deliberately much shorter than the
    // worker's own timeout: this is serving an HTTP request, and an unavailable panel is a far
    // better outcome than a page that hangs because a WAL checkpoint is in progress.
    if let Err(e) = conn.busy_timeout(std::time::Duration::from_millis(750)) {
        return unavailable(&format!("cannot configure the reader: {e}"));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last_pass_age_secs = conn
        .query_row(
            "SELECT value FROM sync_meta WHERE key = 'last_pass_completed_at'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|at| now.saturating_sub(at));

    let conflicts = read_open_conflicts(&conn).unwrap_or_default();
    let mut projects = read_projects(&conn).unwrap_or_default();
    for project in &mut projects {
        if let Some((operation_id, commit)) = conflicts.get(&project.id) {
            project.conflict_operation_id = Some(operation_id.clone());
            project.conflict_commit = commit.clone();
        }
    }
    let incomplete_operations = conn
        .query_row(
            "SELECT COUNT(*) FROM operation WHERE state NOT IN ('committed', 'failed')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize;

    SyncResponse {
        available: true,
        unavailable_reason: None,
        last_pass_age_secs,
        projects,
        incomplete_operations,
    }
}

fn read_projects(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<SyncProject>> {
    let mut stmt = conn.prepare(
        "SELECT id, status, enabled, downstream_head_sha, monorepo_commit_id, last_error
         FROM project ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SyncProject {
            id: row.get(0)?,
            status: row.get(1)?,
            enabled: row.get::<_, i64>(2).unwrap_or(0) != 0,
            downstream_head_sha: row.get(3)?,
            monorepo_commit_id: row.get(4)?,
            last_error: row.get(5)?,
            conflict_operation_id: None,
            conflict_commit: None,
        })
    })?;
    rows.collect()
}

/// The import operation each project is currently stuck on, with the conflicted commit.
///
/// Newest first, so a project that has somehow accumulated more than one shows the live one.
fn read_open_conflicts(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<BTreeMap<String, (String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT project_id, id, result_jj_commit_id FROM operation
         WHERE kind = 'import' AND state NOT IN ('committed', 'failed')
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            (row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?),
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (project, entry) = row?;
        out.entry(project).or_insert(entry);
    }
    Ok(out)
}

async fn repository(State(state): State<AppState>) -> Result<Json<RepositoryResponse>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        Ok(RepositoryResponse {
            operation_id: loaded.repo.op_id().hex(),
            workspace: loaded.workspace_name,
        })
    })
    .await?;
    Ok(Json(response))
}

async fn revisions(
    State(state): State<AppState>,
    Query(query): Query<RevisionQuery>,
) -> Result<Json<RevisionResponse>, AppError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    // An all-whitespace `q` is the same as no filter: a search box that has been cleared to
    // spaces should show the history, not nothing.
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let mut stream = RevsetExpression::all()
            .evaluate(loaded.repo.as_ref())?
            .stream()
            .commits(loaded.repo.store());

        // Walk the revset, keeping only the requested window of MATCHING revisions.
        //
        // Filtering here rather than in the revset is deliberate. jj's revset language has
        // `description()`/`author()` functions, but building an expression out of user input
        // means user input becomes syntax — a search for `)` or `|` would either error or,
        // worse, parse into a different query than the one that was typed.
        //
        // `revision_from_commit` runs only for the page, so an unmatched commit costs a
        // string compare rather than a full render.
        let mut seen = 0usize;
        let mut page = Vec::with_capacity(limit.min(64));
        while let Some(commit) = stream.try_next().await? {
            if let Some(needle) = &needle
                && !commit_matches(&commit, needle)
            {
                continue;
            }
            if seen >= offset {
                page.push(commit);
                // One past the page: enough to know another exists, without counting the rest.
                if page.len() > limit {
                    break;
                }
            }
            seen += 1;
        }
        let has_more = page.len() > limit;
        page.truncate(limit);

        let mut revisions = Vec::with_capacity(page.len());
        for commit in page {
            revisions.push(revision_from_commit(loaded.repo.as_ref(), &commit)?);
        }

        Ok(RevisionResponse {
            operation_id: loaded.repo.op_id().hex(),
            head_count: loaded.repo.view().heads().len(),
            revisions,
            has_more,
        })
    })
    .await?;
    Ok(Json(response))
}

/// Does `commit` match a search needle (already lowercased)?
///
/// Description and author are substring matches; ids are PREFIX matches, because a substring
/// match on a hex id turns any short query into noise — nearly every two-character string
/// appears somewhere in some id.
fn commit_matches(commit: &Commit, needle: &str) -> bool {
    commit.description().to_lowercase().contains(needle)
        || commit.author().name.to_lowercase().contains(needle)
        || commit.author().email.to_lowercase().contains(needle)
        || commit.change_id().reverse_hex().starts_with(needle)
        || commit.id().hex().starts_with(needle)
}

/// The review queue: changes on a `review/*` bookmark that have not landed on `main`.
///
/// "Not landed" is the whole definition of open, and it is computed rather than tracked —
/// there is no status column to fall out of step with the repository. Landing a change makes
/// it disappear from here because it becomes an ancestor of `main`, with nothing to update.
async fn changes(State(state): State<AppState>) -> Result<Json<ChangesResponse>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let repo = loaded.repo.as_ref();
        let view = repo.view();

        // Every review bookmark and the commit it points at. Local AND remote: the viewer's
        // repository only ever fetches, so a review bookmark exists there as `review/x@origin`
        // and never locally — reading only local bookmarks returned an empty queue against a
        // real repository.
        let mut tips: Vec<(String, CommitId)> = Vec::new();
        let mut landed = Vec::new();
        for (name, targets) in view.bookmarks() {
            let name = name.as_str().to_owned();
            if name == "main" {
                landed.extend(targets.local_target.added_ids().cloned());
                for (_, remote) in &targets.remote_refs {
                    landed.extend(remote.target.added_ids().cloned());
                }
                continue;
            }
            if !name.starts_with("review/") {
                continue;
            }
            let ids = targets.local_target.added_ids().chain(
                targets
                    .remote_refs
                    .iter()
                    .flat_map(|(_, r)| r.target.added_ids()),
            );
            for id in ids {
                if !tips.iter().any(|(_, existing)| existing == id) {
                    tips.push((name.clone(), id.clone()));
                }
            }
        }

        // One revset per bookmark rather than one for all of them. Grouping after the fact
        // cannot say which stack a mid-stack commit belongs to, because the commit itself
        // carries no bookmark — the answer is which tip it is an ancestor of.
        let landed = RevsetExpression::commits(landed).ancestors();
        let mut stacks = Vec::new();
        for (bookmark, tip) in tips {
            let open = RevsetExpression::commits(vec![tip.clone()])
                .ancestors()
                .minus(&landed);
            let commits = open
                .evaluate(repo)?
                .stream()
                .commits(repo.store())
                .try_collect::<Vec<_>>()
                .await?;

            let mut changes = Vec::new();
            for commit in commits {
                let change_id = commit.change_id().reverse_hex();
                let patch_sets = read_patch_sets(repo, &change_id)?.len();
                changes.push(ChangeSummary {
                    change_id,
                    commit_id: commit.id().hex(),
                    description: commit.description().to_owned(),
                    author_name: commit.author().name.clone(),
                    authored_at: commit.author().timestamp.to_datetime()?.to_rfc3339(),
                    patch_sets,
                    has_conflict: commit.has_conflict(),
                });
            }
            // The revset streams newest first; landing order is the opposite, and the gate
            // lists unapproved commits oldest first too.
            changes.reverse();
            if changes.is_empty() {
                // The bookmark has landed and not yet been deleted. Nothing to review.
                continue;
            }
            stacks.push(Stack {
                bookmark,
                tip: tip.hex(),
                changes,
            });
        }
        stacks.sort_by(|a, b| a.bookmark.cmp(&b.bookmark));

        Ok(ChangesResponse {
            operation_id: repo.op_id().hex(),
            stacks,
        })
    })
    .await?;
    Ok(Json(response))
}

/// One change and every version of it that has been pushed.
async fn change(
    State(state): State<AppState>,
    AxumPath(change_id): AxumPath<String>,
) -> Result<Json<ChangeDetail>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let repo = loaded.repo.as_ref();
        let commit = resolve_visible_commit(repo, &change_id).await?;
        let change_id = commit.change_id().reverse_hex();
        let current = revision_from_commit(repo, &commit)?;

        let recorded = read_patch_sets(repo, &change_id)?;
        let latest = recorded.last().map(|(number, _)| *number);
        let patch_sets = recorded
            .into_iter()
            .map(|(number, commit_id)| PatchSet {
                number,
                commit_id,
                current: Some(number) == latest,
            })
            .collect();

        let bookmark = repo
            .view()
            .bookmarks()
            .filter(|(name, _)| name.as_str().starts_with("review/"))
            .find(|(_, targets)| {
                targets.local_target.added_ids().any(|id| id == commit.id())
                    || targets
                        .remote_refs
                        .iter()
                        .any(|(_, r)| r.target.added_ids().any(|id| id == commit.id()))
            })
            .map(|(name, _)| name.as_str().to_owned());

        Ok(ChangeDetail {
            operation_id: repo.op_id().hex(),
            change_id,
            current,
            bookmark,
            patch_sets,
        })
    })
    .await?;
    Ok(Json(response))
}

/// Open the review store, or explain why writing is unavailable.
///
/// Opened per request, matching how the sync database is read: a `Connection` is not `Sync`,
/// and SQLite handles concurrent opens perfectly well.
fn open_review(state: &AppState) -> Result<review::Review> {
    let Some(path) = state.review_db.as_ref() else {
        bail!("review is read-only on this instance: no review database is configured");
    };
    review::Review::open(path.as_ref())
}

#[derive(Deserialize)]
struct NewThreadBody {
    path: String,
    side: String,
    line: i64,
    /// The anchored line's text and its surrounding lines, captured by the browser. Stored
    /// verbatim; the server never interprets them.
    fingerprint: String,
    context: String,
    body: String,
    /// Which patch set the reader was looking at.
    patch_set_commit_id: String,
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
    patch_set_commit_id: String,
}

#[derive(Deserialize)]
struct ResolveBody {
    resolved: bool,
}

async fn list_threads(
    State(state): State<AppState>,
    AxumPath(change_id): AxumPath<String>,
) -> Result<Json<Vec<review::ThreadRow>>, AppError> {
    let db = open_review(&state)?;
    Ok(Json(db.threads_for_change(&change_id)?))
}

async fn create_thread(
    State(state): State<AppState>,
    AxumPath(change_id): AxumPath<String>,
    Extension(identity): Extension<Identity>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewThreadBody>,
) -> Result<Json<review::ThreadRow>, AppError> {
    ensure_same_origin(&headers)?;
    if body.body.trim().is_empty() {
        return Err(anyhow!("a comment needs something in it").into());
    }
    let mut db = open_review(&state)?;
    let thread = db.start_thread(&review::NewThread {
        change_id,
        path: body.path,
        side: review::Side::parse(&body.side)?,
        line: body.line,
        fingerprint: body.fingerprint,
        context: body.context,
        created_by: identity.0,
        body: body.body,
        patch_set_commit_id: body.patch_set_commit_id,
    })?;
    Ok(Json(thread))
}

async fn reply_to_thread(
    State(state): State<AppState>,
    AxumPath(thread_id): AxumPath<i64>,
    Extension(identity): Extension<Identity>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ReplyBody>,
) -> Result<Json<review::ThreadRow>, AppError> {
    ensure_same_origin(&headers)?;
    if body.body.trim().is_empty() {
        return Err(anyhow!("a comment needs something in it").into());
    }
    let db = open_review(&state)?;
    match db.reply(
        thread_id,
        &body.body,
        &identity.0,
        &body.patch_set_commit_id,
    )? {
        Some(thread) => Ok(Json(thread)),
        None => Err(anyhow!("no thread {thread_id}").into()),
    }
}

async fn resolve_thread(
    State(state): State<AppState>,
    AxumPath(thread_id): AxumPath<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ResolveBody>,
) -> Result<Json<review::ThreadRow>, AppError> {
    ensure_same_origin(&headers)?;
    let db = open_review(&state)?;
    match db.set_resolved(thread_id, body.resolved)? {
        Some(thread) => Ok(Json(thread)),
        None => Err(anyhow!("no thread {thread_id}").into()),
    }
}

#[derive(Serialize)]
struct IdentityResponse {
    username: String,
    /// Whether this instance can land a stack. The UI hides Merge when it cannot, rather than
    /// offering a button whose only possible answer is that the button does not work here.
    can_merge: bool,
}

/// Who the proxy says the caller is.
///
/// The browser needs this to render "your" approval differently from someone else's, and it
/// cannot know it any other way: the header is added by nginx and never reaches the page.
/// Read-only and derived entirely from the request, so it says nothing a caller did not already
/// prove by getting this far.
async fn identity(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
) -> Json<IdentityResponse> {
    Json(IdentityResponse {
        username: who.0,
        can_merge: state.merge.is_some(),
    })
}

#[derive(Deserialize)]
struct ApprovalBody {
    /// The exact patch set being approved. Required, and not inferred from "the latest": the
    /// reviewer approves what they read, and between loading the page and pressing the button a
    /// new patch set may have been pushed.
    commit_id: String,
    approved: bool,
}

#[derive(Serialize)]
struct ApprovalsResponse {
    change_id: String,
    approvals: Vec<review::ApprovalRow>,
    /// Whether this instance can gate pushes at all. False means approving still records who
    /// read what, but nothing enforces it — worth saying rather than implying a gate that is
    /// not there.
    gated: bool,
}

async fn list_approvals(
    State(state): State<AppState>,
    AxumPath(change_id): AxumPath<String>,
) -> Result<Json<ApprovalsResponse>, AppError> {
    let db = open_review(&state)?;
    Ok(Json(ApprovalsResponse {
        approvals: db.approvals_for_change(&change_id)?,
        change_id,
        gated: state.approvals_file.is_some(),
    }))
}

async fn set_approval(
    State(state): State<AppState>,
    AxumPath(change_id): AxumPath<String>,
    Extension(identity): Extension<Identity>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ApprovalBody>,
) -> Result<Json<ApprovalsResponse>, AppError> {
    ensure_same_origin(&headers)?;
    // The gate matches on the exact string, so a short id or stray whitespace would record an
    // approval that can never be found and refuse the push it was meant to allow.
    let commit_id = body.commit_id.trim();
    if commit_id.len() != 40 || !commit_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("a patch set is identified by its full 40-character commit id").into());
    }

    let db = open_review(&state)?;
    if body.approved {
        db.approve(&change_id, commit_id, &identity.0)?;
    } else {
        db.withdraw(&change_id, commit_id, &identity.0)?;
    }

    // The file is what the push gate reads, so a failure to write it must reach the reviewer
    // rather than being logged: they would otherwise see an approval recorded and a push refused
    // for no visible reason.
    if let Some(path) = state.approvals_file.as_ref() {
        approvals::write(path.as_ref(), &db.all_approvals()?)
            .context("recording the approval for the push gate")?;
    }

    Ok(Json(ApprovalsResponse {
        approvals: db.approvals_for_change(&change_id)?,
        change_id,
        gated: state.approvals_file.is_some(),
    }))
}

/// Every approval on the instance, for colouring the queue without one request per change.
async fn all_approvals(
    State(state): State<AppState>,
) -> Result<Json<Vec<review::ApprovalRow>>, AppError> {
    let db = open_review(&state)?;
    Ok(Json(db.all_approvals()?))
}

#[derive(Deserialize)]
struct MergeBody {
    /// The review bookmark being landed. Carried for the message, not for the decision.
    bookmark: String,
    /// The exact commit main should move to. Named by the caller rather than re-read here, so
    /// a stack that gained a patch set between rendering the page and pressing the button
    /// lands what was on screen or nothing — never something unread.
    tip: String,
}

#[derive(Serialize)]
struct MergeResponse {
    bookmark: String,
    tip: String,
    /// git's own output. On success this is the ref update; on failure it carries the update
    /// hook's refusal, which names each unapproved change and where to review it.
    output: String,
}

/// Land a stack by pushing its tip to the canonical repository's main.
///
/// Deliberately a plain `git push` over ssh, exactly as a person would do it, rather than a
/// ref write. That is what keeps ONE gate: receive-pack runs `hooks/update`, which refuses a
/// rewrite, a deletion, or any commit nobody has approved. This service cannot reach the
/// repository any other way — it has no filesystem access to it — so the button cannot become
/// a way around the thing it exists to serve.
async fn merge(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: axum::http::HeaderMap,
    Json(body): Json<MergeBody>,
) -> Result<Json<MergeResponse>, AppError> {
    ensure_same_origin(&headers)?;
    let Some(config) = state.merge.as_ref().cloned() else {
        return Err(anyhow!(
            "merging is not available on this instance: no merge remote is configured"
        )
        .into());
    };
    let tip = body.tip.trim().to_owned();
    if tip.len() != 40 || !tip.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("a merge names its tip by full 40-character commit id").into());
    }

    let repository = state.repository.as_ref().clone();
    let bookmark = body.bookmark.clone();
    tracing::info!(who = %identity.0, %bookmark, %tip, "landing a stack");

    // Blocking: git is a subprocess and this is a button press, not a hot path.
    let output = tokio::task::spawn_blocking(move || push_to_main(&repository, &config, &tip))
        .await
        .context("running git push")??;

    Ok(Json(MergeResponse {
        bookmark: body.bookmark,
        tip: body.tip,
        output,
    }))
}

fn push_to_main(repository: &Path, config: &MergeConfig, tip: &str) -> Result<String> {
    // `-o IdentitiesOnly` so an agent or a stray key in the service's home cannot be used
    // instead of the one deployment intends. `accept-new` records the host key on first use
    // and refuses a later change, which is the useful half of strict checking for a loopback
    // remote that is this same machine.
    let ssh = format!(
        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={} -o BatchMode=yes",
        config.ssh_key.display(),
        config.known_hosts.display(),
    );
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .arg("push")
        .arg(&config.remote)
        .arg(format!("{tip}:refs/heads/main"))
        .env("GIT_SSH_COMMAND", ssh)
        // git writes its progress and the remote's messages to stderr, and the remote's
        // messages are the whole point of showing the result.
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("spawning git push")?;

    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    let text = text.trim().to_owned();
    if output.status.success() {
        Ok(text)
    } else {
        // Surfaced verbatim: the update hook's refusal names each unapproved change and links
        // to it, and paraphrasing that would lose the only actionable part.
        Err(anyhow!(
            "{}",
            if text.is_empty() {
                "git push failed".into()
            } else {
                text
            }
        ))
    }
}

/// Rebuild the approvals file from the database. Run at startup; see `approvals.rs`.
fn republish_approvals(state: &AppState) -> Result<()> {
    let Some(path) = state.approvals_file.as_ref() else {
        return Ok(());
    };
    let db = open_review(state).context("opening the review database to publish approvals")?;
    approvals::write(path.as_ref(), &db.all_approvals()?)
}

async fn bookmarks(State(state): State<AppState>) -> Result<Json<BookmarkResponse>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let bookmarks = loaded
            .repo
            .view()
            .local_bookmarks()
            .map(|(name, target)| Bookmark {
                name: name.as_str().to_owned(),
                added_commit_ids: target.added_ids().map(ObjectId::hex).collect(),
                removed_commit_ids: target.removed_ids().map(ObjectId::hex).collect(),
                conflicted: target.has_conflict(),
            })
            .collect();

        Ok(BookmarkResponse {
            operation_id: loaded.repo.op_id().hex(),
            bookmarks,
        })
    })
    .await?;
    Ok(Json(response))
}

async fn revision(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Revision>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let commit = resolve_visible_commit(loaded.repo.as_ref(), &id).await?;
        revision_from_commit(loaded.repo.as_ref(), &commit)
    })
    .await?;
    Ok(Json(response))
}

async fn tree(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TreeResponse>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let commit = resolve_visible_commit(loaded.repo.as_ref(), &id).await?;
        let mut paths = Vec::new();
        for (path, value) in commit.tree().entries() {
            let value = value?;
            let conflicted = !value.is_resolved();
            let kind = match value.into_resolved() {
                Ok(Some(TreeValue::File { .. })) => "file",
                Ok(Some(TreeValue::Symlink(_))) => "symlink",
                Ok(Some(TreeValue::GitSubmodule(_))) => "submodule",
                Ok(Some(TreeValue::Tree(_))) => "directory",
                Ok(None) => continue,
                Err(_) => "conflict",
            };
            paths.push(TreePath {
                path: path.as_internal_file_string().to_owned(),
                kind,
                conflicted,
            });
        }

        Ok(TreeResponse {
            operation_id: loaded.repo.op_id().hex(),
            change_id: commit.change_id().reverse_hex(),
            commit_id: commit.id().hex(),
            paths,
        })
    })
    .await?;
    Ok(Json(response))
}

async fn diff(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DiffQuery>,
) -> Result<Response, AppError> {
    let path = state.repository.as_ref().clone();
    let (loaded, commit, mut entries, paths, start_index, before_labels, after_labels) =
        run_jj(move || async move {
            let loaded = load_repository(&path).await?;
            let commit = resolve_visible_commit(loaded.repo.as_ref(), &id).await?;
            let before_tree = commit.parent_tree(loaded.repo.as_ref()).await?;
            let after_tree = commit.tree();
            // Each side's conflict labels, so a conflicted term in the diff can be materialised
            // with the same names the file view shows rather than as anonymous sides.
            let before_labels = before_tree.labels().clone();
            let after_labels = after_tree.labels().clone();
            let mut stream = before_tree.diff_stream(&after_tree, &EverythingMatcher);
            let mut entries = Vec::new();
            while let Some(entry) = stream.next().await {
                entries.push(entry);
            }
            let paths: Vec<_> = entries
                .iter()
                .map(|entry| entry.path.as_internal_file_string().to_owned())
                .collect();
            // Where to start the stream. An exact path first; failing that, the first change UNDER
            // it, so a directory works.
            //
            // Directories used to 400 with "requested path is not changed in this revision", which
            // is both unhelpful and, for a directory that plainly does contain changes, untrue. It
            // reads as though the viewer disagrees with you about the repository.
            let start_index = match query.path {
                Some(requested_path) => {
                    let prefix = format!("{}/", requested_path.trim_end_matches('/'));
                    paths
                        .iter()
                        .position(|path| path == &requested_path)
                        .or_else(|| paths.iter().position(|path| path.starts_with(&prefix)))
                        .ok_or_else(|| {
                            anyhow!("nothing under {requested_path} changed in this revision")
                        })?
                }
                None => 0,
            };
            Ok((
                loaded,
                commit,
                entries,
                paths,
                start_index,
                before_labels,
                after_labels,
            ))
        })
        .await?;
    let mut indexed_entries: Vec<_> = entries.drain(..).enumerate().collect();
    indexed_entries.rotate_left(start_index);
    let (mut sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(2);

    tokio::task::spawn_blocking(move || {
        pollster::block_on(async move {
            let result = async {
                send_diff_event(
                    &mut sender,
                    &DiffEvent::Metadata {
                        operation_id: loaded.repo.op_id().hex(),
                        change_id: commit.change_id().reverse_hex(),
                        commit_id: commit.id().hex(),
                        paths,
                    },
                )
                .await?;

                for (index, entry) in indexed_entries {
                    let path = entry.path.as_internal_file_string().to_owned();
                    let values = entry.values?;
                    let conflicted = !values.before.is_resolved() || !values.after.is_resolved();
                    // A conflicted side is materialised into jj's marker text rather than
                    // dropped.
                    //
                    // It used to read as `None`, so the Changes screen rendered a conflicted
                    // path as a deletion -- the one view where "what happened to this file"
                    // is the entire question, answering it with silence. Browse showed the
                    // conflict and Changes did not, which is the wrong way round if anything:
                    // Changes is where someone lands first.
                    let before = read_text_or_markers(
                        loaded.repo.as_ref(),
                        &entry.path,
                        values.before,
                        &before_labels,
                    )
                    .await?;
                    let after = read_text_or_markers(
                        loaded.repo.as_ref(),
                        &entry.path,
                        values.after,
                        &after_labels,
                    )
                    .await?;
                    let binary = matches!(before, FileContents::Binary)
                        || matches!(after, FileContents::Binary);

                    send_diff_event(
                        &mut sender,
                        &DiffEvent::File(FileDiff {
                            index,
                            path,
                            before: before.into_text(),
                            after: after.into_text(),
                            conflicted,
                            binary,
                        }),
                    )
                    .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;

            if let Err(error) = result {
                let message = format!("{error:#}");
                if send_diff_event(
                    &mut sender,
                    &DiffEvent::Error {
                        error: message.clone(),
                    },
                )
                .await
                .is_ok()
                {
                    tracing::warn!(error = %message, "diff stream failed");
                }
            }
        });
    });

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(receiver))?)
}

async fn send_diff_event(
    sender: &mut mpsc::Sender<Result<Bytes, Infallible>>,
    event: &DiffEvent,
) -> Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    sender
        .send(Ok(Bytes::from(line)))
        .await
        .map_err(|_| anyhow!("diff client disconnected"))
}

async fn file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileResponse>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let commit = resolve_visible_commit(loaded.repo.as_ref(), &id).await?;
        let repo_path = RepoPath::from_internal_string(&query.path)?;
        let value = commit.tree().path_value(repo_path).await?;
        if value.is_absent() {
            bail!("path does not exist in revision");
        }
        let conflicted = !value.is_resolved();
        let conflict = if conflicted {
            // The COMMIT's labels, not `unlabeled()`. A commit carries the labels for its own
            // conflict shape (`Commit::tree`), and `materialize_tree_value` simplifies them to
            // match the per-path merge. Without them every side is an anonymous "side #1",
            // which is the difference between "two versions differ" and "the monorepo says
            // this, the downstream says that".
            read_conflict(
                loaded.repo.as_ref(),
                repo_path,
                value.clone(),
                commit.tree().labels().clone(),
            )
            .await?
        } else {
            None
        };
        let contents = read_text_value(loaded.repo.as_ref(), repo_path, value).await?;
        let binary = matches!(contents, FileContents::Binary);

        Ok(FileResponse {
            operation_id: loaded.repo.op_id().hex(),
            change_id: commit.change_id().reverse_hex(),
            commit_id: commit.id().hex(),
            path: query.path,
            contents: contents.into_text(),
            conflicted,
            binary,
            conflict,
        })
    })
    .await?;
    Ok(Json(response))
}

async fn run_jj<T, F, Fut>(operation: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>> + 'static,
{
    tokio::task::spawn_blocking(move || pollster::block_on(operation()))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(AppError)
}

struct LoadedRepository {
    repo: Arc<ReadonlyRepo>,
    workspace_name: String,
}

async fn load_repository(path: &Path) -> Result<LoadedRepository> {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())?;
    let workspace = Workspace::load(
        &settings,
        path,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )?;
    let loader = workspace.repo_loader().clone();
    let workspace_name = workspace.workspace_name().as_str().to_owned();
    drop(workspace);
    let heads = loader.op_heads_store().get_op_heads().await?;
    let [head] = heads.as_slice() else {
        bail!(
            "repository has {} operation heads; refusing to reconcile them in a read-only viewer",
            heads.len()
        );
    };
    let operation = loader.load_operation(head).await?;
    let repo = loader.load_at(&operation).await?;
    Ok(LoadedRepository {
        repo,
        workspace_name,
    })
}

/// A full 40-character hex commit id, or `None`. Deliberately strict: this is the fallback for
/// commits jj cannot see, so it must not guess.
fn hex_to_commit_id(id: &str) -> Option<CommitId> {
    if id.len() != 40 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(20);
    for pair in id.as_bytes().chunks(2) {
        bytes.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
    }
    Some(CommitId::new(bytes))
}

async fn resolve_visible_commit(repo: &ReadonlyRepo, id: &str) -> Result<Commit> {
    if id.len() < 8 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        bail!("revision must be a jj change ID or commit ID prefix of at least 8 characters");
    }

    let mut matches = BTreeMap::new();
    let mut stream = RevsetExpression::all()
        .evaluate(repo)?
        .stream()
        .commits(repo.store());
    while let Some(commit) = stream.try_next().await? {
        if commit.id().hex().starts_with(id) || commit.change_id().reverse_hex().starts_with(id) {
            matches.insert(commit.id().hex(), commit);
        }
    }

    match matches.len() {
        // Nothing visible. Before giving up, try the object store directly for an exact commit
        // id — that is how a SUPERSEDED PATCH SET is reached.
        //
        // Patch sets live at refs/changes/<change-id>/<n>, deliberately outside refs/heads so
        // that importing them cannot make a change id divergent. The cost of that choice is
        // that jj does not consider those commits visible, so the revset walk above never sees
        // them, and reading an old patch set — the entire reason they are pinned — returned
        // "revision does not resolve to a visible jj commit".
        //
        // Safe because it demands the FULL id: the caller must already have the exact commit,
        // which they only get from a patch-set ref we wrote. No prefix matching, so it cannot
        // silently resolve to something the reader did not ask for.
        0 => match hex_to_commit_id(id).and_then(|id| repo.store().get_commit(&id).ok()) {
            Some(commit) => Ok(commit),
            None => Err(anyhow!("revision does not resolve to a visible jj commit")),
        },
        1 => Ok(matches.into_values().next().unwrap()),
        count => Err(anyhow!(
            "revision is ambiguous and resolves to {count} visible jj commits"
        )),
    }
}

fn revision_from_commit(repo: &ReadonlyRepo, commit: &Commit) -> Result<Revision> {
    let divergent = repo
        .resolve_change_id(commit.change_id())?
        .is_some_and(|targets| targets.is_divergent());
    let bookmarks = repo
        .view()
        .local_bookmarks_for_commit(commit.id())
        .map(|(name, _)| name.as_str().to_owned())
        .collect();

    Ok(Revision {
        change_id: commit.change_id().reverse_hex(),
        commit_id: commit.id().hex(),
        parent_commit_ids: commit.parent_ids().iter().map(ObjectId::hex).collect(),
        description: commit.description().to_owned(),
        author_name: commit.author().name.clone(),
        author_email: commit.author().email.clone(),
        authored_at: commit.author().timestamp.to_datetime()?.to_rfc3339(),
        has_conflict: commit.has_conflict(),
        divergent,
        working_copy: repo.view().is_wc_commit_id(commit.id()),
        is_head: repo.view().heads().contains(commit.id()),
        bookmarks,
    })
}

enum FileContents {
    Missing,
    Text(String),
    Binary,
}

impl FileContents {
    fn into_text(self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            Self::Missing | Self::Binary => None,
        }
    }
}

/// Decompose a conflicted path into its bases and sides, plus jj's own materialisation.
///
/// Everything here comes from `materialize_tree_value`, which is the same call jj makes when
/// writing a conflict into a working copy — so what the viewer shows is what `jj` would show,
/// rather than a second implementation of conflict rendering that could drift from it.
///
/// Returns `None` rather than erroring for conflicts that are not file conflicts (a file
/// against a directory, say). Those are real and worth not crashing on, but they have no
/// side-by-side reading, and the caller still reports the path as conflicted.
async fn read_conflict(
    repo: &ReadonlyRepo,
    path: &RepoPath,
    value: MergedTreeValue,
    labels: ConflictLabels,
) -> Result<Option<ConflictView>> {
    let materialized = materialize_tree_value(repo.store(), path, value, &labels).await?;
    let MaterializedTreeValue::FileConflict(conflict) = materialized else {
        return Ok(None);
    };

    // `Merge` stores terms interleaved (side, base, side, ...); `adds()`/`removes()` separate
    // them. Labels are a parallel `Merge<String>` with the same shape, so they zip term for term.
    let labels = conflict.labels.as_merge().clone();
    let term = |contents: &[u8], label: Option<&String>| {
        let bytes = contents;
        let oversized = bytes.len() > MAX_FILE_BYTES;
        let text = if oversized || bytes.contains(&0) {
            None
        } else {
            String::from_utf8(bytes.to_vec()).ok()
        };
        ConflictTerm {
            label: label.filter(|value| !value.is_empty()).cloned(),
            binary: text.is_none() && !oversized,
            // An empty term is how a merge represents "absent on this side" — a delete.
            absent: bytes.is_empty(),
            contents: text,
        }
    };

    let mut label_sides = labels.adds();
    let sides: Vec<_> = conflict
        .contents
        .adds()
        .map(|contents| term(contents.as_ref(), label_sides.next()))
        .collect();
    let mut label_bases = labels.removes();
    let bases: Vec<_> = conflict
        .contents
        .removes()
        .map(|contents| term(contents.as_ref(), label_bases.next()))
        .collect();

    // The marker text jj itself writes. `Snapshot` shows every base and side in full, which is
    // the readable form for someone deciding between them; the `Diff` default is more compact
    // but assumes the reader already knows what changed.
    let settings = UserSettings::from_config(StackedConfig::with_defaults())?;
    let options = ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Snapshot,
        marker_len: None,
        merge: MergeOptions::from_settings(&settings)?,
    };
    let rendered =
        materialize_merge_result_to_bytes(&conflict.contents, &conflict.labels, &options);
    let rendered: &[u8] = rendered.as_ref();
    let materialized = if rendered.len() > MAX_FILE_BYTES || rendered.contains(&0) {
        None
    } else {
        String::from_utf8(rendered.to_vec()).ok()
    };

    Ok(Some(ConflictView {
        bases,
        sides,
        materialized,
    }))
}

/// Text for a tree value, falling back to jj's conflict markers when it is unresolved.
///
/// `read_text_value` reports an unresolved value as missing, which is right for a file view
/// that renders the sides separately and wrong for a diff, where "missing" is indistinguishable
/// from "deleted".
async fn read_text_or_markers(
    repo: &ReadonlyRepo,
    path: &RepoPath,
    value: MergedTreeValue,
    labels: &ConflictLabels,
) -> Result<FileContents> {
    if value.is_resolved() {
        return read_text_value(repo, path, value).await;
    }
    let materialized = materialize_tree_value(repo.store(), path, value, labels).await?;
    let MaterializedTreeValue::FileConflict(conflict) = materialized else {
        // A conflict with no textual form (a file against a directory). Nothing useful to put
        // in a diff, and claiming emptiness would be a lie of the same kind.
        return Ok(FileContents::Binary);
    };
    let options = ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Snapshot,
        marker_len: None,
        merge: MergeOptions::from_settings(&UserSettings::from_config(
            StackedConfig::with_defaults(),
        )?)?,
    };
    let rendered =
        materialize_merge_result_to_bytes(&conflict.contents, &conflict.labels, &options);
    let rendered: &[u8] = rendered.as_ref();
    if rendered.len() > MAX_FILE_BYTES || rendered.contains(&0) {
        return Ok(FileContents::Binary);
    }
    Ok(String::from_utf8(rendered.to_vec())
        .map(FileContents::Text)
        .unwrap_or(FileContents::Binary))
}

/// Patch sets recorded for `change_id`, oldest first.
///
/// Read straight from `refs/changes/<change-id>/<n>` through jj-lib's own git backend, so
/// there is one source of truth and no shelling out. These refs are deliberately outside
/// `refs/heads`, which is why jj does not surface them as bookmarks: importing them would make
/// every version of a change a visible commit sharing one change id, and jj would then refuse
/// to resolve it at all.
fn read_patch_sets(repo: &ReadonlyRepo, change_id: &str) -> Result<Vec<(u32, String)>> {
    let Some(backend) = repo
        .store()
        .backend_impl::<jj_lib::git_backend::GitBackend>()
    else {
        // Not a git-backed repo. Possible in principle, and not worth failing a whole page for.
        return Ok(Vec::new());
    };
    let git = backend.git_repo();
    let prefix = format!("refs/changes/{change_id}/");
    let mut sets = Vec::new();
    for reference in git.references()?.prefixed(prefix.as_bytes())? {
        // gix yields a boxed dyn error the ? operator cannot convert; a bad ref should not
        // take down the page, so name it and move on.
        let reference = reference.map_err(|e| anyhow!("reading refs/changes: {e}"))?;
        let name = String::from_utf8_lossy(reference.name().as_bstr()).into_owned();
        let Some(number) = name.rsplit('/').next().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        sets.push((number, reference.id().to_string()));
    }
    sets.sort_by_key(|(number, _)| *number);
    Ok(sets)
}

async fn read_text_value(
    repo: &ReadonlyRepo,
    path: &RepoPath,
    value: MergedTreeValue,
) -> Result<FileContents> {
    let Ok(value) = value.into_resolved() else {
        return Ok(FileContents::Missing);
    };
    let Some(value) = value else {
        return Ok(FileContents::Missing);
    };

    match value {
        TreeValue::File { id, .. } => {
            let reader = repo.store().read_file(path, &id).await?;
            let mut bytes = Vec::new();
            reader
                .take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
                return Ok(FileContents::Binary);
            }
            match String::from_utf8(bytes) {
                Ok(text) => Ok(FileContents::Text(text)),
                Err(_) => Ok(FileContents::Binary),
            }
        }
        TreeValue::Symlink(id) => Ok(FileContents::Text(
            repo.store().read_symlink(path, &id).await?,
        )),
        TreeValue::Tree(_) | TreeValue::GitSubmodule(_) => Ok(FileContents::Binary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_contents_only_returns_text() {
        assert_eq!(
            FileContents::Text("hello".into()).into_text().as_deref(),
            Some("hello")
        );
        assert!(FileContents::Missing.into_text().is_none());
        assert!(FileContents::Binary.into_text().is_none());
    }

    /// A missing or unreadable synchronization database degrades to an unavailable panel, never
    /// to a failed request.
    ///
    /// cresset-view is a repository viewer first. The worker may not be deployed, may not have
    /// run yet, or may live on another host; none of that should cost anyone the ability to look
    /// at the monorepo.
    #[test]
    fn an_absent_sync_database_is_reported_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let response = read_sync_state(&tmp.path().join("nonexistent.db"));

        assert!(!response.available, "an absent database is unavailable");
        assert!(
            response.unavailable_reason.is_some(),
            "and it says why, rather than looking like an empty fleet"
        );
        assert!(response.projects.is_empty());
    }

    /// The panel reports what an operator needs to act: which projects are blocked and why, which
    /// are actually enabled, and how long since a pass completed.
    #[test]
    fn sync_state_reports_blocked_projects_and_staleness() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE project (
                 id TEXT PRIMARY KEY, config_hash TEXT, downstream_head_sha TEXT,
                 monorepo_commit_id TEXT, status TEXT NOT NULL DEFAULT 'ready',
                 last_error TEXT, enabled INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE operation (
                 id TEXT PRIMARY KEY, project_id TEXT NOT NULL, kind TEXT NOT NULL,
                 state TEXT NOT NULL);
             CREATE TABLE sync_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO project (id, status, enabled, last_error)
                 VALUES ('wick', 'blocked', 1, 'import conflict parked on sync/conflict/wick/op1');
             INSERT INTO project (id, status, enabled) VALUES ('jibs', 'ready', 1);
             INSERT INTO project (id, status, enabled) VALUES ('sconce', 'ready', 0);
             INSERT INTO operation (id, project_id, kind, state)
                 VALUES ('op1', 'wick', 'import', 'applied');
             INSERT INTO operation (id, project_id, kind, state)
                 VALUES ('op0', 'wick', 'export', 'committed');
             INSERT INTO sync_meta (key, value)
                 VALUES ('last_pass_completed_at', CAST(unixepoch() - 900 AS TEXT));",
        )
        .unwrap();
        drop(conn);

        let response = read_sync_state(&path);
        assert!(response.available);

        let age = response.last_pass_age_secs.expect("a pass has completed");
        assert!(
            (890..=910).contains(&age),
            "the age is what distinguishes converged from stalled, got {age}"
        );

        let blocked: Vec<_> = response
            .projects
            .iter()
            .filter(|p| p.status == "blocked")
            .collect();
        assert_eq!(blocked.len(), 1, "one project is blocked");
        assert!(
            blocked[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("sync/conflict/wick/op1")),
            "the conflict bookmark must reach the operator: {:?}",
            blocked[0].last_error
        );

        // `enabled` is an operator decision and `status` is a worker finding; conflating them
        // would hide a project that is simply switched off.
        let disabled: Vec<_> = response.projects.iter().filter(|p| !p.enabled).collect();
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].id, "sconce");
        assert_eq!(
            disabled[0].status, "ready",
            "switched off is not the same as unhealthy"
        );

        // Only unfinished work counts: a committed operation is not outstanding.
        assert_eq!(response.incomplete_operations, 1);
    }

    #[test]
    fn diff_file_event_is_flat_ndjson_record() {
        let event = DiffEvent::File(FileDiff {
            index: 3,
            path: "src/main.rs".into(),
            before: Some("old".into()),
            after: Some("new".into()),
            conflicted: false,
            binary: false,
        });

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({
                "type": "file",
                "index": 3,
                "path": "src/main.rs",
                "before": "old",
                "after": "new",
                "conflicted": false,
                "binary": false,
            })
        );
    }
}
