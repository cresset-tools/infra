use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use futures::channel::mpsc;
use futures::{AsyncReadExt, SinkExt, StreamExt, TryStreamExt};
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::config::StackedConfig;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::MergedTreeValue;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo, StoreFactories};
use jj_lib::repo_path::RepoPath;
use jj_lib::revset::{RevsetExpression, RevsetStreamExt};
use jj_lib::settings::UserSettings;
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

    /// The cresset-sync checkpoint database, read read-only to surface synchronization state.
    ///
    /// Optional on purpose. cresset-view is a repository viewer first; the worker may not be
    /// deployed, may not have run yet, or may be on another host. Its absence removes a panel,
    /// it does not break the service.
    #[arg(long, env = "CRESSET_VIEW_SYNC_DB")]
    sync_db: Option<PathBuf>,

    #[arg(long)]
    check: bool,
}

#[derive(Clone)]
struct AppState {
    repository: Arc<PathBuf>,
    sync_db: Option<Arc<PathBuf>>,
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
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

#[derive(Deserialize)]
struct DiffQuery {
    path: Option<String>,
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
    let state = AppState {
        repository: Arc::new(args.repository),
        sync_db: args.sync_db.map(Arc::new),
    };
    let index = args.assets.join("index.html");
    let static_files = ServeDir::new(&args.assets).not_found_service(ServeFile::new(index));

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/repository", get(repository))
        .route("/api/revisions", get(revisions))
        .route("/api/revisions/{id}", get(revision))
        .route("/api/bookmarks", get(bookmarks))
        .route("/api/sync", get(sync_status))
        .route("/api/revisions/{id}/tree", get(tree))
        .route("/api/revisions/{id}/file", get(file))
        .route("/api/revisions/{id}/diff", get(diff))
        .fallback_service(static_files)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

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
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let commits = RevsetExpression::all()
            .evaluate(loaded.repo.as_ref())?
            .stream()
            .commits(loaded.repo.store())
            .take(limit)
            .try_collect::<Vec<_>>()
            .await?;

        let mut revisions = Vec::with_capacity(commits.len());
        for commit in commits {
            revisions.push(revision_from_commit(loaded.repo.as_ref(), &commit)?);
        }

        Ok(RevisionResponse {
            operation_id: loaded.repo.op_id().hex(),
            head_count: loaded.repo.view().heads().len(),
            revisions,
        })
    })
    .await?;
    Ok(Json(response))
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
    let (loaded, commit, mut entries, paths, start_index) = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let commit = resolve_visible_commit(loaded.repo.as_ref(), &id).await?;
        let before_tree = commit.parent_tree(loaded.repo.as_ref()).await?;
        let after_tree = commit.tree();
        let mut stream = before_tree.diff_stream(&after_tree, &EverythingMatcher);
        let mut entries = Vec::new();
        while let Some(entry) = stream.next().await {
            entries.push(entry);
        }
        let paths: Vec<_> = entries
            .iter()
            .map(|entry| entry.path.as_internal_file_string().to_owned())
            .collect();
        let start_index = match query.path {
            Some(requested_path) => paths
                .iter()
                .position(|path| path == &requested_path)
                .ok_or_else(|| anyhow!("requested path is not changed in this revision"))?,
            None => 0,
        };
        Ok((loaded, commit, entries, paths, start_index))
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
                    let before =
                        read_text_value(loaded.repo.as_ref(), &entry.path, values.before).await?;
                    let after =
                        read_text_value(loaded.repo.as_ref(), &entry.path, values.after).await?;
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
        0 => Err(anyhow!("revision does not resolve to a visible jj commit")),
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
