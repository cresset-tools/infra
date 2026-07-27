use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use futures::{AsyncReadExt, StreamExt, TryStreamExt};
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

    #[arg(long)]
    check: bool,
}

#[derive(Clone)]
struct AppState {
    repository: Arc<PathBuf>,
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
struct DiffResponse {
    operation_id: String,
    change_id: String,
    commit_id: String,
    files: Vec<FileDiff>,
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
    path: String,
    before: Option<String>,
    after: Option<String>,
    conflicted: bool,
    binary: bool,
}

#[derive(Deserialize)]
struct RevisionQuery {
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
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
    };
    let index = args.assets.join("index.html");
    let static_files = ServeDir::new(&args.assets).not_found_service(ServeFile::new(index));

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/repository", get(repository))
        .route("/api/revisions", get(revisions))
        .route("/api/bookmarks", get(bookmarks))
        .route("/api/revisions/{id}/tree", get(tree))
        .route("/api/revisions/{id}/file", get(file))
        .route("/api/revisions/{id}/diff", get(diff))
        .fallback_service(static_files)
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
) -> Result<Json<DiffResponse>, AppError> {
    let path = state.repository.as_ref().clone();
    let response = run_jj(move || async move {
        let loaded = load_repository(&path).await?;
        let commit = resolve_visible_commit(loaded.repo.as_ref(), &id).await?;
        let before_tree = commit.parent_tree(loaded.repo.as_ref()).await?;
        let after_tree = commit.tree();
        let mut stream = before_tree.diff_stream(&after_tree, &EverythingMatcher);
        let mut files = Vec::new();

        while let Some(entry) = stream.next().await {
            let path = entry.path.as_internal_file_string().to_owned();
            let values = entry.values?;
            let conflicted = !values.before.is_resolved() || !values.after.is_resolved();
            let before = read_text_value(loaded.repo.as_ref(), &entry.path, values.before).await?;
            let after = read_text_value(loaded.repo.as_ref(), &entry.path, values.after).await?;
            let binary =
                matches!(before, FileContents::Binary) || matches!(after, FileContents::Binary);

            files.push(FileDiff {
                path,
                before: before.into_text(),
                after: after.into_text(),
                conflicted,
                binary,
            });
        }

        Ok(DiffResponse {
            operation_id: loaded.repo.op_id().hex(),
            change_id: commit.change_id().reverse_hex(),
            commit_id: commit.id().hex(),
            files,
        })
    })
    .await?;
    Ok(Json(response))
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
}
