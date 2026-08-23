use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::env;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_stream::stream;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue,
};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::{any, get, post};
use bytes::BytesMut;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

mod admin;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const LANGUAGE_CODES: &[&str] = &[
    "auto", "en", "zh", "zh-TW", "ja", "ko", "fr", "de", "es", "ru", "it", "pt",
];
const MAX_FIELD_BYTES: usize = 1024;
const MAX_INTERNAL_EVENT_BYTES: usize = 256 * 1024;
const SMALL_API_BODY_BYTES: usize = 64 * 1024;
const MAX_PDF_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GLOSSARY_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const PRESTART_TOMBSTONE_RETENTION: Duration = Duration::from_secs(15 * 60);
const PRESTART_TOMBSTONE_CAPACITY: usize = 4096;
const DEEPSEEK_MODEL_FLASH: &str = "deepseek-v4-flash";
const DEEPSEEK_MODEL_PRO: &str = "deepseek-v4-pro";

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub port: u16,
    pub data_root: PathBuf,
    pub state_root: PathBuf,
    pub frontend_dir: PathBuf,
    pub worker_url: String,
    pub worker_token: String,
    pub max_upload_bytes: u64,
    pub upload_timeout: Duration,
    pub concurrency: usize,
    pub max_active_jobs: usize,
    pub lease_timeout: Duration,
    pub release_grace: Duration,
    pub cancel_reconcile_timeout: Duration,
    pub worker_recycle_timeout: Duration,
    pub terminal_retention: Duration,
    pub id_tombstone_retention: Duration,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, BoxError> {
        let port = env_parse("PORT", 7860_u16)?;
        let data_root =
            PathBuf::from(env::var("PDF2ZH_DATA_ROOT").unwrap_or_else(|_| "/data/jobs".to_owned()));
        let state_root = PathBuf::from(
            env::var("PDF2ZH_STATE_ROOT").unwrap_or_else(|_| "/data/state".to_owned()),
        );
        let frontend_dir = PathBuf::from(
            env::var("PDF2ZH_FRONTEND_DIR").unwrap_or_else(|_| "/app/frontend".to_owned()),
        );
        let worker_url = env::var("PDF2ZH_WORKER_URL")
            .unwrap_or_else(|_| "http://translator:8000".to_owned())
            .trim_end_matches('/')
            .to_owned();
        let worker_token =
            secret_from_env_or_file("PDF2ZH_WORKER_TOKEN", "PDF2ZH_WORKER_TOKEN_FILE")?;
        if worker_token.trim().is_empty() {
            return Err("PDF2ZH_WORKER_TOKEN cannot be blank".into());
        }
        let max_upload_bytes = env_parse("PDF2ZH_MAX_UPLOAD_BYTES", 100_u64 * 1024 * 1024)?;
        let upload_timeout =
            Duration::from_secs(env_parse("PDF2ZH_UPLOAD_TIMEOUT_SECONDS", 900_u64)?);
        let concurrency = env_parse("PDF2ZH_CONCURRENCY", 1_usize)?;
        if concurrency != 1 {
            return Err("PDF2ZH_CONCURRENCY must be exactly one for safe worker recycling".into());
        }
        let max_active_jobs = env_parse("PDF2ZH_MAX_ACTIVE_JOBS", 1_usize)?;
        if max_active_jobs != 1 {
            return Err(
                "PDF2ZH_MAX_ACTIVE_JOBS must be exactly one for per-job worker isolation".into(),
            );
        }
        let lease_timeout = Duration::from_secs(env_parse("PDF2ZH_LEASE_SECONDS", 180_u64)?);
        let release_grace = Duration::from_secs(env_parse("PDF2ZH_RELEASE_GRACE_SECONDS", 15_u64)?);
        let cancel_reconcile_timeout =
            Duration::from_secs(env_parse("PDF2ZH_CANCEL_RECONCILE_SECONDS", 120_u64)?);
        let worker_recycle_timeout =
            Duration::from_secs(env_parse("PDF2ZH_WORKER_RECYCLE_SECONDS", 180_u64)?);
        let terminal_retention =
            Duration::from_secs(env_parse("PDF2ZH_TERMINAL_RETENTION_SECONDS", 21_600_u64)?);
        let id_tombstone_retention =
            Duration::from_secs(env_parse("PDF2ZH_ID_TOMBSTONE_SECONDS", 21_600_u64)?);
        Ok(Self {
            port,
            data_root,
            state_root,
            frontend_dir,
            worker_url,
            worker_token,
            max_upload_bytes,
            upload_timeout,
            concurrency,
            max_active_jobs,
            lease_timeout,
            release_grace,
            cancel_reconcile_timeout,
            worker_recycle_timeout,
            terminal_retention,
            id_tombstone_retention,
        })
    }
}

fn secret_from_env_or_file(value_name: &str, file_name: &str) -> Result<String, BoxError> {
    if let Some(path) = env::var_os(file_name) {
        let path = PathBuf::from(path);
        let value = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {file_name} {}: {error}", path.display()))?;
        return Ok(value.trim_end_matches(&['\r', '\n'][..]).to_owned());
    }
    env::var(value_name).map_err(|_| format!("{value_name} or {file_name} must be set").into())
}

fn env_parse<T>(name: &str, default: T) -> Result<T, BoxError>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => Ok(value.parse::<T>()?),
        Err(_) => Ok(default),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    #[serde(alias = "starting")]
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    fn is_active(&self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct JobFiles {
    pub mono: bool,
    pub dual: bool,
    pub glossary: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum TranslationProvider {
    #[default]
    #[serde(rename = "google")]
    Google,
    #[serde(rename = "deepseek")]
    DeepSeek,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum DeepSeekModel {
    #[serde(rename = "deepseek-v4-flash")]
    V4Flash,
    #[serde(rename = "deepseek-v4-pro")]
    V4Pro,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeepSeekThinkingMode {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeepSeekReasoningEffort {
    Low,
    High,
    Max,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOptions {
    pub source_lang: String,
    pub target_lang: String,
    pub pages: Option<String>,
    pub output_mono: bool,
    pub output_dual: bool,
    #[serde(default)]
    pub provider: TranslationProvider,
    #[serde(default)]
    pub deepseek_model: Option<DeepSeekModel>,
    #[serde(default)]
    pub deepseek_thinking_mode: Option<DeepSeekThinkingMode>,
    #[serde(default)]
    pub deepseek_reasoning_effort: Option<DeepSeekReasoningEffort>,
    #[serde(default = "default_provider_id")]
    pub provider_id: String,
    #[serde(default)]
    pub provider_revision: Option<i64>,
    #[serde(default)]
    pub model_id: Option<String>,
}

fn default_provider_id() -> String {
    "google".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: Uuid,
    pub status: JobStatus,
    pub revision: u64,
    pub progress: f64,
    pub stage: String,
    pub message: String,
    pub error: Option<String>,
    pub files: JobFiles,
    pub original_filename: String,
    pub options: JobOptions,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl JobSnapshot {
    fn queued(id: Uuid, filename: String, options: JobOptions) -> Self {
        Self {
            id,
            status: JobStatus::Queued,
            revision: 1,
            progress: 0.0,
            stage: "等待处理".to_owned(),
            message: "任务已进入队列".to_owned(),
            error: None,
            files: JobFiles::default(),
            original_filename: filename,
            options,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }
}

struct JobEntry {
    snapshot: RwLock<JobSnapshot>,
    sender: broadcast::Sender<JobSnapshot>,
    cancellation: CancellationToken,
    publish_lock: Mutex<()>,
    disposed: AtomicBool,
    admission_permit: StdMutex<Option<OwnedSemaphorePermit>>,
    lease: Mutex<JobLease>,
    job_dir: PathBuf,
    runtime_provider: StdMutex<Option<admin::RuntimeProvider>>,
}

struct JobLease {
    last_heartbeat: Instant,
    released_at: Option<Instant>,
}

struct PendingUploadState {
    cancellation: CancellationToken,
}

struct PendingUploadGuard {
    state: AppState,
    id: Uuid,
    pending: Arc<PendingUploadState>,
    committed: bool,
    created_dir: bool,
    admission_permit: Option<OwnedSemaphorePermit>,
}

impl PendingUploadGuard {
    fn commit(&mut self) {
        self.committed = true;
        let mut pending_uploads = self
            .state
            .0
            .pending_uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending_uploads
            .get(&self.id)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            pending_uploads.remove(&self.id);
        }
    }

    fn take_admission_permit(&mut self) -> OwnedSemaphorePermit {
        self.admission_permit
            .take()
            .expect("pending upload admission permit was already transferred")
    }

    fn mark_created_dir(&mut self) {
        self.created_dir = true;
    }
}

impl Drop for PendingUploadGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if !self.created_dir {
            let mut pending_uploads = self
                .state
                .0
                .pending_uploads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending_uploads
                .get(&self.id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.pending))
            {
                pending_uploads.remove(&self.id);
            }
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = self.state.clone();
            let id = self.id;
            let pending = self.pending.clone();
            runtime.spawn(async move {
                safe_remove_job_dir(&state, id).await;
                let mut pending_uploads = state
                    .0
                    .pending_uploads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if pending_uploads
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(current, &pending))
                {
                    pending_uploads.remove(&id);
                }
            });
        }
    }
}

impl JobEntry {
    #[cfg(test)]
    fn new(snapshot: JobSnapshot, job_dir: PathBuf) -> Arc<Self> {
        Self::new_with_runtime(snapshot, job_dir, None, None)
    }

    fn new_with_runtime(
        snapshot: JobSnapshot,
        job_dir: PathBuf,
        admission_permit: Option<OwnedSemaphorePermit>,
        runtime_provider: Option<admin::RuntimeProvider>,
    ) -> Arc<Self> {
        let (sender, _) = broadcast::channel(64);
        Arc::new(Self {
            snapshot: RwLock::new(snapshot),
            sender,
            cancellation: CancellationToken::new(),
            publish_lock: Mutex::new(()),
            disposed: AtomicBool::new(false),
            admission_permit: StdMutex::new(admission_permit),
            lease: Mutex::new(JobLease {
                last_heartbeat: Instant::now(),
                released_at: None,
            }),
            job_dir,
            runtime_provider: StdMutex::new(runtime_provider),
        })
    }
}

#[derive(Clone)]
struct Persistence {
    pool: SqlitePool,
}

struct PersistedJobs {
    snapshots: Vec<JobSnapshot>,
    corrupt_ids: HashSet<Uuid>,
}

fn parse_canonical_uuid(value: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(value).ok()?;
    (id.to_string() == value).then_some(id)
}

impl Persistence {
    async fn open(data_root: &Path) -> Result<Self, BoxError> {
        fs::create_dir_all(data_root).await?;
        let database_path = data_root.join("jobs.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(database_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .pragma("secure_delete", "ON")
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS jobs (\
                id TEXT PRIMARY KEY NOT NULL, \
                snapshot_json TEXT NOT NULL, \
                updated_at TEXT NOT NULL\
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS job_tombstones (\
                id TEXT PRIMARY KEY NOT NULL, \
                expires_at INTEGER NOT NULL\
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS prestart_tombstones (\
                id TEXT PRIMARY KEY NOT NULL, \
                expires_at INTEGER NOT NULL\
            )",
        )
        .execute(&pool)
        .await?;
        let persistence = Self { pool };
        persistence.purge_expired_tombstones().await?;
        Ok(persistence)
    }

    async fn save(&self, snapshot: &JobSnapshot) -> Result<(), BoxError> {
        let id = snapshot.id.to_string();
        let json = serde_json::to_string(snapshot)?;
        sqlx::query(
            "INSERT INTO jobs (id, snapshot_json, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET snapshot_json=excluded.snapshot_json, \
             updated_at=excluded.updated_at",
        )
        .bind(id)
        .bind(json)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load(&self) -> Result<PersistedJobs, BoxError> {
        let rows = sqlx::query("SELECT rowid AS storage_rowid, id, snapshot_json FROM jobs")
            .fetch_all(&self.pool)
            .await?;
        let mut snapshots = Vec::with_capacity(rows.len());
        let mut corrupt_ids = HashSet::new();
        for row in rows {
            let storage_rowid = match row.try_get::<i64, _>("storage_rowid") {
                Ok(value) => value,
                Err(error) => {
                    warn!(%error, "skipping a persisted job row with an unreadable SQLite rowid");
                    continue;
                }
            };
            let row_id = match row.try_get::<String, _>("id") {
                Ok(value) => value,
                Err(error) => {
                    warn!(%storage_rowid, %error, "discarding a job row with an invalid id field");
                    self.delete_corrupt_rowid(storage_rowid).await;
                    continue;
                }
            };
            let json = match row.try_get::<String, _>("snapshot_json") {
                Ok(value) => value,
                Err(error) => {
                    warn!(%storage_rowid, id = %row_id, %error, "discarding a job row with an invalid snapshot field");
                    if let Some(id) = parse_canonical_uuid(&row_id) {
                        corrupt_ids.insert(id);
                    }
                    self.delete_corrupt_rowid(storage_rowid).await;
                    continue;
                }
            };
            match serde_json::from_str::<JobSnapshot>(&json) {
                Ok(mut snapshot) if snapshot.id.to_string() == row_id => {
                    if snapshot.options.provider_id == "google"
                        && snapshot.options.provider == TranslationProvider::DeepSeek
                    {
                        snapshot.options.provider_id = "deepseek".to_owned();
                    }
                    if snapshot.options.model_id.is_none() {
                        snapshot.options.model_id = snapshot.options.deepseek_model.map(|model| {
                            match model {
                                DeepSeekModel::V4Flash => DEEPSEEK_MODEL_FLASH,
                                DeepSeekModel::V4Pro => DEEPSEEK_MODEL_PRO,
                            }
                            .to_owned()
                        });
                    }
                    snapshots.push(snapshot);
                }
                Ok(snapshot) => {
                    warn!(
                        database_id = %row_id,
                        snapshot_id = %snapshot.id,
                        "discarding a job record whose stored id does not match its snapshot"
                    );
                    // The SQLite primary key is the only trusted identity for
                    // a corrupt row. Never let an embedded snapshot id target
                    // another, otherwise-valid job and its output directory.
                    if let Some(id) = parse_canonical_uuid(&row_id) {
                        corrupt_ids.insert(id);
                    }
                    self.delete_corrupt_row(&row_id).await;
                }
                Err(error) => {
                    warn!(id = %row_id, %error, "discarding a corrupt persisted job snapshot");
                    if let Some(id) = parse_canonical_uuid(&row_id) {
                        corrupt_ids.insert(id);
                    }
                    self.delete_corrupt_row(&row_id).await;
                }
            }
        }
        Ok(PersistedJobs {
            snapshots,
            corrupt_ids,
        })
    }

    async fn delete_corrupt_row(&self, id: &str) {
        if let Err(error) = sqlx::query("DELETE FROM jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
        {
            warn!(%id, %error, "failed to quarantine a corrupt job record");
        }
    }

    async fn delete_corrupt_rowid(&self, storage_rowid: i64) {
        if let Err(error) = sqlx::query("DELETE FROM jobs WHERE rowid = ?")
            .bind(storage_rowid)
            .execute(&self.pool)
            .await
        {
            warn!(%storage_rowid, %error, "failed to quarantine a corrupt SQLite row");
        }
    }

    async fn retire(&self, id: Uuid, retention: Duration) -> Result<(), BoxError> {
        let retention_seconds = i64::try_from(retention.as_secs()).unwrap_or(i64::MAX);
        let expires_at = Utc::now().timestamp().saturating_add(retention_seconds);
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO job_tombstones (id, expires_at) VALUES (?, ?) \
             ON CONFLICT(id) DO UPDATE SET expires_at = MAX(expires_at, excluded.expires_at)",
        )
        .bind(id.to_string())
        .bind(expires_at)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn tombstone_prestart(
        &self,
        id: Uuid,
        retention: Duration,
        capacity: usize,
    ) -> Result<(), BoxError> {
        let retention_seconds = i64::try_from(retention.as_secs()).unwrap_or(i64::MAX);
        let now = Utc::now().timestamp();
        let expires_at = now.saturating_add(retention_seconds);
        let capacity = i64::try_from(capacity.max(1)).unwrap_or(i64::MAX);
        let id = id.to_string();
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM prestart_tombstones WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO prestart_tombstones (id, expires_at) VALUES (?, ?) \
             ON CONFLICT(id) DO UPDATE SET expires_at = MAX(expires_at, excluded.expires_at)",
        )
        .bind(&id)
        .bind(expires_at)
        .execute(&mut *transaction)
        .await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM prestart_tombstones")
            .fetch_one(&mut *transaction)
            .await?;
        let excess = count.saturating_sub(capacity);
        if excess > 0 {
            sqlx::query(
                "DELETE FROM prestart_tombstones WHERE id IN (\
                    SELECT id FROM prestart_tombstones WHERE id <> ? \
                    ORDER BY expires_at ASC, id ASC LIMIT ?\
                )",
            )
            .bind(&id)
            .bind(excess)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn is_tombstoned(&self, id: Uuid) -> Result<bool, BoxError> {
        let now = Utc::now().timestamp();
        let exists: i64 = sqlx::query_scalar(
            "SELECT (\
                EXISTS(SELECT 1 FROM job_tombstones WHERE id = ? AND expires_at > ?) \
                OR EXISTS(SELECT 1 FROM prestart_tombstones WHERE id = ? AND expires_at > ?)\
            )",
        )
        .bind(id.to_string())
        .bind(now)
        .bind(id.to_string())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(exists > 0)
    }

    async fn purge_expired_tombstones(&self) -> Result<(), BoxError> {
        let now = Utc::now().timestamp();
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM job_tombstones WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM prestart_tombstones WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "DELETE FROM prestart_tombstones WHERE id NOT IN (\
                SELECT id FROM prestart_tombstones \
                ORDER BY expires_at DESC, id DESC LIMIT ?\
            )",
        )
        .bind(i64::try_from(PRESTART_TOMBSTONE_CAPACITY).unwrap_or(i64::MAX))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

struct AppStateInner {
    config: AppConfig,
    jobs: RwLock<HashMap<Uuid, Arc<JobEntry>>>,
    permits: Arc<Semaphore>,
    admission: Arc<Semaphore>,
    tasks: TaskTracker,
    shutdown: CancellationToken,
    runner_gate: Mutex<()>,
    pending_uploads: StdMutex<HashMap<Uuid, Arc<PendingUploadState>>>,
    worker_identity: RwLock<Option<WorkerIdentity>>,
    client: Client,
    persistence: Persistence,
    admin: admin::AdminService,
}

#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl AppState {
    pub async fn new(config: AppConfig) -> Result<Self, BoxError> {
        if config.concurrency != 1 || config.max_active_jobs != 1 {
            return Err("job concurrency and active admission must both be exactly one".into());
        }
        fs::create_dir_all(&config.data_root).await?;
        let artifact_root = config.state_root.join("artifacts");
        let upload_root = config.state_root.join("uploads");
        fs::create_dir_all(&artifact_root).await?;
        fs::create_dir_all(&upload_root).await?;
        #[cfg(unix)]
        fs::set_permissions(&artifact_root, std::fs::Permissions::from_mode(0o700)).await?;
        #[cfg(unix)]
        fs::set_permissions(&upload_root, std::fs::Permissions::from_mode(0o700)).await?;
        let persistence = Persistence::open(&config.state_root).await?;
        let admin = admin::AdminService::new(persistence.pool.clone()).await?;
        let client = build_worker_client()?;
        let state = Self(Arc::new(AppStateInner {
            permits: Arc::new(Semaphore::new(config.concurrency)),
            admission: Arc::new(Semaphore::new(config.max_active_jobs)),
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            runner_gate: Mutex::new(()),
            pending_uploads: StdMutex::new(HashMap::new()),
            worker_identity: RwLock::new(None),
            jobs: RwLock::new(HashMap::new()),
            client,
            persistence,
            admin,
            config,
        }));
        state.wait_for_initial_worker_epoch().await;
        state.restore_jobs().await?;
        Ok(state)
    }

    async fn wait_for_initial_worker_epoch(&self) {
        let started_at = Instant::now();
        loop {
            if let Some(health) = fetch_worker_health(self).await
                && health.is_ready(self.0.config.concurrency)
            {
                let identity = health.identity();
                *self.0.worker_identity.write().await = Some(identity.clone());
                info!(epoch = %identity.epoch, container_boot_id = %identity.container_boot_id, "connected to Python worker epoch");
                return;
            }
            if started_at.elapsed() > self.0.config.worker_recycle_timeout {
                fail_stop_coordinator(
                    "Python worker did not expose a valid epoch before the startup deadline",
                );
            }
            warn!("waiting for a valid Python worker epoch before startup");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn restore_jobs(&self) -> Result<(), BoxError> {
        let persisted = self.0.persistence.load().await?;
        let mut interrupted = Vec::new();
        let mut succeeded = Vec::new();
        {
            let mut jobs = self.0.jobs.write().await;
            for snapshot in persisted.snapshots {
                let id = snapshot.id;
                let was_cancelling = snapshot.status == JobStatus::Cancelling;
                let was_succeeded = snapshot.status == JobStatus::Succeeded;
                let active = snapshot.status.is_active();
                let job_dir = self.0.config.data_root.join(snapshot.id.to_string());
                let admission_permit = if active {
                    self.0.admission.clone().try_acquire_owned().ok()
                } else {
                    None
                };
                if active && admission_permit.is_none() {
                    warn!(%id, "restored active jobs exceeded the configured admission limit");
                }
                // Active jobs are always reconciled to a terminal cancellation
                // after a Rust process restart. Provider credentials therefore
                // remain memory-only and are deliberately never rehydrated from
                // a historical database revision.
                let runtime_provider = None;
                let entry = JobEntry::new_with_runtime(
                    snapshot,
                    job_dir,
                    admission_permit,
                    runtime_provider,
                );
                if active {
                    interrupted.push((entry.clone(), was_cancelling));
                }
                if was_succeeded {
                    succeeded.push(entry.clone());
                }
                jobs.insert(id, entry);
            }
        }
        for entry in succeeded {
            let snapshot = entry.snapshot.read().await.clone();
            let id = snapshot.id;
            if published_artifacts_are_valid(self, id, &snapshot.files).await {
                if let Err(error) = remove_worker_job_files(self, id).await {
                    warn!(%id, %error, "failed to clear a restored succeeded worker directory");
                }
                continue;
            }
            match confirm_materialized_outputs(self, &entry, id).await {
                Ok(files) => {
                    update_job(self, &entry, |snapshot| snapshot.files = files).await;
                }
                Err(error) => {
                    warn!(%id, %error, "restored succeeded job had no safe published artifacts");
                    update_job(self, &entry, |snapshot| {
                        snapshot.status = JobStatus::Failed;
                        snapshot.stage = "翻译失败".to_owned();
                        snapshot.message = "任务未完成，可以重新提交".to_owned();
                        snapshot.error = Some("翻译服务暂时无法完成任务，请稍后重试".to_owned());
                        snapshot.files = JobFiles::default();
                        snapshot.finished_at = Some(Utc::now());
                    })
                    .await;
                    safe_remove_job_dir(self, id).await;
                }
            }
        }
        for (entry, was_cancelling) in interrupted {
            let id = entry.snapshot.read().await.id;
            warn!(%id, "reconciling a job left active by the previous Rust process");
            request_cancellation(self, &entry, "服务重启，正在核对旧翻译进程").await;
            let terminal = cancel_worker_until_terminal(self, id).await;
            finalize_confirmed_terminal(
                self,
                &entry,
                id,
                terminal,
                "服务重启期间任务被安全中断",
                was_cancelling,
            )
            .await;
            cleanup_failed_job(self, &entry, id).await;
            let _ = force_recycle_worker(self, id).await;
            release_job_admission(&entry);
        }
        for id in persisted.corrupt_ids {
            if self.0.jobs.read().await.contains_key(&id) {
                error!(%id, "refusing to clean a corrupt row id that is also a valid restored job");
                continue;
            }
            warn!(%id, "cancelling the worker id recovered from a corrupt state row");
            let _ = cancel_worker_until_terminal(self, id).await;
            safe_remove_job_dir(self, id).await;
            let _ = force_recycle_worker(self, id).await;
        }
        // A web restart cannot prove that a process-group escape did not
        // survive in the reusable worker container. Always begin admission on
        // a newly observed epoch; the recycle also drains the replacement and
        // scrubs every shared-volume entry, including non-UUID names/symlinks.
        let _ = force_recycle_worker(self, Uuid::nil()).await;
        cleanup_orphan_job_dirs(self).await;
        cleanup_orphan_artifact_dirs(self).await;
        cleanup_orphan_private_uploads(self).await;
        Ok(())
    }

    pub fn start_maintenance(&self) {
        let state = self.clone();
        self.0
            .tasks
            .spawn(async move { state.maintenance_loop().await });
    }

    pub async fn shutdown(&self) {
        {
            let _runner_guard = self.0.runner_gate.lock().await;
            self.0.shutdown.cancel();
            self.0.permits.close();
            self.0.admission.close();
            self.0.tasks.close();
        }

        let pending_uploads = self
            .0
            .pending_uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for pending in pending_uploads {
            pending.cancellation.cancel();
        }

        let entries = self
            .0
            .jobs
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();

        for entry in &entries {
            request_cancellation(self, entry, "服务正在关闭，正在安全回收任务").await;
        }
        self.0.tasks.wait().await;
    }
}

fn build_worker_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        // Internal worker requests may contain provider credentials. Never
        // inherit HTTP(S)_PROXY and never follow a redirect to another
        // authority with that JSON body.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("任务不存在")]
    NotFound,
    #[error("使用付费翻译渠道需要管理员登录")]
    Unauthorized,
    #[error("管理员会话或 CSRF 校验失败")]
    Forbidden,
    #[error("任务编号已释放，请使用新的任务编号")]
    Gone,
    #[error("翻译服务暂时不可用")]
    Unavailable,
    #[error("{0}")]
    Conflict(String),
    #[error("任务队列已满，请等待当前任务结束")]
    Busy,
    #[error("上传超时，请重新提交 PDF")]
    RequestTimeout,
    #[error("服务器内部错误")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> AxumResponse {
        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Gone => StatusCode::GONE,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut response = (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    provider: &'static str,
    worker: &'static str,
    active_jobs: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkerIdentity {
    epoch: String,
    container_boot_id: String,
}

#[derive(Deserialize)]
struct WorkerHealth {
    status: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    providers: Vec<String>,
    active_jobs: usize,
    accepting_jobs: bool,
    epoch: String,
    container_boot_id: String,
    max_active_jobs: usize,
}

impl WorkerHealth {
    fn identity(&self) -> WorkerIdentity {
        WorkerIdentity {
            epoch: self.epoch.clone(),
            container_boot_id: self.container_boot_id.clone(),
        }
    }

    fn supports_provider(&self, provider: &str) -> bool {
        self.provider.as_deref() == Some(provider)
            || self.providers.iter().any(|candidate| candidate == provider)
    }

    fn is_ready(&self, concurrency: usize) -> bool {
        self.status == "ok"
            && self.supports_provider("google")
            && self.accepting_jobs
            && self.active_jobs <= concurrency
            && self.max_active_jobs == 1
            && !self.epoch.is_empty()
            && !self.container_boot_id.is_empty()
    }

    fn supports_translation_provider(&self, provider: TranslationProvider) -> bool {
        match provider {
            TranslationProvider::Google => true,
            TranslationProvider::DeepSeek => self.supports_provider("deepseek"),
            TranslationProvider::OpenAiCompatible => self.supports_provider("openai_compatible"),
        }
    }

    fn is_replacement_for(&self, previous: &WorkerIdentity) -> bool {
        self.epoch != previous.epoch && self.container_boot_id != previous.container_boot_id
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicConfig {
    provider: &'static str,
    providers: Vec<ProviderDescriptor>,
    languages: Vec<Language>,
    max_upload_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf_token: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderDescriptor {
    id: String,
    kind: String,
    name: String,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    models: Vec<ProviderModelDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    thinking_modes: Vec<String>,
    reasoning_efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_reasoning_effort: Option<String>,
}

#[derive(Serialize)]
struct ProviderModelDescriptor {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct Language {
    code: &'static str,
    name: &'static str,
}

pub fn build_router(state: AppState) -> Router {
    let body_limit = usize::try_from(state.0.config.max_upload_bytes)
        .unwrap_or(usize::MAX.saturating_sub(1024 * 1024))
        .saturating_add(1024 * 1024);
    let index_path = state.0.config.frontend_dir.join("index.html");
    let static_files =
        ServeDir::new(&state.0.config.frontend_dir).fallback(ServeFile::new(index_path));

    // Keep the exceptional multipart allowance scoped to the one upload
    // endpoint. In particular, an outer DefaultBodyLimit would overwrite the
    // admin router's JSON limit and let authentication/configuration requests
    // consume the full PDF allowance.
    let upload_routes = Router::new()
        .route("/api/jobs", post(create_job))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(RequestBodyLimitLayer::new(body_limit));
    let bounded_api_routes = Router::new()
        .nest("/api/admin", admin::router())
        .route("/api/health", get(health))
        .route("/api/config", get(public_config))
        .route("/api/jobs/{id}", get(get_job).delete(dispose_job))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/jobs/{id}/heartbeat", post(heartbeat_job))
        .route("/api/jobs/{id}/release", post(release_job))
        .route("/api/jobs/{id}/files/{kind}", get(download_file))
        .route("/api", any(api_not_found))
        .route("/api/{*path}", any(api_not_found))
        .layer(DefaultBodyLimit::max(SMALL_API_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(SMALL_API_BODY_BYTES));

    Router::new()
        .merge(upload_routes)
        .merge(bounded_api_routes)
        .fallback_service(static_files)
        .layer(CompressionLayer::new())
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn api_not_found() -> ApiError {
    ApiError::NotFound
}

async fn health(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    let active_jobs = state
        .0
        .jobs
        .read()
        .await
        .values()
        .filter(|entry| {
            entry
                .snapshot
                .try_read()
                .is_ok_and(|snapshot| snapshot.status.is_active())
        })
        .count();
    if !worker_is_ready(&state).await {
        return Err(ApiError::Unavailable);
    }
    Ok(Json(HealthResponse {
        status: "ok",
        provider: "google",
        worker: "ready",
        active_jobs,
    }))
}

async fn worker_is_ready(state: &AppState) -> bool {
    worker_is_ready_for_provider(state, TranslationProvider::Google).await
}

async fn worker_is_ready_for_provider(state: &AppState, provider: TranslationProvider) -> bool {
    let Some(health) = fetch_worker_health(state).await else {
        return false;
    };
    let ready = health.is_ready(state.0.config.concurrency)
        && health.supports_translation_provider(provider);
    if ready {
        *state.0.worker_identity.write().await = Some(health.identity());
    }
    ready
}

async fn ready_worker_identity_for_provider(
    state: &AppState,
    provider: TranslationProvider,
) -> Option<WorkerIdentity> {
    let health = fetch_worker_health(state).await?;
    if !health.is_ready(state.0.config.concurrency)
        || !health.supports_translation_provider(provider)
    {
        return None;
    }
    let identity = health.identity();
    *state.0.worker_identity.write().await = Some(identity.clone());
    Some(identity)
}

async fn fetch_worker_health(state: &AppState) -> Option<WorkerHealth> {
    let response = state
        .0
        .client
        .get(format!("{}/internal/health", state.0.config.worker_url))
        .header("x-worker-token", &state.0.config.worker_token)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<WorkerHealth>().await.ok()
}

async fn public_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<PublicConfig>), ApiError> {
    let worker_health = fetch_worker_health(&state).await;
    let admin_session = state.0.admin.config_session(&headers).await;
    let configured = if admin_session.is_some() {
        state
            .0
            .admin
            .public_providers()
            .await
            .map_err(internal_error)?
    } else {
        Vec::new()
    };
    let mut response_headers = HeaderMap::new();
    response_headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((
        response_headers,
        Json(PublicConfig {
            provider: "google",
            providers: public_provider_descriptors(worker_health.as_ref(), configured),
            max_upload_bytes: state.0.config.max_upload_bytes,
            csrf_token: admin_session.and_then(|session| session.csrf_token),
            languages: vec![
                Language {
                    code: "auto",
                    name: "自动检测",
                },
                Language {
                    code: "en",
                    name: "English",
                },
                Language {
                    code: "zh",
                    name: "简体中文",
                },
                Language {
                    code: "zh-TW",
                    name: "繁體中文",
                },
                Language {
                    code: "ja",
                    name: "日本語",
                },
                Language {
                    code: "ko",
                    name: "한국어",
                },
                Language {
                    code: "fr",
                    name: "Français",
                },
                Language {
                    code: "de",
                    name: "Deutsch",
                },
                Language {
                    code: "es",
                    name: "Español",
                },
                Language {
                    code: "ru",
                    name: "Русский",
                },
                Language {
                    code: "it",
                    name: "Italiano",
                },
                Language {
                    code: "pt",
                    name: "Português",
                },
            ],
        }),
    ))
}

fn public_provider_descriptors(
    health: Option<&WorkerHealth>,
    configured: Vec<admin::PublicProviderConfig>,
) -> Vec<ProviderDescriptor> {
    // Provider availability describes configuration/capability, not a
    // transient drain or a currently occupied worker slot. Admission and
    // readiness are checked again when the job is created.
    let google_available =
        health.is_some_and(|health| health.status == "ok" && health.supports_provider("google"));
    let worker_available = health.is_some_and(|health| health.status == "ok");
    let mut providers = vec![ProviderDescriptor {
        id: "google".to_owned(),
        kind: "google".to_owned(),
        name: "Google".to_owned(),
        available: google_available,
        unavailable_reason: (!google_available).then(|| "worker_unavailable".to_owned()),
        models: Vec::new(),
        default_model: None,
        thinking_modes: Vec::new(),
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
    }];
    providers.extend(configured.into_iter().map(|provider| {
        let worker_capable = health.is_some_and(|health| {
            health.status == "ok"
                && health.supports_provider(if provider.kind == "deepseek" {
                    "deepseek"
                } else {
                    "openai_compatible"
                })
        });
        let available = provider.available && worker_capable;
        ProviderDescriptor {
            id: provider.id,
            kind: provider.kind,
            name: provider.name,
            available,
            unavailable_reason: (!available).then(|| {
                if !worker_available || !worker_capable {
                    "worker_unavailable".to_owned()
                } else {
                    provider
                        .unavailable_reason
                        .unwrap_or_else(|| "not_configured".to_owned())
                }
            }),
            models: provider
                .models
                .into_iter()
                .map(|model| ProviderModelDescriptor {
                    id: model.id,
                    name: model.name,
                })
                .collect(),
            default_model: provider.default_model,
            thinking_modes: provider.thinking_modes,
            reasoning_efforts: provider.reasoning_efforts,
            default_reasoning_effort: provider.default_reasoning_effort,
        }
    }));
    providers
}

#[derive(Default)]
struct PendingUpload {
    filename: Option<String>,
    source_lang: Option<String>,
    target_lang: Option<String>,
    pages: Option<String>,
    output_mono: Option<bool>,
    output_dual: Option<bool>,
    provider: Option<String>,
    deepseek_model: Option<String>,
    deepseek_thinking_mode: Option<String>,
    deepseek_reasoning_effort: Option<String>,
    provider_id: Option<String>,
    model_id: Option<String>,
    thinking_mode: Option<String>,
    reasoning_effort: Option<String>,
    saw_file: bool,
}

async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<JobSnapshot>), ApiError> {
    let requested_id = parse_requested_job_id(&headers)?;
    let id = requested_id.unwrap_or_else(Uuid::new_v4);
    let existing = state.0.jobs.read().await.get(&id).cloned();
    if let Some(entry) = existing {
        return Ok((StatusCode::OK, Json(entry.snapshot.read().await.clone())));
    }
    match state.0.persistence.is_tombstoned(id).await {
        Ok(true) => return Err(ApiError::Gone),
        Ok(false) => {}
        Err(error) => {
            error!(%id, %error, "failed to check the retired job id set");
            return Err(ApiError::Internal);
        }
    }
    if state.0.shutdown.is_cancelled() || !worker_is_ready(&state).await {
        return Err(ApiError::Unavailable);
    }
    let admission_permit = state
        .0
        .admission
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::Busy)?;
    let upload_state = Arc::new(PendingUploadState {
        cancellation: CancellationToken::new(),
    });
    // The same gate is used by unknown cancel/release requests. It makes the
    // pending reservation visible before either side can decide that this id
    // has never existed, while the durable tombstone check below prevents a
    // release that arrived first from being overtaken by this upload.
    let reservation_guard = state.0.runner_gate.lock().await;
    {
        let mut pending = state
            .0
            .pending_uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.contains_key(&id) {
            return Err(ApiError::Conflict(
                "同一任务正在上传，请稍后恢复".to_owned(),
            ));
        }
        pending.insert(id, upload_state.clone());
    }
    let mut pending_guard = PendingUploadGuard {
        state: state.clone(),
        id,
        pending: upload_state.clone(),
        committed: false,
        created_dir: false,
        admission_permit: Some(admission_permit),
    };
    // A permit may have been acquired just before shutdown closed admission.
    // The reservation guard makes this check linear with shutdown and leaves
    // a cancellable reservation behind until PendingUploadGuard is dropped.
    if state.0.shutdown.is_cancelled() {
        upload_state.cancellation.cancel();
        return Err(ApiError::Unavailable);
    }
    let existing = state.0.jobs.read().await.get(&id).cloned();
    if let Some(entry) = existing {
        pending_guard.commit();
        return Ok((StatusCode::OK, Json(entry.snapshot.read().await.clone())));
    }
    match state.0.persistence.is_tombstoned(id).await {
        Ok(true) => {
            upload_state.cancellation.cancel();
            return Err(ApiError::Gone);
        }
        Ok(false) => {}
        Err(error) => {
            error!(%id, %error, "failed to recheck the retired job id after reserving its upload");
            upload_state.cancellation.cancel();
            return Err(ApiError::Internal);
        }
    }
    drop(reservation_guard);
    let private_upload_dir = state
        .0
        .config
        .state_root
        .join("uploads")
        .join(id.to_string());
    if let Err(error) = fs::create_dir(&private_upload_dir).await {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            let existing = state.0.jobs.read().await.get(&id).cloned();
            if let Some(entry) = existing {
                pending_guard.commit();
                return Ok((StatusCode::OK, Json(entry.snapshot.read().await.clone())));
            }
            return Err(ApiError::Conflict("任务编号已被占用".to_owned()));
        }
        return Err(internal_error(error));
    }
    pending_guard.mark_created_dir();
    let upload_result = tokio::time::timeout(
        state.0.config.upload_timeout,
        receive_upload(&state, id, &mut multipart, &upload_state.cancellation),
    )
    .await
    .map_err(|_| ApiError::RequestTimeout)
    .and_then(|result| result);
    let pending = match upload_result {
        Ok(upload) => upload,
        Err(error) => {
            safe_remove_job_dir(&state, id).await;
            return Err(error);
        }
    };

    let mut options = match validate_upload_options(&pending) {
        Ok(options) => options,
        Err(error) => {
            safe_remove_job_dir(&state, id).await;
            return Err(error);
        }
    };
    if options.provider != TranslationProvider::Google {
        state
            .0
            .admin
            .authorize_paid_job(&headers)
            .await
            .map_err(|error| match error {
                admin::PaidJobAuthorizationError::Unauthorized => ApiError::Unauthorized,
                admin::PaidJobAuthorizationError::Forbidden => ApiError::Forbidden,
                admin::PaidJobAuthorizationError::Internal => ApiError::Internal,
            })?;
    }
    let runtime_provider = if options.provider == TranslationProvider::Google {
        None
    } else {
        let model_id = options
            .model_id
            .as_deref()
            .ok_or_else(|| ApiError::BadRequest("必须指定模型".to_owned()))?;
        let runtime = match state
            .0
            .admin
            .resolve_runtime(id, &options.provider_id, model_id)
            .await
        {
            Ok(runtime) => runtime,
            Err(message) => {
                safe_remove_job_dir(&state, id).await;
                return Err(ApiError::BadRequest(message));
            }
        };
        options.provider = match runtime.kind.as_str() {
            "deepseek" => TranslationProvider::DeepSeek,
            "openai_compatible" => TranslationProvider::OpenAiCompatible,
            _ => {
                safe_remove_job_dir(&state, id).await;
                return Err(ApiError::Internal);
            }
        };
        options.provider_revision = Some(runtime.revision);
        Some(runtime)
    };
    if !worker_is_ready_for_provider(&state, options.provider).await {
        safe_remove_job_dir(&state, id).await;
        return Err(ApiError::Unavailable);
    }
    if let Err(error) = publish_private_upload_to_worker(&state, id).await {
        safe_remove_job_dir(&state, id).await;
        return Err(error);
    }
    if let Err(error) = remove_private_upload(&state, id).await {
        error!(%id, %error, "failed to erase the private upload after worker publication");
        safe_remove_job_dir(&state, id).await;
        return Err(ApiError::Internal);
    }
    let filename = pending
        .filename
        .unwrap_or_else(|| "document.pdf".to_owned());
    let snapshot = JobSnapshot::queued(id, filename, options);
    let job_dir = state.0.config.data_root.join(id.to_string());
    let entry = JobEntry::new_with_runtime(
        snapshot.clone(),
        job_dir,
        Some(pending_guard.take_admission_permit()),
        runtime_provider,
    );
    let runner_guard = state.0.runner_gate.lock().await;
    if state.0.shutdown.is_cancelled() || upload_state.cancellation.is_cancelled() {
        drop(runner_guard);
        safe_remove_job_dir(&state, id).await;
        return Err(ApiError::Unavailable);
    }
    match state.0.persistence.is_tombstoned(id).await {
        Ok(true) => {
            drop(runner_guard);
            safe_remove_job_dir(&state, id).await;
            return Err(ApiError::Gone);
        }
        Ok(false) => {}
        Err(error) => {
            error!(%id, %error, "failed to recheck the retired job id before committing its upload");
            drop(runner_guard);
            safe_remove_job_dir(&state, id).await;
            return Err(ApiError::Internal);
        }
    }
    if let Err(error) = state.0.persistence.save(&snapshot).await {
        error!(%error, %id, "failed to persist a newly uploaded job");
        drop(runner_guard);
        safe_remove_job_dir(&state, id).await;
        return Err(ApiError::Internal);
    }
    state.0.jobs.write().await.insert(id, entry.clone());
    pending_guard.commit();

    let task_state = state.clone();
    let release_entry = entry.clone();
    state
        .0
        .tasks
        .spawn(async move { run_job(task_state, entry).await });
    drop(runner_guard);
    if upload_state.cancellation.is_cancelled() {
        request_cancellation(&state, &release_entry, "上传页面已离开，正在回收任务").await;
    }
    let response_snapshot = release_entry.snapshot.read().await.clone();
    Ok((StatusCode::ACCEPTED, Json(response_snapshot)))
}

fn parse_requested_job_id(headers: &HeaderMap) -> Result<Option<Uuid>, ApiError> {
    let Some(value) = headers.get("x-job-id") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::BadRequest("任务编号格式无效".to_owned()))?;
    let id =
        Uuid::parse_str(value).map_err(|_| ApiError::BadRequest("任务编号格式无效".to_owned()))?;
    if id.to_string() != value {
        return Err(ApiError::BadRequest("任务编号必须使用标准 UUID".to_owned()));
    }
    Ok(Some(id))
}

struct SecureUploadTarget {
    file: Option<File>,
    #[cfg(target_os = "linux")]
    job_dir_fd: Option<rustix::fd::OwnedFd>,
    #[cfg(target_os = "linux")]
    staging_name: Option<String>,
}

#[cfg(target_os = "linux")]
impl Drop for SecureUploadTarget {
    fn drop(&mut self) {
        let (Some(job_dir_fd), Some(staging_name)) =
            (self.job_dir_fd.as_ref(), self.staging_name.as_deref())
        else {
            return;
        };
        let _ = rustix::fs::unlinkat(job_dir_fd, staging_name, rustix::fs::AtFlags::empty());
        let _ = rustix::fs::fsync(job_dir_fd);
    }
}

impl SecureUploadTarget {
    async fn publish(mut self) -> Result<(), ApiError> {
        #[cfg(target_os = "linux")]
        {
            let file = self.file.take().ok_or(ApiError::Internal)?;
            let job_dir_fd = self.job_dir_fd.take().ok_or(ApiError::Internal)?;
            let staging_name = self.staging_name.take().ok_or(ApiError::Internal)?;
            let file = file.into_std().await;
            tokio::task::spawn_blocking(move || {
                publish_secure_upload_file(file, job_dir_fd, staging_name)
            })
            .await
            .map_err(internal_error)?
            .map_err(internal_error)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
            Err(ApiError::Internal)
        }
    }
}

#[cfg(target_os = "linux")]
async fn create_secure_upload_target(
    state: &AppState,
    id: Uuid,
) -> Result<SecureUploadTarget, ApiError> {
    let upload_root = state.0.config.state_root.join("uploads");
    let (file, job_dir_fd, staging_name) =
        tokio::task::spawn_blocking(move || create_secure_upload_file(&upload_root, id))
            .await
            .map_err(internal_error)?
            .map_err(internal_error)?;
    Ok(SecureUploadTarget {
        file: Some(File::from_std(file)),
        job_dir_fd: Some(job_dir_fd),
        staging_name: Some(staging_name),
    })
}

#[cfg(target_os = "linux")]
fn create_secure_upload_file(
    upload_root: &Path,
    id: Uuid,
) -> Result<(std::fs::File, rustix::fd::OwnedFd, String), BoxError> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};

    let root_fd = rustix::fs::open(
        upload_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    let job_dir_fd = rustix::fs::openat2(
        &root_fd,
        id.to_string(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )?;
    let (file, staging_name) = create_secure_staging_file(&job_dir_fd, resolve)?;
    Ok((file, job_dir_fd, staging_name))
}

#[cfg(target_os = "linux")]
fn create_secure_staging_file(
    job_dir_fd: &rustix::fd::OwnedFd,
    resolve: rustix::fs::ResolveFlags,
) -> Result<(std::fs::File, String), BoxError> {
    use rustix::fs::{Mode, OFlags};

    let staging_name = format!(".input.{}.tmp", Uuid::new_v4());
    let upload_fd = rustix::fs::openat2(
        job_dir_fd,
        staging_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
        resolve,
    )?;
    Ok((std::fs::File::from(upload_fd), staging_name))
}

#[cfg(target_os = "linux")]
fn publish_secure_upload_file(
    file: std::fs::File,
    job_dir_fd: rustix::fd::OwnedFd,
    staging_name: String,
) -> Result<(), BoxError> {
    use rustix::fs::RenameFlags;

    let mut renamed = false;
    let result = (|| -> Result<(), BoxError> {
        file.sync_all()?;
        rustix::fs::renameat_with(
            &job_dir_fd,
            staging_name.as_str(),
            &job_dir_fd,
            "input.pdf",
            RenameFlags::NOREPLACE,
        )?;
        renamed = true;
        rustix::fs::fsync(&job_dir_fd)?;
        Ok(())
    })();
    if result.is_err() {
        let cleanup_name = if renamed {
            "input.pdf"
        } else {
            staging_name.as_str()
        };
        let _ = rustix::fs::unlinkat(&job_dir_fd, cleanup_name, rustix::fs::AtFlags::empty());
        let _ = rustix::fs::fsync(&job_dir_fd);
    }
    result
}

#[cfg(target_os = "linux")]
async fn publish_private_upload_to_worker(state: &AppState, id: Uuid) -> Result<(), ApiError> {
    let upload_root = state.0.config.state_root.join("uploads");
    let data_root = state.0.config.data_root.clone();
    tokio::task::spawn_blocking(move || materialize_worker_input(&upload_root, &data_root, id))
        .await
        .map_err(internal_error)?
        .map_err(internal_error)
}

#[cfg(target_os = "linux")]
fn materialize_worker_input(
    upload_root: &Path,
    data_root: &Path,
    id: Uuid,
) -> Result<(), BoxError> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};

    let root_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    let upload_root_fd = rustix::fs::open(upload_root, root_flags, Mode::empty())?;
    let source_fd = rustix::fs::openat2(
        &upload_root_fd,
        format!("{id}/input.pdf"),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )?;
    let mut source = std::fs::File::from(source_fd);
    let metadata = source.metadata()?;
    if !metadata.is_file() || metadata.len() < 5 {
        return Err("private upload is not a non-empty regular PDF".into());
    }
    let mut prefix = [0_u8; 5];
    std::io::Read::read_exact(&mut source, &mut prefix)?;
    if &prefix != b"%PDF-" {
        return Err("private upload failed PDF magic validation".into());
    }
    std::io::Seek::rewind(&mut source)?;

    let data_root_fd = rustix::fs::open(data_root, root_flags, Mode::empty())?;
    rustix::fs::mkdirat(&data_root_fd, id.to_string(), Mode::RWXU)?;
    let job_dir_fd = rustix::fs::openat2(
        &data_root_fd,
        id.to_string(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )?;
    let (mut destination, staging_name) = create_secure_staging_file(&job_dir_fd, resolve)?;
    let copy_result = (|| -> Result<(), BoxError> {
        let mut limited = std::io::Read::take(&mut source, metadata.len());
        let copied = std::io::copy(&mut limited, &mut destination)?;
        if copied != metadata.len() {
            return Err("private upload changed while being published".into());
        }
        std::io::Write::flush(&mut destination)?;
        destination.sync_all()?;
        Ok(())
    })();
    if let Err(error) = copy_result {
        let _ = rustix::fs::unlinkat(
            &job_dir_fd,
            staging_name.as_str(),
            rustix::fs::AtFlags::empty(),
        );
        let _ = rustix::fs::fsync(&job_dir_fd);
        return Err(error);
    }
    publish_secure_upload_file(destination, job_dir_fd, staging_name)?;
    rustix::fs::fsync(&data_root_fd)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn publish_private_upload_to_worker(_state: &AppState, _id: Uuid) -> Result<(), ApiError> {
    error!("secure worker input publication requires Linux openat2 and renameat2");
    Err(ApiError::Internal)
}

#[cfg(not(target_os = "linux"))]
async fn create_secure_upload_target(
    _state: &AppState,
    _id: Uuid,
) -> Result<SecureUploadTarget, ApiError> {
    error!("secure private upload requires Linux openat2 and renameat2");
    Err(ApiError::Internal)
}

async fn receive_upload(
    state: &AppState,
    id: Uuid,
    multipart: &mut Multipart,
    cancellation: &CancellationToken,
) -> Result<PendingUpload, ApiError> {
    let mut pending = PendingUpload::default();
    loop {
        let next_field = tokio::select! {
            _ = cancellation.cancelled() => {
                return Err(ApiError::Conflict("上传已取消".to_owned()));
            }
            field = multipart.next_field() => {
                field.map_err(|_| ApiError::BadRequest("上传数据格式无效".to_owned()))?
            }
        };
        let Some(mut field) = next_field else {
            break;
        };
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            if pending.saw_file {
                return Err(ApiError::BadRequest("每个任务只能上传一个 PDF".to_owned()));
            }
            pending.saw_file = true;
            pending.filename = Some(sanitize_filename(
                field.file_name().unwrap_or("document.pdf"),
            ));
            let mut output = create_secure_upload_target(state, id).await?;
            let mut written = 0_u64;
            let mut prefix = BytesMut::new();
            loop {
                let next_chunk = tokio::select! {
                    _ = cancellation.cancelled() => {
                        return Err(ApiError::Conflict("上传已取消".to_owned()));
                    }
                    chunk = field.chunk() => {
                        chunk.map_err(|_| ApiError::BadRequest("上传在完成前中断".to_owned()))?
                    }
                };
                let Some(chunk) = next_chunk else {
                    break;
                };
                written = written.saturating_add(chunk.len() as u64);
                if written > state.0.config.max_upload_bytes {
                    return Err(ApiError::BadRequest(format!(
                        "PDF 不能超过 {} MB",
                        state.0.config.max_upload_bytes / 1024 / 1024
                    )));
                }
                if prefix.len() < 5 {
                    let remaining = 5 - prefix.len();
                    prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                output
                    .file
                    .as_mut()
                    .ok_or(ApiError::Internal)?
                    .write_all(&chunk)
                    .await
                    .map_err(internal_error)?;
            }
            let output_file = output.file.as_mut().ok_or(ApiError::Internal)?;
            output_file.flush().await.map_err(internal_error)?;
            output_file.sync_all().await.map_err(internal_error)?;
            if written == 0 || prefix.as_ref() != b"%PDF-" {
                return Err(ApiError::BadRequest("文件不是有效的 PDF".to_owned()));
            }
            output.publish().await?;
        } else {
            if !matches!(
                name.as_str(),
                "source_lang"
                    | "target_lang"
                    | "pages"
                    | "output_mono"
                    | "output_dual"
                    | "provider"
                    | "provider_id"
                    | "model_id"
                    | "thinking_mode"
                    | "reasoning_effort"
                    | "deepseek_model"
                    | "deepseek_thinking_mode"
                    | "deepseek_reasoning_effort"
            ) {
                return Err(ApiError::BadRequest("包含未知的表单字段".to_owned()));
            }
            let value = read_small_field(field, cancellation).await?;
            match name.as_str() {
                "source_lang" => pending.source_lang = Some(value),
                "target_lang" => pending.target_lang = Some(value),
                "pages" => pending.pages = Some(value),
                "output_mono" => pending.output_mono = Some(parse_bool(&value)?),
                "output_dual" => pending.output_dual = Some(parse_bool(&value)?),
                "provider" => pending.provider = Some(value),
                "provider_id" => pending.provider_id = Some(value),
                "model_id" => pending.model_id = Some(value),
                "thinking_mode" => pending.thinking_mode = Some(value),
                "reasoning_effort" => pending.reasoning_effort = Some(value),
                "deepseek_model" => pending.deepseek_model = Some(value),
                "deepseek_thinking_mode" => pending.deepseek_thinking_mode = Some(value),
                "deepseek_reasoning_effort" => pending.deepseek_reasoning_effort = Some(value),
                _ => unreachable!("unknown multipart fields were rejected above"),
            }
        }
    }
    if !pending.saw_file {
        return Err(ApiError::BadRequest("请选择 PDF 文件".to_owned()));
    }
    Ok(pending)
}

async fn read_small_field(
    mut field: axum::extract::multipart::Field<'_>,
    cancellation: &CancellationToken,
) -> Result<String, ApiError> {
    let mut bytes = BytesMut::new();
    loop {
        let next_chunk = tokio::select! {
            _ = cancellation.cancelled() => {
                return Err(ApiError::Conflict("上传已取消".to_owned()));
            }
            chunk = field.chunk() => {
                chunk.map_err(|_| ApiError::BadRequest("表单字段无效".to_owned()))?
            }
        };
        let Some(chunk) = next_chunk else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_FIELD_BYTES {
            return Err(ApiError::BadRequest("表单字段过长".to_owned()));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes.to_vec())
        .map_err(|_| ApiError::BadRequest("表单字段编码无效".to_owned()))
}

fn validate_upload_options(pending: &PendingUpload) -> Result<JobOptions, ApiError> {
    let source_lang = pending
        .source_lang
        .clone()
        .unwrap_or_else(|| "auto".to_owned());
    let target_lang = pending
        .target_lang
        .clone()
        .unwrap_or_else(|| "zh".to_owned());
    if !LANGUAGE_CODES.contains(&source_lang.as_str()) {
        return Err(ApiError::BadRequest("不支持该源语言".to_owned()));
    }
    if target_lang == "auto" || !LANGUAGE_CODES.contains(&target_lang.as_str()) {
        return Err(ApiError::BadRequest("不支持该目标语言".to_owned()));
    }
    if source_lang == target_lang {
        return Err(ApiError::BadRequest("源语言和目标语言不能相同".to_owned()));
    }
    let pages = normalize_pages(pending.pages.as_deref())?;
    let output_mono = pending.output_mono.unwrap_or(true);
    let output_dual = pending.output_dual.unwrap_or(true);
    if !output_mono && !output_dual {
        return Err(ApiError::BadRequest("至少选择一种输出文件".to_owned()));
    }
    let legacy_provider = nonblank(pending.provider.as_deref());
    let explicit_provider_id = nonblank(pending.provider_id.as_deref());
    if let (Some(legacy), Some(explicit)) = (legacy_provider, explicit_provider_id)
        && legacy != explicit
    {
        return Err(ApiError::BadRequest("翻译渠道字段互相冲突".to_owned()));
    }
    let provider_id = explicit_provider_id
        .or(legacy_provider)
        .unwrap_or("google")
        .to_owned();
    if provider_id.len() > 64
        || provider_id.is_empty()
        || !provider_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ApiError::BadRequest("翻译渠道编号无效".to_owned()));
    }
    let provider = match provider_id.as_str() {
        "google" => TranslationProvider::Google,
        "deepseek" => TranslationProvider::DeepSeek,
        _ => TranslationProvider::OpenAiCompatible,
    };
    let legacy_model = nonblank(pending.deepseek_model.as_deref());
    let generic_model = nonblank(pending.model_id.as_deref());
    if let (Some(legacy), Some(generic)) = (legacy_model, generic_model)
        && legacy != generic
    {
        return Err(ApiError::BadRequest("模型字段互相冲突".to_owned()));
    }
    let requested_model = generic_model.or(legacy_model);
    let legacy_thinking = nonblank(pending.deepseek_thinking_mode.as_deref());
    let generic_thinking = nonblank(pending.thinking_mode.as_deref());
    if let (Some(legacy), Some(generic)) = (legacy_thinking, generic_thinking)
        && legacy != generic
    {
        return Err(ApiError::BadRequest("思考模式字段互相冲突".to_owned()));
    }
    let requested_thinking_mode = generic_thinking.or(legacy_thinking);
    let legacy_effort = nonblank(pending.deepseek_reasoning_effort.as_deref());
    let generic_effort = nonblank(pending.reasoning_effort.as_deref());
    if let (Some(legacy), Some(generic)) = (legacy_effort, generic_effort)
        && legacy != generic
    {
        return Err(ApiError::BadRequest("推理强度字段互相冲突".to_owned()));
    }
    let requested_reasoning_effort = generic_effort.or(legacy_effort);
    let (deepseek_model, deepseek_thinking_mode, deepseek_reasoning_effort) = match provider {
        TranslationProvider::Google => {
            if requested_model.is_some()
                || requested_thinking_mode.is_some()
                || requested_reasoning_effort.is_some()
            {
                return Err(ApiError::BadRequest(
                    "Google 任务不能包含 DeepSeek 选项".to_owned(),
                ));
            }
            (None, None, None)
        }
        TranslationProvider::DeepSeek => {
            let model = match requested_model {
                None | Some(DEEPSEEK_MODEL_FLASH) => DeepSeekModel::V4Flash,
                Some(DEEPSEEK_MODEL_PRO) => DeepSeekModel::V4Pro,
                Some(_) => {
                    return Err(ApiError::BadRequest("不支持该 DeepSeek 模型".to_owned()));
                }
            };
            let thinking_mode = match requested_thinking_mode {
                None => None,
                Some("enabled") => Some(DeepSeekThinkingMode::Enabled),
                Some("disabled") => Some(DeepSeekThinkingMode::Disabled),
                Some(_) => {
                    return Err(ApiError::BadRequest(
                        "DeepSeek 思考模式必须为 enabled 或 disabled".to_owned(),
                    ));
                }
            };
            let reasoning_effort = match requested_reasoning_effort {
                None => None,
                Some("high") => Some(DeepSeekReasoningEffort::High),
                Some("max") => Some(DeepSeekReasoningEffort::Max),
                Some(_) => {
                    return Err(ApiError::BadRequest(
                        "DeepSeek 推理强度必须为 high 或 max".to_owned(),
                    ));
                }
            };
            if reasoning_effort.is_some() && thinking_mode != Some(DeepSeekThinkingMode::Enabled) {
                return Err(ApiError::BadRequest(
                    "只有显式启用 DeepSeek 思考模式时才能设置推理强度".to_owned(),
                ));
            }
            let reasoning_effort = if thinking_mode == Some(DeepSeekThinkingMode::Enabled) {
                Some(reasoning_effort.unwrap_or(DeepSeekReasoningEffort::High))
            } else {
                None
            };
            (Some(model), thinking_mode, reasoning_effort)
        }
        TranslationProvider::OpenAiCompatible => {
            let model = requested_model
                .ok_or_else(|| ApiError::BadRequest("OpenAI 兼容渠道必须指定模型".to_owned()))?;
            if model.len() > 128 || model.chars().any(char::is_control) {
                return Err(ApiError::BadRequest("模型编号无效".to_owned()));
            }
            if requested_thinking_mode.is_some() || requested_reasoning_effort.is_some() {
                return Err(ApiError::BadRequest(
                    "OpenAI 兼容渠道不接受 DeepSeek 专用选项".to_owned(),
                ));
            }
            (None, None, None)
        }
    };
    let model_id = match provider {
        TranslationProvider::Google => None,
        TranslationProvider::DeepSeek => Some(
            match deepseek_model.expect("validated DeepSeek model is present") {
                DeepSeekModel::V4Flash => DEEPSEEK_MODEL_FLASH,
                DeepSeekModel::V4Pro => DEEPSEEK_MODEL_PRO,
            }
            .to_owned(),
        ),
        TranslationProvider::OpenAiCompatible => requested_model.map(str::to_owned),
    };
    Ok(JobOptions {
        source_lang,
        target_lang,
        pages,
        output_mono,
        output_dual,
        provider,
        deepseek_model,
        deepseek_thinking_mode,
        deepseek_reasoning_effort,
        provider_id,
        provider_revision: None,
        model_id,
    })
}

fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn normalize_pages(value: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() > 256 {
        return Err(ApiError::BadRequest("页码范围过长".to_owned()));
    }
    let value = value.replace(' ', "");
    for part in value.split(',') {
        let bounds: Vec<_> = part.split('-').collect();
        if bounds.is_empty() || bounds.len() > 2 {
            return Err(ApiError::BadRequest("页码格式应为 1,3-5".to_owned()));
        }
        let numbers = bounds
            .iter()
            .map(|number| number.parse::<u32>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ApiError::BadRequest("页码格式应为 1,3-5".to_owned()))?;
        if numbers.contains(&0) || (numbers.len() == 2 && numbers[0] > numbers[1]) {
            return Err(ApiError::BadRequest(
                "页码必须从 1 开始并按升序填写".to_owned(),
            ));
        }
    }
    Ok(Some(value))
}

fn parse_bool(value: &str) -> Result<bool, ApiError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Ok(true),
        "false" | "0" | "off" | "no" => Ok(false),
        _ => Err(ApiError::BadRequest("布尔选项无效".to_owned())),
    }
}

fn sanitize_filename(value: &str) -> String {
    let filename = value
        .replace(['\\', '/'], "_")
        .chars()
        .filter(|character| !character.is_control())
        .take(180)
        .collect::<String>();
    if filename.to_ascii_lowercase().ends_with(".pdf") && filename.len() > 4 {
        filename
    } else {
        "document.pdf".to_owned()
    }
}

async fn get_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Json<JobSnapshot>, ApiError> {
    if pending_upload_token(&state, id).is_some() {
        return Err(ApiError::Conflict("任务仍在上传，请稍后重试".to_owned()));
    }
    let entry = find_job(&state, id).await?;
    let snapshot = entry.snapshot.read().await.clone();
    Ok(Json(snapshot))
}

async fn job_events(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let entry = find_job(&state, id).await?;
    let mut events = BroadcastStream::new(entry.sender.subscribe());
    let initial = entry.snapshot.read().await.clone();
    let fallback_entry = entry.clone();
    let output = stream! {
        yield Ok(snapshot_event(&initial));
        if initial.status.is_terminal() {
            return;
        }
        while let Some(event) = events.next().await {
            match event {
                Ok(snapshot) => {
                    let terminal = snapshot.status.is_terminal();
                    yield Ok(snapshot_event(&snapshot));
                    if terminal {
                        return;
                    }
                }
                Err(_) => {
                    let snapshot = fallback_entry.snapshot.read().await.clone();
                    let terminal = snapshot.status.is_terminal();
                    yield Ok(snapshot_event(&snapshot));
                    if terminal {
                        return;
                    }
                }
            }
        }
    };
    Ok(Sse::new(output).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn snapshot_event(snapshot: &JobSnapshot) -> Event {
    Event::default()
        .id(snapshot.revision.to_string())
        .json_data(snapshot)
        .unwrap_or_else(|_| Event::default().event("error").data("serialization failed"))
}

async fn cancel_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<AxumResponse, ApiError> {
    let StopTarget::Job(entry) = find_job_or_tombstone_prestart(&state, id).await? else {
        return Ok(StatusCode::ACCEPTED.into_response());
    };
    request_cancellation(&state, &entry, "正在安全停止翻译").await;
    Ok(Json(entry.snapshot.read().await.clone()).into_response())
}

async fn heartbeat_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    if pending_upload_token(&state, id).is_some() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let entry = find_job(&state, id).await?;
    let _publish_guard = entry.publish_lock.lock().await;
    if entry.disposed.load(AtomicOrdering::Acquire) {
        return Err(ApiError::Gone);
    }
    let mut lease = entry.lease.lock().await;
    lease.last_heartbeat = Instant::now();
    lease.released_at = None;
    Ok(StatusCode::NO_CONTENT)
}

async fn release_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    let StopTarget::Job(entry) = find_job_or_tombstone_prestart(&state, id).await? else {
        return Ok(StatusCode::ACCEPTED);
    };
    let _publish_guard = entry.publish_lock.lock().await;
    if entry.disposed.load(AtomicOrdering::Acquire) {
        return Err(ApiError::Gone);
    }
    entry.lease.lock().await.released_at = Some(Instant::now());
    Ok(StatusCode::ACCEPTED)
}

async fn dispose_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    if let Some(cancellation) = pending_upload_token(&state, id) {
        cancellation.cancel();
        return Ok(StatusCode::ACCEPTED);
    }
    let entry = find_job(&state, id).await?;
    let publish_guard = entry.publish_lock.lock().await;
    let snapshot = entry.snapshot.read().await.clone();
    if !snapshot.status.is_terminal() {
        return Err(ApiError::Conflict(
            "活动任务必须先取消，不能直接删除".to_owned(),
        ));
    }
    if let Err(error) = state
        .0
        .persistence
        .retire(id, state.0.config.id_tombstone_retention)
        .await
    {
        error!(%id, %error, "failed to dispose terminal job state");
        return Err(ApiError::Internal);
    }
    entry.disposed.store(true, AtomicOrdering::Release);
    if let Err(error) = remove_job_files(&state, id).await {
        warn!(%id, %error, "failed to dispose terminal job files; restoring state");
        entry.disposed.store(false, AtomicOrdering::Release);
        if let Err(save_error) = state.0.persistence.save(&snapshot).await {
            error!(%id, %save_error, "failed to restore terminal job after dispose error");
        }
        return Err(ApiError::Internal);
    }
    state.0.jobs.write().await.remove(&id);
    drop(publish_guard);
    Ok(StatusCode::NO_CONTENT)
}

async fn download_file(
    State(state): State<AppState>,
    AxumPath((id, kind)): AxumPath<(Uuid, String)>,
) -> Result<Response<Body>, ApiError> {
    let entry = find_job(&state, id).await?;
    let snapshot = entry.snapshot.read().await.clone();
    if snapshot.status != JobStatus::Succeeded {
        return Err(ApiError::BadRequest("任务尚未生成下载文件".to_owned()));
    }
    let (available, artifact_name, filename, content_type) = match kind.as_str() {
        "mono" => (
            snapshot.files.mono,
            "mono.pdf",
            "translated-mono.pdf",
            "application/pdf",
        ),
        "dual" => (
            snapshot.files.dual,
            "dual.pdf",
            "translated-dual.pdf",
            "application/pdf",
        ),
        "glossary" => (
            snapshot.files.glossary,
            "glossary.csv",
            "translated-glossary.csv",
            "text/csv; charset=utf-8",
        ),
        _ => return Err(ApiError::NotFound),
    };
    if !available {
        return Err(ApiError::NotFound);
    }
    let artifact_root = state.0.config.state_root.join("artifacts");
    let (file, length) = tokio::task::spawn_blocking(move || {
        open_published_artifact(&artifact_root, id, artifact_name)
    })
    .await
    .map_err(internal_error)?
    .map_err(|_| ApiError::NotFound)?;
    let file = File::from_std(file);
    let body = Body::from_stream(ReaderStream::new(file));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, length)
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(CACHE_CONTROL, "private, no-store")
        .body(body)
        .map_err(internal_error)
}

async fn find_job(state: &AppState, id: Uuid) -> Result<Arc<JobEntry>, ApiError> {
    let entry = state.0.jobs.read().await.get(&id).cloned();
    if let Some(entry) = entry {
        return Ok(entry);
    }
    match state.0.persistence.is_tombstoned(id).await {
        Ok(true) => Err(ApiError::Gone),
        Ok(false) => Err(ApiError::NotFound),
        Err(error) => {
            error!(%id, %error, "failed to check whether a missing job id was retired");
            Err(ApiError::Internal)
        }
    }
}

fn pending_upload_token(state: &AppState, id: Uuid) -> Option<CancellationToken> {
    state
        .0
        .pending_uploads
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&id)
        .map(|pending| pending.cancellation.clone())
}

enum StopTarget {
    Job(Arc<JobEntry>),
    PreStart,
}

async fn find_job_or_tombstone_prestart(
    state: &AppState,
    id: Uuid,
) -> Result<StopTarget, ApiError> {
    // create_job uses this same gate for reservation and final commit. Once we
    // observe neither a committed job nor a pending upload, its id cannot be
    // committed until this durable tombstone is visible.
    let _runner_guard = state.0.runner_gate.lock().await;
    if let Some(entry) = state.0.jobs.read().await.get(&id).cloned() {
        return Ok(StopTarget::Job(entry));
    }
    if let Some(cancellation) = pending_upload_token(state, id) {
        cancellation.cancel();
    }
    if let Err(error) = state
        .0
        .persistence
        .tombstone_prestart(
            id,
            PRESTART_TOMBSTONE_RETENTION,
            PRESTART_TOMBSTONE_CAPACITY,
        )
        .await
    {
        error!(%id, %error, "failed to persist a pre-start job tombstone");
        return Err(ApiError::Internal);
    }
    Ok(StopTarget::PreStart)
}

async fn request_cancellation(state: &AppState, entry: &Arc<JobEntry>, message: &str) -> bool {
    let committed = commit_job_update(state, entry, |snapshot| {
        if snapshot.status.is_terminal() || entry.cancellation.is_cancelled() {
            return false;
        }
        snapshot.status = JobStatus::Cancelling;
        snapshot.stage = "正在取消".to_owned();
        snapshot.message = message.to_owned();
        // Set the token while the publish lock is still held so no progress
        // revision can race in after this Cancelling candidate. Persistence
        // failure below is fail-stop, leaving reconciliation to restart.
        entry.cancellation.cancel();
        true
    })
    .await;
    match committed {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            error!(%error, "failed to durably persist requested cancellation");
            fail_stop_coordinator("job cancellation could not be durably persisted")
        }
    }
}

#[derive(Serialize)]
struct WorkerRequest {
    input_path: String,
    output_dir: String,
    source_lang: String,
    target_lang: String,
    pages: Option<String>,
    output_mono: bool,
    output_dual: bool,
    provider: TranslationProvider,
    model: Option<String>,
    thinking_mode: Option<DeepSeekThinkingMode>,
    reasoning_effort: Option<DeepSeekReasoningEffort>,
    provider_name: Option<String>,
    openai_base_url: Option<String>,
    openai_api_key: Option<Zeroizing<String>>,
}

#[derive(Debug, Deserialize)]
struct WorkerEvent {
    #[serde(rename = "type")]
    kind: String,
    progress: Option<f64>,
    message: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WorkerTerminal {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Debug, Deserialize)]
struct WorkerStateResponse {
    status: String,
}

#[derive(Debug, Deserialize)]
struct WorkerControlResponse {
    status: String,
    active_jobs: usize,
}

impl WorkerStateResponse {
    fn terminal(&self) -> Option<WorkerTerminal> {
        match self.status.as_str() {
            "finished" => Some(WorkerTerminal::Finished),
            "failed" => Some(WorkerTerminal::Failed),
            "cancelled" => Some(WorkerTerminal::Cancelled),
            _ => None,
        }
    }
}

enum WorkerStart {
    Running,
    Terminal(WorkerTerminal),
}

async fn run_job(state: AppState, entry: Arc<JobEntry>) {
    let id = entry.snapshot.read().await.id;
    let permit = loop {
        if entry.cancellation.is_cancelled() {
            mark_cancelled(&state, &entry, "排队任务已取消").await;
            safe_remove_job_dir(&state, id).await;
            release_job_admission(&entry);
            return;
        }
        tokio::select! {
            permit = state.0.permits.clone().acquire_owned() => {
                match permit {
                    Ok(permit) => break permit,
                    Err(_) => {
                        if entry.cancellation.is_cancelled() || state.0.shutdown.is_cancelled() {
                            mark_cancelled(&state, &entry, "服务关闭，排队任务已取消").await;
                        } else {
                            mark_failed(&state, &entry, "任务队列已关闭").await;
                        }
                        safe_remove_job_dir(&state, id).await;
                        release_job_admission(&entry);
                        return;
                    }
                }
            }
            _ = entry.cancellation.cancelled() => {}
        }
    };

    let starting_snapshot = match commit_job_update(&state, &entry, |snapshot| {
        if entry.cancellation.is_cancelled() || state.0.shutdown.is_cancelled() {
            false
        } else {
            snapshot.status = JobStatus::Running;
            snapshot.stage = "启动翻译引擎".to_owned();
            snapshot.message = "正在连接内部翻译服务".to_owned();
            snapshot.started_at = Some(Utc::now());
            true
        }
    })
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            error!(%id, %error, "failed to durably persist job start");
            fail_stop_coordinator("job start could not be durably persisted")
        }
    };
    let Some(starting_snapshot) = starting_snapshot else {
        mark_cancelled(&state, &entry, "任务已取消").await;
        safe_remove_job_dir(&state, id).await;
        drop(permit);
        release_job_admission(&entry);
        return;
    };

    let snapshot = starting_snapshot;
    let Some(job_worker_identity) =
        ready_worker_identity_for_provider(&state, snapshot.options.provider).await
    else {
        mark_failed(&state, &entry, "翻译服务暂时不可用").await;
        cleanup_failed_job(&state, &entry, id).await;
        drop(permit);
        release_job_admission(&entry);
        return;
    };
    let runtime_provider = entry
        .runtime_provider
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let runtime_api_key = match runtime_provider.as_ref() {
        Some(runtime) => match state.0.admin.open_runtime_secret(id, runtime) {
            Ok(secret) => Some(secret),
            Err(error) => {
                warn!(%id, %error, "could not open the accepted in-memory provider credential");
                mark_failed(&state, &entry, "任务凭据不可用").await;
                cleanup_failed_job(&state, &entry, id).await;
                drop(permit);
                release_job_admission(&entry);
                return;
            }
        },
        None => None,
    };
    let (model, provider_name, openai_base_url, openai_api_key) = match runtime_provider {
        Some(runtime) if snapshot.options.provider == TranslationProvider::DeepSeek => (
            Some(runtime.model),
            Some(runtime.provider_name),
            None,
            runtime_api_key,
        ),
        Some(runtime) => (
            Some(runtime.model),
            Some(runtime.provider_name),
            Some(runtime.base_url),
            runtime_api_key,
        ),
        None => (None, None, None, None),
    };
    let request = WorkerRequest {
        input_path: entry
            .job_dir
            .join("input.pdf")
            .to_string_lossy()
            .to_string(),
        output_dir: entry.job_dir.join("output").to_string_lossy().to_string(),
        source_lang: snapshot.options.source_lang.clone(),
        target_lang: snapshot.options.target_lang.clone(),
        pages: snapshot.options.pages.clone(),
        output_mono: snapshot.options.output_mono,
        output_dual: snapshot.options.output_dual,
        provider: snapshot.options.provider,
        model,
        thinking_mode: snapshot.options.deepseek_thinking_mode,
        reasoning_effort: snapshot.options.deepseek_reasoning_effort,
        provider_name,
        openai_base_url,
        openai_api_key,
    };

    let worker_start = start_worker_job(&state, id, &request).await;
    // The plaintext credential is needed only while serializing and sending
    // the start request. Drop it immediately after that request completes.
    drop(request);
    match worker_start {
        Ok(WorkerStart::Running) => {}
        Ok(WorkerStart::Terminal(terminal)) => {
            finalize_confirmed_terminal(
                &state,
                &entry,
                id,
                terminal,
                "翻译服务在启动阶段提前结束",
                entry.cancellation.is_cancelled(),
            )
            .await;
            cleanup_failed_job(&state, &entry, id).await;
            finish_job_isolation(&state, &entry, id, &job_worker_identity, permit).await;
            return;
        }
        Err(error_message) => {
            let user_cancelled = entry.cancellation.is_cancelled();
            request_cancellation(&state, &entry, "正在核对翻译进程状态").await;
            let terminal = cancel_worker_until_terminal(&state, id).await;
            finalize_confirmed_terminal(
                &state,
                &entry,
                id,
                terminal,
                &error_message,
                user_cancelled,
            )
            .await;
            cleanup_failed_job(&state, &entry, id).await;
            finish_job_isolation(&state, &entry, id, &job_worker_identity, permit).await;
            return;
        }
    }

    update_job(&state, &entry, |snapshot| {
        if !entry.cancellation.is_cancelled() {
            snapshot.status = JobStatus::Running;
            snapshot.stage = "准备文档".to_owned();
            snapshot.message = match snapshot.options.provider {
                TranslationProvider::Google => "Google 翻译引擎已启动",
                TranslationProvider::DeepSeek => "DeepSeek V4 翻译引擎已启动",
                TranslationProvider::OpenAiCompatible => "OpenAI 兼容翻译引擎已启动",
            }
            .to_owned();
        }
    })
    .await;

    let mut cancellation = tokio::spawn(forward_cancellation(state.clone(), entry.clone(), id));
    tokio::select! {
        events = consume_worker_events(&state, &entry, id) => {
            cancellation.abort();
            let _ = cancellation.await;
            if let Err(message) = events {
                let user_cancelled = entry.cancellation.is_cancelled();
                request_cancellation(&state, &entry, "进度连接异常，正在回收翻译进程").await;
                let terminal = cancel_worker_until_terminal(&state, id).await;
                finalize_confirmed_terminal(
                    &state,
                    &entry,
                    id,
                    terminal,
                    &message,
                    user_cancelled,
                ).await;
            }
        }
        confirmed = &mut cancellation => {
            match confirmed {
                Ok(terminal) => {
                    finalize_confirmed_terminal(
                        &state,
                        &entry,
                        id,
                        terminal,
                        "翻译任务取消后未返回完整结果",
                        true,
                    ).await;
                }
                Err(error) => {
                    warn!(%id, %error, "cancellation reconciler stopped unexpectedly");
                    let terminal = cancel_worker_until_terminal(&state, id).await;
                    finalize_confirmed_terminal(
                        &state,
                        &entry,
                        id,
                        terminal,
                        "取消协调任务异常",
                        true,
                    ).await;
                }
            }
        }
    }
    cleanup_failed_job(&state, &entry, id).await;
    finish_job_isolation(&state, &entry, id, &job_worker_identity, permit).await;
}

async fn finish_job_isolation(
    state: &AppState,
    entry: &Arc<JobEntry>,
    id: Uuid,
    job_worker_identity: &WorkerIdentity,
    permit: OwnedSemaphorePermit,
) {
    // A terminal event confirms the task subprocess, but a malicious document
    // may have escaped that process group. Destroy the complete worker epoch,
    // drain its replacement, and scrub the shared volume before another upload
    // or task can be admitted.
    let _ = await_automatic_worker_replacement(state, id, job_worker_identity.clone()).await;
    drop(permit);
    release_job_admission(entry);
}

async fn start_worker_job(
    state: &AppState,
    id: Uuid,
    request: &WorkerRequest,
) -> Result<WorkerStart, String> {
    let response = state
        .0
        .client
        .post(format!("{}/internal/jobs/{id}", state.0.config.worker_url))
        .header("x-worker-token", &state.0.config.worker_token)
        .timeout(Duration::from_secs(20))
        .json(request)
        .send()
        .await
        .map_err(|error| format!("无法启动翻译服务：{error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("翻译服务拒绝了任务（{status}）"));
    }
    let worker_state = response
        .json::<WorkerStateResponse>()
        .await
        .map_err(|_| "翻译服务启动响应无效".to_owned())?;
    Ok(match worker_state.terminal() {
        Some(terminal) => WorkerStart::Terminal(terminal),
        None if worker_state.status == "running" => WorkerStart::Running,
        None => return Err("翻译服务返回了未知启动状态".to_owned()),
    })
}

async fn forward_cancellation(state: AppState, entry: Arc<JobEntry>, id: Uuid) -> WorkerTerminal {
    entry.cancellation.cancelled().await;
    cancel_worker_until_terminal(&state, id).await
}

async fn cancel_worker_until_terminal(state: &AppState, id: Uuid) -> WorkerTerminal {
    let started_at = Instant::now();
    loop {
        if let Ok(event) = fetch_worker_terminal_event(state, id).await {
            let recovered = match event.kind.as_str() {
                "finished" => Some(WorkerTerminal::Finished),
                "failed" => Some(WorkerTerminal::Failed),
                "cancelled" => Some(WorkerTerminal::Cancelled),
                _ => None,
            };
            if let Some(terminal) = recovered {
                info!(%id, ?terminal, "recovered authoritative worker terminal journal");
                return terminal;
            }
        }
        let result = state
            .0
            .client
            .post(format!(
                "{}/internal/jobs/{id}/cancel",
                state.0.config.worker_url
            ))
            .header("x-worker-token", &state.0.config.worker_token)
            .timeout(Duration::from_secs(45))
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                match response.json::<WorkerStateResponse>().await {
                    Ok(worker_state) => {
                        if let Some(terminal) = worker_state.terminal() {
                            info!(%id, ?terminal, "Python worker confirmed terminal cleanup");
                            return terminal;
                        }
                        warn!(%id, status = %worker_state.status, "worker is still cleaning up");
                    }
                    Err(error) => {
                        warn!(%id, %error, "worker cancellation response was invalid");
                    }
                }
            }
            Ok(response) => {
                warn!(%id, status = %response.status(), "worker cancellation was rejected");
            }
            Err(error) => {
                warn!(%id, %error, "worker cancellation reconciliation failed");
            }
        }
        if started_at.elapsed() > state.0.config.cancel_reconcile_timeout {
            return force_recycle_worker(state, id).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn force_recycle_worker(state: &AppState, id: Uuid) -> WorkerTerminal {
    let observed_identity = fetch_worker_health(state)
        .await
        .filter(|health| health.is_ready(state.0.config.concurrency))
        .map(|health| health.identity());
    let cached_identity = state.0.worker_identity.read().await.clone();
    let previous_identity = observed_identity.or(cached_identity).unwrap_or_else(|| {
        fail_stop_coordinator("Python worker did not expose a complete identity before recycle")
    });
    warn!(
        %id,
        epoch = %previous_identity.epoch,
        container_boot_id = %previous_identity.container_boot_id,
        "destroying the complete Python worker epoch before releasing job isolation"
    );
    wait_for_worker_replacement(state, id, previous_identity, true).await
}

async fn await_automatic_worker_replacement(
    state: &AppState,
    id: Uuid,
    previous_identity: WorkerIdentity,
) -> WorkerTerminal {
    wait_for_worker_replacement(state, id, previous_identity, false).await
}

async fn wait_for_worker_replacement(
    state: &AppState,
    id: Uuid,
    mut previous_identity: WorkerIdentity,
    force_immediately: bool,
) -> WorkerTerminal {
    let mut fail_stop_requested = force_immediately;
    if force_immediately {
        request_worker_fail_stop(state).await;
    }
    let recycle_started = Instant::now();
    let mut last_fail_stop = Instant::now();
    let automatic_exit_deadline = state
        .0
        .config
        .release_grace
        .min(state.0.config.cancel_reconcile_timeout);
    loop {
        if recycle_started.elapsed() > state.0.config.worker_recycle_timeout {
            fail_stop_coordinator("Python worker could not be recycled before the hard deadline");
        }
        if let Some(health) = fetch_worker_health(state).await
            && health.is_ready(state.0.config.concurrency)
            && health.is_replacement_for(&previous_identity)
        {
            let identity = health.identity();
            info!(%id, epoch = %identity.epoch, container_boot_id = %identity.container_boot_id, "new Python worker container identity confirmed");
            *state.0.worker_identity.write().await = Some(identity.clone());
            if drain_worker_until_idle(state).await {
                if let Err(error) = purge_shared_job_root(state).await {
                    error!(%error, "replacement worker was drained but shared storage could not be scrubbed");
                    fail_stop_coordinator(
                        "shared worker storage could not be scrubbed before admission release",
                    );
                }
                let resume_started = Instant::now();
                loop {
                    match resume_worker(state).await {
                        Ok(()) => return WorkerTerminal::Cancelled,
                        Err(error) => {
                            warn!(%error, "new worker epoch was not ready to resume");
                        }
                    }
                    if resume_started.elapsed() > state.0.config.cancel_reconcile_timeout {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            warn!(epoch = %identity.epoch, container_boot_id = %identity.container_boot_id, "replacement worker could not be drained and resumed; recycling it again");
            previous_identity = identity;
            request_worker_fail_stop(state).await;
            fail_stop_requested = true;
            last_fail_stop = Instant::now();
        }
        if !fail_stop_requested && recycle_started.elapsed() >= automatic_exit_deadline {
            warn!(%id, "worker did not auto-exit after terminal state; requesting fail-stop");
            request_worker_fail_stop(state).await;
            fail_stop_requested = true;
            last_fail_stop = Instant::now();
        } else if fail_stop_requested && last_fail_stop.elapsed() >= Duration::from_secs(10) {
            request_worker_fail_stop(state).await;
            last_fail_stop = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn fail_stop_coordinator(reason: &str) -> ! {
    error!(%reason, "coordinator safety deadline expired; exiting for Docker recovery");
    std::process::exit(70)
}

async fn request_worker_fail_stop(state: &AppState) {
    let result = state
        .0
        .client
        .post(format!("{}/internal/fail-stop", state.0.config.worker_url))
        .header("x-worker-token", &state.0.config.worker_token)
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    match result {
        Ok(response) => warn!(status = %response.status(), "worker fail-stop request sent"),
        Err(error) => warn!(%error, "worker fail-stop request could not be delivered"),
    }
}

async fn drain_worker_until_idle(state: &AppState) -> bool {
    let started_at = Instant::now();
    loop {
        let result = state
            .0
            .client
            .post(format!("{}/internal/drain", state.0.config.worker_url))
            .header("x-worker-token", &state.0.config.worker_token)
            .timeout(Duration::from_secs(45))
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                match response.json::<WorkerControlResponse>().await {
                    Ok(control) if control.status == "idle" && control.active_jobs == 0 => {
                        info!("Python worker startup drain is complete");
                        return true;
                    }
                    Ok(control) => {
                        warn!(
                            status = %control.status,
                            active_jobs = control.active_jobs,
                            "Python worker is still draining before startup"
                        );
                    }
                    Err(error) => warn!(%error, "worker drain response was invalid"),
                }
            }
            Ok(response) => {
                warn!(status = %response.status(), "worker startup drain was rejected");
            }
            Err(error) => warn!(%error, "worker startup drain request failed"),
        }
        if started_at.elapsed() > state.0.config.cancel_reconcile_timeout {
            warn!("Python worker drain exceeded the reconciliation deadline");
            return false;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn resume_worker(state: &AppState) -> Result<(), BoxError> {
    let response = state
        .0
        .client
        .post(format!("{}/internal/resume", state.0.config.worker_url))
        .header("x-worker-token", &state.0.config.worker_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("worker resume was rejected with {}", response.status()).into());
    }
    let control = response.json::<WorkerControlResponse>().await?;
    if control.status != "ready" || control.active_jobs != 0 {
        return Err("worker did not become ready after startup drain".into());
    }
    info!("Python worker resumed after startup reconciliation");
    Ok(())
}

async fn finalize_confirmed_terminal(
    state: &AppState,
    entry: &Arc<JobEntry>,
    id: Uuid,
    terminal: WorkerTerminal,
    failure_message: &str,
    user_cancelled: bool,
) {
    match terminal {
        WorkerTerminal::Finished | WorkerTerminal::Failed => {
            match fetch_worker_terminal_event(state, id).await {
                Ok(event) => {
                    if !apply_worker_event(state, entry, event).await {
                        mark_failed(state, entry, "翻译服务返回了无效终态").await;
                    }
                }
                Err(error) if terminal == WorkerTerminal::Finished => {
                    warn!(%id, %error, "terminal event was unavailable; validating shared output files");
                    if let Ok(files) = confirm_materialized_outputs(state, entry, id).await {
                        update_job(state, entry, |snapshot| {
                            if snapshot.status.is_terminal() {
                                return;
                            }
                            snapshot.status = JobStatus::Succeeded;
                            snapshot.progress = 100.0;
                            snapshot.stage = "翻译完成".to_owned();
                            snapshot.message = "文件已经可以下载".to_owned();
                            snapshot.files = files;
                            snapshot.finished_at = Some(Utc::now());
                        })
                        .await;
                    } else {
                        let message = format!("{failure_message}；{error}；输出文件校验失败");
                        mark_failed(state, entry, &message).await;
                    }
                }
                Err(error) => {
                    let message = format!("{failure_message}；{error}");
                    mark_failed(state, entry, &message).await;
                }
            }
        }
        WorkerTerminal::Cancelled if user_cancelled => {
            mark_cancelled(state, entry, "翻译进程与临时资源已全部回收").await;
        }
        WorkerTerminal::Cancelled => {
            mark_failed(state, entry, failure_message).await;
        }
    }
}

async fn fetch_worker_terminal_event(state: &AppState, id: Uuid) -> Result<WorkerEvent, String> {
    let response = state
        .0
        .client
        .get(format!(
            "{}/internal/jobs/{id}/terminal",
            state.0.config.worker_url
        ))
        .header("x-worker-token", &state.0.config.worker_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| format!("无法读取翻译终态：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("翻译终态服务返回 {}", response.status()));
    }
    response
        .json::<WorkerEvent>()
        .await
        .map_err(|_| "翻译终态响应无效".to_owned())
}

async fn confirm_materialized_outputs(
    state: &AppState,
    _entry: &Arc<JobEntry>,
    id: Uuid,
) -> Result<JobFiles, String> {
    let data_root = state.0.config.data_root.clone();
    let artifact_root = state.0.config.state_root.join("artifacts");
    let files =
        tokio::task::spawn_blocking(move || promote_worker_outputs(&data_root, &artifact_root, id))
            .await
            .map_err(|_| "无法安全发布输出文件".to_owned())?
            .map_err(|error| {
                warn!(%id, %error, "worker output promotion was rejected");
                "输出文件安全校验失败".to_owned()
            })?;
    // The worker terminal journal stays in shared storage until the resulting
    // terminal snapshot has committed to SQLite. The replacement-worker scrub
    // removes it only after that durable commit, so a Rust crash can reconcile
    // this exact terminal event on restart.
    Ok(files)
}

#[cfg(target_os = "linux")]
fn promote_worker_outputs(
    data_root: &Path,
    artifact_root: &Path,
    id: Uuid,
) -> Result<JobFiles, BoxError> {
    use rustix::fs::{Mode, OFlags, RenameFlags, ResolveFlags};

    if let Some(files) = existing_published_artifacts(artifact_root, id)? {
        return Ok(files);
    }

    let root_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    let data_root_fd = rustix::fs::open(data_root, root_flags, Mode::empty())?;
    let output_fd = rustix::fs::openat2(
        &data_root_fd,
        format!("{id}/output"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )?;
    let artifact_root_fd = rustix::fs::open(artifact_root, root_flags, Mode::empty())?;
    let staging_name = format!(".{id}.{}.tmp", Uuid::new_v4());
    let final_name = id.to_string();
    let staging_path = artifact_root.join(&staging_name);
    let final_path = artifact_root.join(&final_name);
    rustix::fs::mkdirat(&artifact_root_fd, staging_name.as_str(), Mode::RWXU)?;
    let mut renamed = false;
    let result = (|| -> Result<JobFiles, BoxError> {
        let staging_fd = rustix::fs::openat2(
            &artifact_root_fd,
            staging_name.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            resolve,
        )?;
        let mut total_bytes = 0_u64;
        let mono = copy_worker_artifact(
            &output_fd,
            &staging_fd,
            "mono.pdf",
            true,
            MAX_PDF_ARTIFACT_BYTES,
            &mut total_bytes,
            resolve,
        )?;
        let dual = copy_worker_artifact(
            &output_fd,
            &staging_fd,
            "dual.pdf",
            true,
            MAX_PDF_ARTIFACT_BYTES,
            &mut total_bytes,
            resolve,
        )?;
        if !mono && !dual {
            return Err("worker produced no valid PDF artifact".into());
        }
        let glossary = copy_worker_artifact(
            &output_fd,
            &staging_fd,
            "glossary.csv",
            false,
            MAX_GLOSSARY_ARTIFACT_BYTES,
            &mut total_bytes,
            resolve,
        )?;
        rustix::fs::fsync(&staging_fd)?;
        rustix::fs::renameat_with(
            &artifact_root_fd,
            staging_name.as_str(),
            &artifact_root_fd,
            final_name.as_str(),
            RenameFlags::NOREPLACE,
        )?;
        renamed = true;
        rustix::fs::fsync(&artifact_root_fd)?;
        Ok(JobFiles {
            mono,
            dual,
            glossary,
        })
    })();
    if result.is_err() {
        let cleanup_path = if renamed { &final_path } else { &staging_path };
        let _ = std::fs::remove_dir_all(cleanup_path);
        let _ = rustix::fs::fsync(&artifact_root_fd);
    }
    result
}

#[cfg(target_os = "linux")]
fn existing_published_artifacts(
    artifact_root: &Path,
    id: Uuid,
) -> Result<Option<JobFiles>, BoxError> {
    let final_path = artifact_root.join(id.to_string());
    let metadata = match std::fs::symlink_metadata(&final_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("published artifact location is not a trusted directory".into());
    }

    let mut total_bytes = 0_u64;
    let mono = validate_existing_published_artifact(
        artifact_root,
        id,
        "mono.pdf",
        MAX_PDF_ARTIFACT_BYTES,
        &mut total_bytes,
    )?;
    let dual = validate_existing_published_artifact(
        artifact_root,
        id,
        "dual.pdf",
        MAX_PDF_ARTIFACT_BYTES,
        &mut total_bytes,
    )?;
    if !mono && !dual {
        return Err("published artifact directory contains no valid PDF".into());
    }
    let glossary = validate_existing_published_artifact(
        artifact_root,
        id,
        "glossary.csv",
        MAX_GLOSSARY_ARTIFACT_BYTES,
        &mut total_bytes,
    )?;
    Ok(Some(JobFiles {
        mono,
        dual,
        glossary,
    }))
}

#[cfg(target_os = "linux")]
fn validate_existing_published_artifact(
    artifact_root: &Path,
    id: Uuid,
    name: &str,
    max_bytes: u64,
    total_bytes: &mut u64,
) -> Result<bool, BoxError> {
    let path = artifact_root.join(id.to_string()).join(name);
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!("published artifact {name} is not a regular file").into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let (_, length) = open_published_artifact(artifact_root, id, name)?;
    account_artifact_bytes(name, length, max_bytes, total_bytes)?;
    Ok(true)
}

fn account_artifact_bytes(
    name: &str,
    length: u64,
    max_bytes: u64,
    total_bytes: &mut u64,
) -> Result<(), BoxError> {
    if length > max_bytes {
        return Err(format!("worker artifact {name} exceeds its size limit").into());
    }
    let next_total = total_bytes
        .checked_add(length)
        .ok_or("worker artifact size overflow")?;
    if next_total > MAX_TOTAL_ARTIFACT_BYTES {
        return Err("worker artifacts exceed the aggregate size limit".into());
    }
    *total_bytes = next_total;
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_worker_artifact(
    output_fd: &rustix::fd::OwnedFd,
    staging_fd: &rustix::fd::OwnedFd,
    name: &str,
    require_pdf_magic: bool,
    max_bytes: u64,
    total_bytes: &mut u64,
    resolve: rustix::fs::ResolveFlags,
) -> Result<bool, BoxError> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt;

    let source_fd = match rustix::fs::openat2(
        output_fd,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    ) {
        Ok(file) => file,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut source = std::fs::File::from(source_fd);
    let metadata = source.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("worker artifact {name} is not a non-empty regular file").into());
    }
    if metadata.nlink() != 1 {
        return Err(format!("worker artifact {name} has an unsafe hard-link count").into());
    }
    account_artifact_bytes(name, metadata.len(), max_bytes, total_bytes)?;
    if require_pdf_magic {
        if metadata.len() < 5 {
            return Err(format!("worker artifact {name} is too short").into());
        }
        let mut prefix = [0_u8; 5];
        std::io::Read::read_exact(&mut source, &mut prefix)?;
        if &prefix != b"%PDF-" {
            return Err(format!("worker artifact {name} is not a PDF").into());
        }
        std::io::Seek::rewind(&mut source)?;
    }
    let destination_fd = rustix::fs::openat2(
        staging_fd,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
        resolve,
    )?;
    let mut destination = std::fs::File::from(destination_fd);
    let mut limited = std::io::Read::take(&mut source, metadata.len());
    let copied = std::io::copy(&mut limited, &mut destination)?;
    if copied != metadata.len() {
        return Err(format!("worker artifact {name} changed while being copied").into());
    }
    let final_metadata = source.metadata()?;
    if final_metadata.len() != metadata.len()
        || final_metadata.nlink() != metadata.nlink()
        || final_metadata.mtime() != metadata.mtime()
        || final_metadata.mtime_nsec() != metadata.mtime_nsec()
        || final_metadata.ctime() != metadata.ctime()
        || final_metadata.ctime_nsec() != metadata.ctime_nsec()
    {
        return Err(format!("worker artifact {name} changed while being copied").into());
    }
    std::io::Write::flush(&mut destination)?;
    destination.sync_all()?;
    if require_pdf_magic {
        std::io::Seek::rewind(&mut destination)?;
        let mut prefix = [0_u8; 5];
        std::io::Read::read_exact(&mut destination, &mut prefix)?;
        if &prefix != b"%PDF-" {
            return Err(format!("published artifact {name} failed PDF validation").into());
        }
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
fn promote_worker_outputs(
    _data_root: &Path,
    _artifact_root: &Path,
    _id: Uuid,
) -> Result<JobFiles, BoxError> {
    Err("secure worker output promotion requires Linux openat2".into())
}

#[cfg(target_os = "linux")]
fn open_published_artifact(
    artifact_root: &Path,
    id: Uuid,
    name: &str,
) -> Result<(std::fs::File, u64), BoxError> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};

    let root_fd = rustix::fs::open(
        artifact_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    let file_fd = rustix::fs::openat2(
        &root_fd,
        format!("{id}/{name}"),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )?;
    let mut file = std::fs::File::from(file_fd);
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err("published artifact is not a non-empty regular file".into());
    }
    if name.ends_with(".pdf") {
        if metadata.len() < 5 {
            return Err("published PDF artifact is too short".into());
        }
        let mut prefix = [0_u8; 5];
        std::io::Read::read_exact(&mut file, &mut prefix)?;
        if &prefix != b"%PDF-" {
            return Err("published PDF artifact has invalid magic".into());
        }
        std::io::Seek::rewind(&mut file)?;
    }
    Ok((file, metadata.len()))
}

#[cfg(not(target_os = "linux"))]
fn open_published_artifact(
    _artifact_root: &Path,
    _id: Uuid,
    _name: &str,
) -> Result<(std::fs::File, u64), BoxError> {
    Err("secure artifact downloads require Linux openat2".into())
}

async fn published_artifacts_are_valid(state: &AppState, id: Uuid, files: &JobFiles) -> bool {
    if !files.mono && !files.dual {
        return false;
    }
    let artifact_root = state.0.config.state_root.join("artifacts");
    let files = files.clone();
    tokio::task::spawn_blocking(move || {
        (!files.mono || open_published_artifact(&artifact_root, id, "mono.pdf").is_ok())
            && (!files.dual || open_published_artifact(&artifact_root, id, "dual.pdf").is_ok())
            && (!files.glossary
                || open_published_artifact(&artifact_root, id, "glossary.csv").is_ok())
    })
    .await
    .unwrap_or(false)
}

async fn cleanup_failed_job(state: &AppState, entry: &Arc<JobEntry>, id: Uuid) {
    let final_status = entry.snapshot.read().await.status.clone();
    if matches!(final_status, JobStatus::Cancelled | JobStatus::Failed) {
        safe_remove_job_dir(state, id).await;
    }
}

async fn consume_worker_events(
    state: &AppState,
    entry: &Arc<JobEntry>,
    id: Uuid,
) -> Result<(), String> {
    let response = state
        .0
        .client
        .get(format!(
            "{}/internal/jobs/{id}/events",
            state.0.config.worker_url
        ))
        .header("x-worker-token", &state.0.config.worker_token)
        .send()
        .await
        .map_err(|error| format!("无法读取翻译进度：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("翻译进度服务返回 {}", response.status()));
    }

    let mut bytes = response.bytes_stream();
    let mut buffer = BytesMut::new();
    let mut saw_terminal = false;
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|error| format!("翻译进度连接中断：{error}"))?;
        buffer.extend_from_slice(&chunk);
        if buffer.len() > MAX_INTERNAL_EVENT_BYTES {
            return Err("翻译进度事件过大".to_owned());
        }
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.split_to(position + 1);
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            let event: WorkerEvent =
                serde_json::from_slice(line).map_err(|_| "翻译服务返回了无效进度".to_owned())?;
            saw_terminal = apply_worker_event(state, entry, event).await;
            if saw_terminal {
                break;
            }
        }
        if saw_terminal {
            break;
        }
    }
    if saw_terminal {
        Ok(())
    } else {
        Err("翻译服务提前断开，未确认资源清理完成".to_owned())
    }
}

async fn apply_worker_event(state: &AppState, entry: &Arc<JobEntry>, event: WorkerEvent) -> bool {
    match event.kind.as_str() {
        "progress" => {
            update_job(state, entry, |snapshot| {
                if snapshot.status != JobStatus::Cancelling {
                    snapshot.status = JobStatus::Running;
                    snapshot.progress =
                        event.progress.unwrap_or(snapshot.progress).clamp(0.0, 99.9);
                    snapshot.stage = "正在翻译".to_owned();
                    snapshot.message = "翻译处理中".to_owned();
                }
            })
            .await;
            false
        }
        "finished" => {
            let id = entry.snapshot.read().await.id;
            let files = match confirm_materialized_outputs(state, entry, id).await {
                Ok(files) => files,
                Err(error) => {
                    mark_failed(state, entry, &error).await;
                    return true;
                }
            };
            update_job(state, entry, |snapshot| {
                if snapshot.status.is_terminal() {
                    return;
                }
                snapshot.status = JobStatus::Succeeded;
                snapshot.progress = 100.0;
                snapshot.stage = "翻译完成".to_owned();
                snapshot.message = "文件已经可以下载".to_owned();
                snapshot.files = files;
                snapshot.finished_at = Some(Utc::now());
            })
            .await;
            true
        }
        "cancelled" => {
            mark_cancelled(state, entry, "翻译任务已安全取消").await;
            true
        }
        "failed" => {
            mark_failed(state, entry, event.message.as_deref().unwrap_or("翻译失败")).await;
            true
        }
        _ => false,
    }
}

async fn update_job<F>(state: &AppState, entry: &Arc<JobEntry>, update: F)
where
    F: FnOnce(&mut JobSnapshot),
{
    let committed = commit_job_update(state, entry, |snapshot| {
        update(snapshot);
        true
    })
    .await;
    let snapshot = match committed {
        Ok(snapshot) => snapshot,
        Err(error) => {
            error!(%error, "failed to durably persist job snapshot");
            // Continuing could erase the only worker terminal journal and
            // release the single-job permit while SQLite still says Running.
            fail_stop_coordinator("job snapshot could not be durably persisted")
        }
    };
    if snapshot.is_some_and(|snapshot| snapshot.status.is_terminal()) {
        clear_runtime_provider(entry);
    }
}

async fn commit_job_update<F>(
    state: &AppState,
    entry: &Arc<JobEntry>,
    update: F,
) -> Result<Option<JobSnapshot>, BoxError>
where
    F: FnOnce(&mut JobSnapshot) -> bool,
{
    let _publish_guard = entry.publish_lock.lock().await;
    if entry.disposed.load(AtomicOrdering::Acquire) {
        return Ok(None);
    }

    // Build a candidate without exposing it to request handlers or SSE. The
    // persisted row is the commit point; only then replace memory and publish.
    let mut candidate = entry.snapshot.read().await.clone();
    if !update(&mut candidate) {
        return Ok(None);
    }
    candidate.revision = candidate.revision.saturating_add(1);
    state.0.persistence.save(&candidate).await?;
    *entry.snapshot.write().await = candidate.clone();
    let _ = entry.sender.send(candidate.clone());
    Ok(Some(candidate))
}

fn release_job_admission(entry: &Arc<JobEntry>) {
    entry
        .admission_permit
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
}

fn clear_runtime_provider(entry: &Arc<JobEntry>) {
    entry
        .runtime_provider
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
}

async fn mark_cancelled(state: &AppState, entry: &Arc<JobEntry>, message: &str) {
    update_job(state, entry, |snapshot| {
        if snapshot.status.is_terminal() {
            return;
        }
        snapshot.status = JobStatus::Cancelled;
        snapshot.stage = "已取消".to_owned();
        snapshot.message = message.to_owned();
        snapshot.error = None;
        snapshot.finished_at = Some(Utc::now());
    })
    .await;
}

async fn mark_failed(state: &AppState, entry: &Arc<JobEntry>, message: &str) {
    let public_message = sanitize_public_error(message);
    update_job(state, entry, |snapshot| {
        if snapshot.status.is_terminal() {
            return;
        }
        snapshot.status = JobStatus::Failed;
        snapshot.stage = "翻译失败".to_owned();
        snapshot.message = "任务未完成，可以重新提交".to_owned();
        snapshot.error = Some(public_message);
        snapshot.finished_at = Some(Utc::now());
    })
    .await;
}

fn sanitize_public_error(message: &str) -> String {
    const GENERIC: &str = "翻译服务暂时无法完成任务，请稍后重试";
    match message.trim() {
        "翻译渠道认证失败，请联系管理员检查密钥"
        | "翻译渠道请求过于频繁或额度不足，请稍后重试"
        | "翻译渠道响应超时，请稍后重试"
        | "无法连接翻译渠道，请稍后重试"
        | "翻译渠道拒绝了请求，请联系管理员检查模型配置"
        | GENERIC => message.trim().to_owned(),
        _ => GENERIC.to_owned(),
    }
}

async fn request_expired_lease_cancellation(state: &AppState, entry: &Arc<JobEntry>) -> bool {
    let _publish_guard = entry.publish_lock.lock().await;
    if entry.disposed.load(AtomicOrdering::Acquire) {
        return false;
    }
    let (expired, release_expired) = {
        let lease = entry.lease.lock().await;
        let release_expired = lease
            .released_at
            .as_ref()
            .is_some_and(|released_at| released_at.elapsed() > state.0.config.release_grace);
        (
            lease.last_heartbeat.elapsed() > state.0.config.lease_timeout || release_expired,
            release_expired,
        )
    };
    if !expired {
        return false;
    }
    let mut snapshot = entry.snapshot.read().await.clone();
    if !snapshot.status.is_active() || entry.cancellation.is_cancelled() {
        return false;
    }
    snapshot.status = JobStatus::Cancelling;
    snapshot.stage = "正在取消".to_owned();
    snapshot.message = if release_expired {
        "页面离开后未恢复，正在回收任务".to_owned()
    } else {
        "页面连接已失效，正在回收任务".to_owned()
    };
    snapshot.revision = snapshot.revision.saturating_add(1);
    info!(id = %snapshot.id, release_expired, "job lease expired; requesting cancellation");
    if let Err(error) = state.0.persistence.save(&snapshot).await {
        error!(id = %snapshot.id, %error, "failed to persist lease-expired cancellation");
        fail_stop_coordinator("lease cancellation could not be durably persisted");
    }
    *entry.snapshot.write().await = snapshot.clone();
    entry.cancellation.cancel();
    let _ = entry.sender.send(snapshot);
    true
}

impl AppState {
    async fn maintenance_loop(self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = self.0.shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            if let Err(error) = self.0.persistence.purge_expired_tombstones().await {
                warn!(%error, "failed to purge expired job id tombstones");
            }
            let entries = self
                .0
                .jobs
                .read()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for entry in entries {
                let snapshot = entry.snapshot.read().await.clone();
                if snapshot.status.is_active()
                    && request_expired_lease_cancellation(&self, &entry).await
                {
                    continue;
                }
                if snapshot.status.is_terminal() {
                    let publish_guard = entry.publish_lock.lock().await;
                    if entry.disposed.load(AtomicOrdering::Acquire) {
                        continue;
                    }
                    let snapshot = entry.snapshot.read().await.clone();
                    let lease = entry.lease.lock().await;
                    let idle_for = lease.last_heartbeat.elapsed();
                    let release_expired = lease.released_at.as_ref().is_some_and(|released_at| {
                        released_at.elapsed() > self.0.config.release_grace
                    });
                    let terminal_age = snapshot
                        .finished_at
                        .and_then(|finished_at| {
                            Utc::now().signed_duration_since(finished_at).to_std().ok()
                        })
                        .unwrap_or(idle_for);
                    if !snapshot.status.is_terminal()
                        || (terminal_age <= self.0.config.terminal_retention && !release_expired)
                    {
                        continue;
                    }
                    drop(lease);
                    let id = snapshot.id;
                    if let Err(error) = self
                        .0
                        .persistence
                        .retire(id, self.0.config.id_tombstone_retention)
                        .await
                    {
                        warn!(%id, %error, "failed to remove expired job record");
                        continue;
                    }
                    entry.disposed.store(true, AtomicOrdering::Release);
                    if let Err(error) = remove_job_files(&self, id).await {
                        warn!(%id, %error, "failed to expire job files; restoring tracking record");
                        entry.disposed.store(false, AtomicOrdering::Release);
                        if let Err(save_error) = self.0.persistence.save(&snapshot).await {
                            error!(%id, %save_error, "failed to restore job tracking after file cleanup error");
                        }
                        continue;
                    }
                    self.0.jobs.write().await.remove(&id);
                    drop(publish_guard);
                }
            }
        }
    }
}

async fn safe_remove_job_dir(state: &AppState, id: Uuid) {
    if let Err(error) = remove_job_files(state, id).await {
        warn!(%id, %error, "failed to remove job directory");
    }
}

async fn purge_shared_job_root(state: &AppState) -> Result<(), BoxError> {
    let root = state.0.config.data_root.clone();
    tokio::task::spawn_blocking(move || purge_shared_job_root_sync(&root))
        .await
        .map_err(|error| format!("shared storage scrub task failed: {error}"))??;
    Ok(())
}

fn purge_shared_job_root_sync(root: &Path) -> Result<(), BoxError> {
    let root_metadata = std::fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("shared job root is not a trusted directory".into());
    }

    // This runs only after the replacement worker has been drained, so there
    // is no legitimate concurrent writer. A hostile old worker may have left
    // mode-000 directories; restore owner traversal top-down without following
    // symlinks, then remove every entry from the trusted root.
    make_shared_tree_owner_accessible(root)?;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && !file_type.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    if std::fs::read_dir(root)?.next().transpose()?.is_some() {
        return Err("shared job root was repopulated during isolation scrub".into());
    }
    Ok(())
}

fn make_shared_tree_owner_accessible(root: &Path) -> Result<(), BoxError> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let metadata = std::fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("shared storage directory changed type during scrub".into());
        }
        #[cfg(unix)]
        {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o700);
            std::fs::set_permissions(&directory, permissions)?;
        }
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() && !file_type.is_symlink() {
                directories.push(entry.path());
            }
        }
    }
    Ok(())
}

async fn cleanup_orphan_job_dirs(state: &AppState) {
    let known_ids = state
        .0
        .jobs
        .read()
        .await
        .keys()
        .copied()
        .collect::<HashSet<_>>();
    let mut entries = match fs::read_dir(&state.0.config.data_root).await {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, "failed to scan job storage for orphan directories");
            return;
        }
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "failed while scanning orphan job directories");
                break;
            }
        };
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&name) else {
            continue;
        };
        if id.to_string() == name && !known_ids.contains(&id) {
            warn!(%id, "removing an untracked job directory after worker drain");
            safe_remove_job_dir(state, id).await;
        }
    }
}

async fn cleanup_orphan_artifact_dirs(state: &AppState) {
    let entries = state
        .0
        .jobs
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut succeeded_ids = HashSet::new();
    for entry in entries {
        let snapshot = entry.snapshot.read().await;
        if snapshot.status == JobStatus::Succeeded {
            succeeded_ids.insert(snapshot.id);
        }
    }
    let artifact_root = state.0.config.state_root.join("artifacts");
    let mut entries = match fs::read_dir(&artifact_root).await {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, "failed to scan published artifact storage");
            return;
        }
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "failed while scanning published artifact storage");
                break;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let retained = Uuid::parse_str(&name)
            .ok()
            .is_some_and(|id| id.to_string() == name && succeeded_ids.contains(&id));
        if retained {
            continue;
        }
        let path = entry.path();
        let result = match entry.file_type().await {
            Ok(file_type) if file_type.is_dir() => fs::remove_dir_all(&path).await,
            Ok(_) => fs::remove_file(&path).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            warn!(path = %path.display(), %error, "failed to remove orphan published artifact entry");
        }
    }
}

async fn cleanup_orphan_private_uploads(state: &AppState) {
    let upload_root = state.0.config.state_root.join("uploads");
    let mut entries = match fs::read_dir(&upload_root).await {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, "failed to scan private upload storage");
            return;
        }
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "failed while scanning private upload storage");
                break;
            }
        };
        let path = entry.path();
        let result = match entry.file_type().await {
            Ok(file_type) if file_type.is_dir() => fs::remove_dir_all(&path).await,
            Ok(_) => fs::remove_file(&path).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            warn!(path = %path.display(), %error, "failed to remove orphan private upload entry");
        }
    }
}

async fn remove_job_files(state: &AppState, id: Uuid) -> Result<(), BoxError> {
    let upload_result = remove_private_upload(state, id).await;
    let worker_result = remove_worker_job_files(state, id).await;
    let artifact_result = remove_published_artifacts(state, id).await;
    upload_result?;
    worker_result?;
    artifact_result
}

async fn remove_private_upload(state: &AppState, id: Uuid) -> Result<(), BoxError> {
    let root = state.0.config.state_root.join("uploads");
    let path = root.join(id.to_string());
    if path.parent() != Some(root.as_path()) {
        return Err("refusing to remove an unsafe private upload path".into());
    }
    match fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn remove_worker_job_files(state: &AppState, id: Uuid) -> Result<(), BoxError> {
    let path = state.0.config.data_root.join(id.to_string());
    if !path_belongs_to_job(state, id, &path) {
        return Err("refusing to remove an unsafe job path".into());
    }
    match fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn remove_published_artifacts(state: &AppState, id: Uuid) -> Result<(), BoxError> {
    let root = state.0.config.state_root.join("artifacts");
    let path = root.join(id.to_string());
    if path.parent() != Some(root.as_path()) {
        return Err("refusing to remove an unsafe artifact path".into());
    }
    match fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn path_belongs_to_job(state: &AppState, id: Uuid, path: &Path) -> bool {
    let expected = state.0.config.data_root.join(id.to_string());
    path == expected || path.starts_with(&expected)
}

fn internal_error(error: impl std::fmt::Display) -> ApiError {
    error!(%error, "internal request error");
    ApiError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool as TestAtomicBool, AtomicUsize, Ordering};

    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    static MOCK_WORKER_EPOCH: AtomicUsize = AtomicUsize::new(1);

    #[test]
    fn page_ranges_are_normalized_and_validated() {
        assert_eq!(
            normalize_pages(Some(" 1, 3-5 ")).unwrap(),
            Some("1,3-5".to_owned())
        );
        assert!(normalize_pages(Some("0")).is_err());
        assert!(normalize_pages(Some("8-3")).is_err());
        assert!(normalize_pages(Some("1,,2")).is_err());
        assert_eq!(normalize_pages(Some(" ")).unwrap(), None);
    }

    #[test]
    fn filenames_cannot_escape_the_job_directory() {
        assert_eq!(sanitize_filename("../../paper.pdf"), ".._.._paper.pdf");
        assert_eq!(sanitize_filename("notes.txt"), "document.pdf");
        assert_eq!(sanitize_filename("论文.pdf"), "论文.pdf");
    }

    #[test]
    fn public_job_errors_are_strictly_allow_listed() {
        let allowed = "翻译渠道认证失败，请联系管理员检查密钥";
        assert_eq!(sanitize_public_error(allowed), allowed);
        assert_eq!(
            sanitize_public_error("upstream leaked response body: secret-token"),
            "翻译服务暂时无法完成任务，请稍后重试"
        );
        assert_eq!(
            sanitize_public_error("翻译渠道认证失败，请联系管理员检查密钥: detail"),
            "翻译服务暂时无法完成任务，请稍后重试"
        );
    }

    #[tokio::test]
    async fn worker_client_does_not_follow_redirects() {
        let app = Router::new()
            .route(
                "/redirect",
                get(|| async { axum::response::Redirect::temporary("/target") }),
            )
            .route("/target", get(|| async { StatusCode::OK }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let response = build_worker_client()
            .unwrap()
            .get(format!("http://{address}/redirect"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        server.abort();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn regular_worker_outputs_are_promoted_to_private_artifact_storage() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let artifact_root = temp.path().join("state/artifacts");
        std::fs::create_dir_all(&data_root).unwrap();
        std::fs::create_dir_all(&artifact_root).unwrap();
        let id = Uuid::new_v4();
        let job_dir = data_root.join(id.to_string());
        let output_dir = job_dir.join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("mono.pdf"), b"%PDF-result").unwrap();

        let files = promote_worker_outputs(&data_root, &artifact_root, id).unwrap();
        assert!(files.mono);
        let (mut published, length) =
            open_published_artifact(&artifact_root, id, "mono.pdf").unwrap();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut published, &mut body).unwrap();
        assert_eq!(length, body.len() as u64);
        assert_eq!(body, b"%PDF-result");

        // A crash after the private-directory rename but before the SQLite
        // terminal commit must be recoverable without touching shared input.
        let recovered = promote_worker_outputs(&data_root, &artifact_root, id).unwrap();
        assert!(recovered.mono);
        assert!(!recovered.dual);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_output_symlink_cannot_publish_or_download_an_external_secret() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let artifact_root = temp.path().join("state/artifacts");
        let id = Uuid::new_v4();
        let output_dir = data_root.join(id.to_string()).join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::create_dir_all(&artifact_root).unwrap();
        std::fs::write(output_dir.join("mono.pdf"), b"%PDF-result").unwrap();
        let external_secret = temp.path().join("master-key");
        std::fs::write(&external_secret, b"super-secret-master-key").unwrap();
        symlink(&external_secret, output_dir.join("glossary.csv")).unwrap();

        assert!(promote_worker_outputs(&data_root, &artifact_root, id).is_err());
        assert!(!artifact_root.join(id.to_string()).exists());
        assert!(open_published_artifact(&artifact_root, id, "glossary.csv").is_err());
        assert_eq!(
            std::fs::read(&external_secret).unwrap(),
            b"super-secret-master-key"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_output_fifo_is_rejected_without_blocking() {
        use rustix::fs::{Mode, OFlags};
        use std::sync::mpsc;

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let artifact_root = temp.path().join("state/artifacts");
        let id = Uuid::new_v4();
        let output_dir = data_root.join(id.to_string()).join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::create_dir_all(&artifact_root).unwrap();
        let output_fd = rustix::fs::open(
            &output_dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        rustix::fs::mkfifoat(&output_fd, "mono.pdf", Mode::RUSR | Mode::WUSR).unwrap();

        let (sender, receiver) = mpsc::channel();
        let worker_data_root = data_root.clone();
        let worker_artifact_root = artifact_root.clone();
        std::thread::spawn(move || {
            let result = promote_worker_outputs(&worker_data_root, &worker_artifact_root, id);
            let _ = sender.send(result.is_err());
        });
        assert!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            "opening a hostile FIFO blocked artifact promotion"
        );
        assert!(!artifact_root.join(id.to_string()).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn oversized_sparse_and_hardlinked_worker_outputs_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let artifact_root = temp.path().join("state/artifacts");
        std::fs::create_dir_all(&artifact_root).unwrap();

        let sparse_id = Uuid::new_v4();
        let sparse_output = data_root.join(sparse_id.to_string()).join("output");
        std::fs::create_dir_all(&sparse_output).unwrap();
        let sparse = std::fs::File::create(sparse_output.join("mono.pdf")).unwrap();
        sparse.set_len(MAX_PDF_ARTIFACT_BYTES + 1).unwrap();
        drop(sparse);
        assert!(promote_worker_outputs(&data_root, &artifact_root, sparse_id).is_err());
        assert!(!artifact_root.join(sparse_id.to_string()).exists());

        let hardlink_id = Uuid::new_v4();
        let hardlink_output = data_root.join(hardlink_id.to_string()).join("output");
        std::fs::create_dir_all(&hardlink_output).unwrap();
        let external = temp.path().join("external.pdf");
        std::fs::write(&external, b"%PDF-external").unwrap();
        std::fs::hard_link(&external, hardlink_output.join("mono.pdf")).unwrap();
        assert!(promote_worker_outputs(&data_root, &artifact_root, hardlink_id).is_err());
        assert!(!artifact_root.join(hardlink_id.to_string()).exists());
        assert_eq!(std::fs::read(external).unwrap(), b"%PDF-external");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_storage_scrub_repairs_mode_zero_trees_without_following_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("jobs");
        let nested = root.join("hostile/deep");
        let external = temp.path().join("outside-secret");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("payload"), b"worker-data").unwrap();
        std::fs::write(&external, b"must-survive").unwrap();
        symlink(&external, nested.join("escape")).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(root.join("hostile"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();

        purge_shared_job_root_sync(&root).unwrap();
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        assert_eq!(std::fs::read(external).unwrap(), b"must-survive");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_upload_symlinks_are_never_followed_or_truncated() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let id = Uuid::new_v4();
        let job_dir = data_root.join(id.to_string());
        std::fs::create_dir_all(&job_dir).unwrap();
        let external_secret = temp.path().join("state.sqlite3");
        std::fs::write(&external_secret, b"must-not-be-truncated").unwrap();
        symlink(&external_secret, job_dir.join("input.pdf")).unwrap();
        symlink(&external_secret, job_dir.join("input.partial")).unwrap();

        let (mut upload, job_dir_fd, staging_name) =
            create_secure_upload_file(&data_root, id).unwrap();
        std::io::Write::write_all(&mut upload, b"%PDF-new-upload").unwrap();
        upload.sync_all().unwrap();
        assert!(publish_secure_upload_file(upload, job_dir_fd, staging_name).is_err());
        assert_eq!(
            std::fs::read(&external_secret).unwrap(),
            b"must-not-be-truncated"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_upload_is_materialized_once_into_a_fresh_shared_directory() {
        let temp = tempfile::tempdir().unwrap();
        let upload_root = temp.path().join("state/uploads");
        let data_root = temp.path().join("jobs");
        let id = Uuid::new_v4();
        std::fs::create_dir_all(upload_root.join(id.to_string())).unwrap();
        std::fs::create_dir_all(&data_root).unwrap();
        std::fs::write(
            upload_root.join(id.to_string()).join("input.pdf"),
            b"%PDF-private-upload",
        )
        .unwrap();

        materialize_worker_input(&upload_root, &data_root, id).unwrap();
        assert_eq!(
            std::fs::read(data_root.join(id.to_string()).join("input.pdf")).unwrap(),
            b"%PDF-private-upload"
        );
        assert!(materialize_worker_input(&upload_root, &data_root, id).is_err());
    }

    #[test]
    fn terminal_cleanup_drops_the_queued_sealed_provider_snapshot() {
        let id = Uuid::new_v4();
        let entry = JobEntry::new_with_runtime(
            JobSnapshot::queued(
                id,
                "paper.pdf".to_owned(),
                JobOptions {
                    source_lang: "auto".to_owned(),
                    target_lang: "zh".to_owned(),
                    pages: None,
                    output_mono: true,
                    output_dual: false,
                    provider: TranslationProvider::OpenAiCompatible,
                    deepseek_model: None,
                    deepseek_thinking_mode: None,
                    deepseek_reasoning_effort: None,
                    provider_id: "provider-a".to_owned(),
                    provider_revision: Some(1),
                    model_id: Some("model-a".to_owned()),
                },
            ),
            PathBuf::from("unused-test-job"),
            None,
            Some(admin::RuntimeProvider {
                provider_id: "provider-a".to_owned(),
                provider_name: "Provider A".to_owned(),
                kind: "openai_compatible".to_owned(),
                revision: 1,
                model: "model-a".to_owned(),
                base_url: "https://api.example.test/v1".to_owned(),
                sealed_api_key_ciphertext: vec![1, 2, 3],
                sealed_api_key_nonce: vec![0; 24],
                sealed_key_id: "primary-v1",
            }),
        );
        assert!(
            entry
                .runtime_provider
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
        );
        clear_runtime_provider(&entry);
        assert!(
            entry
                .runtime_provider
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
        );
    }

    #[test]
    fn upload_options_are_google_compatible() {
        let upload = PendingUpload {
            source_lang: Some("auto".to_owned()),
            target_lang: Some("zh".to_owned()),
            output_mono: Some(true),
            output_dual: Some(false),
            ..PendingUpload::default()
        };
        let options = validate_upload_options(&upload).unwrap();
        assert_eq!(options.source_lang, "auto");
        assert_eq!(options.target_lang, "zh");
        assert_eq!(options.provider, TranslationProvider::Google);
        assert_eq!(options.deepseek_model, None);
        assert_eq!(options.deepseek_thinking_mode, None);
        assert_eq!(options.deepseek_reasoning_effort, None);

        let invalid = PendingUpload {
            source_lang: Some("en".to_owned()),
            target_lang: Some("en".to_owned()),
            ..PendingUpload::default()
        };
        assert!(validate_upload_options(&invalid).is_err());
    }

    #[test]
    fn deepseek_upload_options_are_strictly_validated_and_normalized() {
        let defaults = validate_upload_options(&PendingUpload {
            provider: Some("deepseek".to_owned()),
            ..PendingUpload::default()
        })
        .unwrap();
        assert_eq!(defaults.provider, TranslationProvider::DeepSeek);
        assert_eq!(defaults.deepseek_model, Some(DeepSeekModel::V4Flash));
        assert_eq!(defaults.deepseek_thinking_mode, None);
        assert_eq!(defaults.deepseek_reasoning_effort, None);

        let enabled_defaults = validate_upload_options(&PendingUpload {
            provider: Some("deepseek".to_owned()),
            deepseek_model: Some(DEEPSEEK_MODEL_PRO.to_owned()),
            deepseek_thinking_mode: Some("enabled".to_owned()),
            ..PendingUpload::default()
        })
        .unwrap();
        assert_eq!(enabled_defaults.deepseek_model, Some(DeepSeekModel::V4Pro));
        assert_eq!(
            enabled_defaults.deepseek_thinking_mode,
            Some(DeepSeekThinkingMode::Enabled)
        );
        assert_eq!(
            enabled_defaults.deepseek_reasoning_effort,
            Some(DeepSeekReasoningEffort::High)
        );
        let disabled = validate_upload_options(&PendingUpload {
            provider: Some("deepseek".to_owned()),
            deepseek_thinking_mode: Some("disabled".to_owned()),
            ..PendingUpload::default()
        })
        .unwrap();
        assert_eq!(
            disabled.deepseek_thinking_mode,
            Some(DeepSeekThinkingMode::Disabled)
        );
        assert_eq!(disabled.deepseek_reasoning_effort, None);
        let worker_payload = serde_json::to_value(WorkerRequest {
            input_path: "/data/input.pdf".to_owned(),
            output_dir: "/data/output".to_owned(),
            source_lang: enabled_defaults.source_lang.clone(),
            target_lang: enabled_defaults.target_lang.clone(),
            pages: enabled_defaults.pages.clone(),
            output_mono: enabled_defaults.output_mono,
            output_dual: enabled_defaults.output_dual,
            provider: enabled_defaults.provider,
            model: enabled_defaults.model_id.clone(),
            thinking_mode: enabled_defaults.deepseek_thinking_mode,
            reasoning_effort: enabled_defaults.deepseek_reasoning_effort,
            provider_name: Some("DeepSeek".to_owned()),
            openai_base_url: None,
            openai_api_key: Some(Zeroizing::new("test-secret".to_owned())),
        })
        .unwrap();
        assert_eq!(worker_payload["provider"], "deepseek");
        assert_eq!(worker_payload["model"], DEEPSEEK_MODEL_PRO);
        assert_eq!(worker_payload["thinking_mode"], "enabled");
        assert_eq!(worker_payload["reasoning_effort"], "high");
        assert_eq!(worker_payload["openai_base_url"], serde_json::Value::Null);
        assert!(worker_payload.get("deepseek_model").is_none());
        assert!(worker_payload.get("deepseek_thinking_mode").is_none());
        assert!(worker_payload.get("deepseek_reasoning_effort").is_none());

        for (effort, expected) in [
            ("high", DeepSeekReasoningEffort::High),
            ("max", DeepSeekReasoningEffort::Max),
        ] {
            let options = validate_upload_options(&PendingUpload {
                provider: Some("deepseek".to_owned()),
                deepseek_thinking_mode: Some("enabled".to_owned()),
                deepseek_reasoning_effort: Some(effort.to_owned()),
                ..PendingUpload::default()
            })
            .unwrap();
            assert_eq!(options.deepseek_reasoning_effort, Some(expected));
        }

        for invalid in [
            PendingUpload {
                provider: Some("deepseek".to_owned()),
                deepseek_thinking_mode: Some("enabled".to_owned()),
                deepseek_reasoning_effort: Some("low".to_owned()),
                ..PendingUpload::default()
            },
            PendingUpload {
                provider: Some("deepseek".to_owned()),
                deepseek_model: Some("deepseek-v5".to_owned()),
                ..PendingUpload::default()
            },
            PendingUpload {
                provider: Some("deepseek".to_owned()),
                deepseek_thinking_mode: Some("auto".to_owned()),
                ..PendingUpload::default()
            },
            PendingUpload {
                provider: Some("deepseek".to_owned()),
                deepseek_reasoning_effort: Some("high".to_owned()),
                ..PendingUpload::default()
            },
            PendingUpload {
                provider: Some("deepseek".to_owned()),
                deepseek_thinking_mode: Some("disabled".to_owned()),
                deepseek_reasoning_effort: Some("max".to_owned()),
                ..PendingUpload::default()
            },
            PendingUpload {
                provider: Some("google".to_owned()),
                deepseek_model: Some(DEEPSEEK_MODEL_FLASH.to_owned()),
                ..PendingUpload::default()
            },
            PendingUpload {
                provider: Some("unknown".to_owned()),
                ..PendingUpload::default()
            },
        ] {
            assert!(validate_upload_options(&invalid).is_err());
        }
    }

    #[test]
    fn generic_provider_upload_stores_only_identity_revision_and_model() {
        let options = validate_upload_options(&PendingUpload {
            provider_id: Some("11111111-1111-4111-8111-111111111111".to_owned()),
            model_id: Some("example/model".to_owned()),
            ..PendingUpload::default()
        })
        .unwrap();
        assert_eq!(options.provider, TranslationProvider::OpenAiCompatible);
        assert_eq!(options.model_id.as_deref(), Some("example/model"));
        assert_eq!(options.provider_revision, None);
        let json = serde_json::to_string(&options).unwrap();
        assert!(!json.contains("api_key"));
        assert!(!json.contains("apiKey"));
        assert!(!json.contains("base_url"));
        assert!(!json.contains("baseUrl"));
    }

    #[test]
    fn legacy_persisted_snapshot_defaults_to_google() {
        let snapshot = JobSnapshot::queued(
            Uuid::new_v4(),
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        let mut persisted = serde_json::to_value(snapshot).unwrap();
        let persisted_options = persisted
            .get_mut("options")
            .unwrap()
            .as_object_mut()
            .unwrap();
        persisted_options.remove("provider");
        persisted_options.remove("deepseekModel");
        persisted_options.remove("deepseekThinkingMode");
        persisted_options.remove("deepseekReasoningEffort");
        persisted_options.remove("providerId");
        persisted_options.remove("providerRevision");
        persisted_options.remove("modelId");

        let restored: JobSnapshot = serde_json::from_value(persisted).unwrap();
        assert_eq!(restored.options.provider, TranslationProvider::Google);
        assert_eq!(restored.options.deepseek_model, None);
        assert_eq!(restored.options.deepseek_thinking_mode, None);
        assert_eq!(restored.options.deepseek_reasoning_effort, None);
    }

    #[test]
    fn public_config_omits_anonymous_csrf_and_exposes_authenticated_csrf() {
        let anonymous = serde_json::to_value(PublicConfig {
            provider: "google",
            providers: Vec::new(),
            languages: Vec::new(),
            max_upload_bytes: 1024,
            csrf_token: None,
        })
        .unwrap();
        assert!(anonymous.get("csrfToken").is_none());

        let authenticated = serde_json::to_value(PublicConfig {
            provider: "google",
            providers: Vec::new(),
            languages: Vec::new(),
            max_upload_bytes: 1024,
            csrf_token: Some("csrf-value".to_owned()),
        })
        .unwrap();
        assert_eq!(
            authenticated
                .get("csrfToken")
                .and_then(|value| value.as_str()),
            Some("csrf-value")
        );
    }

    #[test]
    fn worker_health_capabilities_drive_public_provider_config() {
        let legacy: WorkerHealth = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "provider": "google",
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": "legacy",
            "container_boot_id": "legacy-boot",
            "max_active_jobs": 1
        }))
        .unwrap();
        assert!(legacy.is_ready(1));
        assert!(legacy.supports_provider("google"));
        assert!(!legacy.supports_provider("deepseek"));

        let capable: WorkerHealth = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "providers": ["google", "deepseek", "openai_compatible"],
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": "capable",
            "container_boot_id": "capable-boot",
            "max_active_jobs": 1
        }))
        .unwrap();
        assert!(capable.is_ready(1));
        assert!(capable.supports_provider("google"));
        assert!(capable.supports_provider("deepseek"));
        let prior_identity = capable.identity();
        let only_epoch_changed: WorkerHealth = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "providers": ["google"],
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": "replacement-epoch",
            "container_boot_id": "capable-boot",
            "max_active_jobs": 1
        }))
        .unwrap();
        assert!(!only_epoch_changed.is_replacement_for(&prior_identity));
        let unsafe_capacity: WorkerHealth = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "providers": ["google"],
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": "replacement-epoch",
            "container_boot_id": "replacement-boot",
            "max_active_jobs": 4
        }))
        .unwrap();
        assert!(unsafe_capacity.is_replacement_for(&prior_identity));
        assert!(!unsafe_capacity.is_ready(1));
        let anonymous = public_provider_descriptors(Some(&capable), Vec::new());
        assert_eq!(anonymous.len(), 1);
        assert_eq!(anonymous[0].id, "google");
        let configured = vec![admin::PublicProviderConfig {
            id: "deepseek".to_owned(),
            kind: "deepseek".to_owned(),
            name: "DeepSeek".to_owned(),
            available: true,
            unavailable_reason: None,
            models: vec![
                admin::PublicProviderModel {
                    id: DEEPSEEK_MODEL_FLASH.to_owned(),
                    name: "DeepSeek V4 Flash".to_owned(),
                },
                admin::PublicProviderModel {
                    id: DEEPSEEK_MODEL_PRO.to_owned(),
                    name: "DeepSeek V4 Pro".to_owned(),
                },
            ],
            default_model: Some(DEEPSEEK_MODEL_FLASH.to_owned()),
            thinking_modes: vec!["enabled".to_owned(), "disabled".to_owned()],
            reasoning_efforts: vec!["high".to_owned(), "max".to_owned()],
            default_reasoning_effort: Some("high".to_owned()),
        }];
        let descriptors = public_provider_descriptors(Some(&capable), configured.clone());
        assert_eq!(descriptors.len(), 2);
        assert_eq!(descriptors[0].id, "google");
        assert!(descriptors[0].available);
        assert!(descriptors[0].models.is_empty());
        let deepseek = &descriptors[1];
        assert_eq!(deepseek.id, "deepseek");
        assert!(deepseek.available);
        assert_eq!(deepseek.unavailable_reason, None);
        assert_eq!(
            deepseek.default_model.as_deref(),
            Some(DEEPSEEK_MODEL_FLASH)
        );
        assert_eq!(
            deepseek
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![DEEPSEEK_MODEL_FLASH, DEEPSEEK_MODEL_PRO]
        );
        assert_eq!(deepseek.reasoning_efforts, vec!["high", "max"]);
        assert_eq!(deepseek.default_reasoning_effort.as_deref(), Some("high"));

        let draining: WorkerHealth = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "providers": ["google", "deepseek", "openai_compatible"],
            "active_jobs": 1,
            "accepting_jobs": false,
            "epoch": "draining",
            "container_boot_id": "draining-boot",
            "max_active_jobs": 1
        }))
        .unwrap();
        assert!(!draining.is_ready(1));
        assert!(
            public_provider_descriptors(Some(&draining), configured.clone())
                .iter()
                .all(|provider| provider.available)
        );

        let unavailable = public_provider_descriptors(None, configured.clone());
        assert!(unavailable.iter().all(|provider| !provider.available));
        assert!(
            unavailable.iter().all(
                |provider| provider.unavailable_reason.as_deref() == Some("worker_unavailable")
            )
        );
        let mut unconfigured = configured;
        unconfigured[0].available = false;
        unconfigured[0].unavailable_reason = Some("not_configured".to_owned());
        let legacy_descriptors = public_provider_descriptors(Some(&capable), unconfigured);
        assert_eq!(
            legacy_descriptors[1].unavailable_reason.as_deref(),
            Some("not_configured")
        );
        let public_config = serde_json::to_value(PublicConfig {
            provider: "google",
            providers: descriptors,
            languages: Vec::new(),
            max_upload_bytes: 1024,
            csrf_token: None,
        })
        .unwrap();
        assert_eq!(public_config["provider"], "google");
        assert_eq!(
            public_config["providers"][1]["defaultModel"],
            DEEPSEEK_MODEL_FLASH
        );
        assert_eq!(
            public_config["providers"][1]["defaultReasoningEffort"],
            "high"
        );
        let public_json = public_config.to_string();
        assert!(!public_json.contains("token"));
        assert!(!public_json.contains("secret"));
        assert!(!public_json.contains("apiKey"));
    }

    #[test]
    fn terminal_states_are_explicit() {
        assert!(!JobStatus::Cancelling.is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::Succeeded.is_terminal());
    }

    #[tokio::test]
    async fn corrupt_persisted_rows_do_not_block_valid_job_restore() {
        let temp = tempfile::tempdir().unwrap();
        let persistence = Persistence::open(temp.path()).await.unwrap();
        let good_id = Uuid::new_v4();
        let good = JobSnapshot::queued(
            good_id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        persistence.save(&good).await.unwrap();
        let corrupt_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO jobs (id, snapshot_json, updated_at) VALUES (?, ?, ?)")
            .bind(&corrupt_id)
            .bind("{not-json")
            .bind(Utc::now().to_rfc3339())
            .execute(&persistence.pool)
            .await
            .unwrap();
        let mismatched_row_id = Uuid::new_v4();
        sqlx::query("INSERT INTO jobs (id, snapshot_json, updated_at) VALUES (?, ?, ?)")
            .bind(mismatched_row_id.to_string())
            .bind(serde_json::to_string(&good).unwrap())
            .bind(Utc::now().to_rfc3339())
            .execute(&persistence.pool)
            .await
            .unwrap();
        let invalid_snapshot_id = Uuid::new_v4();
        sqlx::query("INSERT INTO jobs (id, snapshot_json, updated_at) VALUES (?, ?, ?)")
            .bind(invalid_snapshot_id.to_string())
            .bind(vec![0xff_u8, 0xfe])
            .bind(Utc::now().to_rfc3339())
            .execute(&persistence.pool)
            .await
            .unwrap();

        let loaded = persistence.load().await.unwrap();

        assert_eq!(loaded.snapshots.len(), 1);
        assert_eq!(loaded.snapshots[0].id, good_id);
        assert_eq!(loaded.corrupt_ids.len(), 3);
        assert!(!loaded.corrupt_ids.contains(&good_id));
        assert!(loaded.corrupt_ids.contains(&mismatched_row_id));
        assert!(loaded.corrupt_ids.contains(&invalid_snapshot_id));
        let corrupt_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE id = ?")
            .bind(corrupt_id)
            .fetch_one(&persistence.pool)
            .await
            .unwrap();
        assert_eq!(corrupt_rows, 0);
    }

    #[tokio::test]
    async fn retired_job_ids_are_tombstoned_before_state_is_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let persistence = Persistence::open(temp.path()).await.unwrap();
        let id = Uuid::new_v4();
        let snapshot = JobSnapshot::queued(
            id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        persistence.save(&snapshot).await.unwrap();

        persistence
            .retire(id, Duration::from_secs(60))
            .await
            .unwrap();

        assert!(persistence.is_tombstoned(id).await.unwrap());
        assert!(persistence.load().await.unwrap().snapshots.is_empty());
    }

    #[tokio::test]
    async fn prestart_tombstones_are_bounded_and_purged_without_touching_terminal_ids() {
        let temp = tempfile::tempdir().unwrap();
        let persistence = Persistence::open(temp.path()).await.unwrap();
        let terminal_id = Uuid::new_v4();
        let snapshot = JobSnapshot::queued(
            terminal_id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        persistence.save(&snapshot).await.unwrap();
        persistence
            .retire(terminal_id, Duration::from_secs(3_600))
            .await
            .unwrap();

        let oldest = Uuid::new_v4();
        let middle = Uuid::new_v4();
        let newest = Uuid::new_v4();
        persistence
            .tombstone_prestart(oldest, Duration::from_secs(3_600), 2)
            .await
            .unwrap();
        persistence
            .tombstone_prestart(middle, Duration::from_secs(7_200), 2)
            .await
            .unwrap();
        persistence
            .tombstone_prestart(newest, Duration::from_secs(10_800), 2)
            .await
            .unwrap();

        let bounded_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM prestart_tombstones")
            .fetch_one(&persistence.pool)
            .await
            .unwrap();
        assert_eq!(bounded_count, 2);
        assert!(!persistence.is_tombstoned(oldest).await.unwrap());
        assert!(persistence.is_tombstoned(middle).await.unwrap());
        assert!(persistence.is_tombstoned(newest).await.unwrap());

        let expired = Uuid::new_v4();
        persistence
            .tombstone_prestart(expired, Duration::ZERO, 3)
            .await
            .unwrap();
        let count_before_purge: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM prestart_tombstones")
                .fetch_one(&persistence.pool)
                .await
                .unwrap();
        assert_eq!(count_before_purge, 3);
        persistence.purge_expired_tombstones().await.unwrap();
        let count_after_purge: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM prestart_tombstones")
            .fetch_one(&persistence.pool)
            .await
            .unwrap();
        assert_eq!(count_after_purge, 2);

        let terminal_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM job_tombstones WHERE id = ?")
                .bind(terminal_id.to_string())
                .fetch_one(&persistence.pool)
                .await
                .unwrap();
        assert_eq!(terminal_count, 1);
        assert!(persistence.is_tombstoned(terminal_id).await.unwrap());
    }

    #[tokio::test]
    async fn terminal_sse_subscription_closes_after_initial_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let mock_app = Router::new()
            .route("/internal/health", get(mock_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });
        let state = AppState::new(AppConfig {
            port: 0,
            data_root: data_root.clone(),
            state_root: temp.path().join("state"),
            frontend_dir: temp.path().join("frontend"),
            worker_url: format!("http://{address}"),
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        })
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let mut snapshot = JobSnapshot::queued(
            id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        snapshot.status = JobStatus::Cancelled;
        snapshot.finished_at = Some(Utc::now());
        let entry = JobEntry::new(snapshot, data_root.join(id.to_string()));
        state.0.jobs.write().await.insert(id, entry);
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/jobs/{id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = tokio::time::timeout(Duration::from_secs(1), response.into_body().collect())
            .await
            .expect("terminal SSE response stayed open")
            .unwrap()
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("cancelled"));
        server.abort();
    }

    #[derive(Clone)]
    struct MockWorker {
        cancelled: Arc<Notify>,
        cancel_calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct BlockingHealth {
        block: Arc<TestAtomicBool>,
        entered: Arc<Notify>,
        resume: Arc<Notify>,
    }

    async fn mock_start() -> (StatusCode, Json<serde_json::Value>) {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "running"})),
        )
    }

    async fn mock_drain() -> Json<serde_json::Value> {
        Json(serde_json::json!({"status": "idle", "active_jobs": 0}))
    }

    async fn mock_health() -> Json<serde_json::Value> {
        let epoch = MOCK_WORKER_EPOCH.load(Ordering::SeqCst);
        Json(serde_json::json!({
            "status": "ok",
            "provider": "google",
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": format!("mock-worker-epoch-{epoch}"),
            "container_boot_id": format!("mock-container-boot-{epoch}"),
            "max_active_jobs": 1
        }))
    }

    async fn mock_fail_stop() -> StatusCode {
        MOCK_WORKER_EPOCH.fetch_add(1, Ordering::SeqCst);
        StatusCode::ACCEPTED
    }

    async fn mock_blocking_health(
        State(control): State<BlockingHealth>,
    ) -> Json<serde_json::Value> {
        if control.block.load(Ordering::SeqCst) {
            control.entered.notify_one();
            control.resume.notified().await;
        }
        let epoch = MOCK_WORKER_EPOCH.load(Ordering::SeqCst);
        Json(serde_json::json!({
            "status": "ok",
            "provider": "google",
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": format!("blocking-worker-epoch-{epoch}"),
            "container_boot_id": format!("blocking-container-boot-{epoch}"),
            "max_active_jobs": 1
        }))
    }

    async fn mock_resume() -> Json<serde_json::Value> {
        Json(serde_json::json!({"status": "ready", "active_jobs": 0}))
    }

    fn mock_app_config(root: &Path, worker_url: String) -> AppConfig {
        AppConfig {
            port: 0,
            data_root: root.join("jobs"),
            state_root: root.join("state"),
            frontend_dir: root.join("frontend"),
            worker_url,
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        }
    }

    async fn state_with_mock_worker(root: &Path) -> (AppState, tokio::task::JoinHandle<()>) {
        let mock_app = Router::new()
            .route("/internal/health", get(mock_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });
        let state = AppState::new(mock_app_config(root, format!("http://{address}")))
            .await
            .unwrap();
        (state, server)
    }

    #[tokio::test]
    async fn small_api_body_limit_does_not_reduce_the_upload_allowance() {
        let temp = tempfile::tempdir().unwrap();
        let (state, server) = state_with_mock_worker(temp.path()).await;
        let app = build_router(state);
        let oversized = vec![b'x'; SMALL_API_BODY_BYTES + 1];
        let content_length = oversized.len().to_string();

        for uri in ["/api/admin/login", "/api/not-a-real-route"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header(CONTENT_TYPE, "application/json")
                        .header(CONTENT_LENGTH, &content_length)
                        .body(Body::from(oversized.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
        }

        let upload_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/jobs")
                    .header(CONTENT_TYPE, "multipart/form-data; boundary=test-boundary")
                    .header(CONTENT_LENGTH, content_length)
                    .body(Body::from(oversized))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(upload_response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_terminal_commit_keeps_memory_broadcast_and_worker_journal_unmodified() {
        let temp = tempfile::tempdir().unwrap();
        let (state, server) = state_with_mock_worker(temp.path()).await;
        let id = Uuid::new_v4();
        let output_dir = state.0.config.data_root.join(id.to_string()).join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("mono.pdf"), b"%PDF-result").unwrap();
        let journal = output_dir.join(".worker-terminal.json");
        std::fs::write(&journal, br#"{"type":"finished"}"#).unwrap();

        let mut snapshot = JobSnapshot::queued(
            id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: false,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        snapshot.status = JobStatus::Running;
        snapshot.stage = "正在翻译".to_owned();
        state.0.persistence.save(&snapshot).await.unwrap();
        let entry = JobEntry::new(
            snapshot.clone(),
            state.0.config.data_root.join(id.to_string()),
        );
        state.0.jobs.write().await.insert(id, entry.clone());

        let files = confirm_materialized_outputs(&state, &entry, id)
            .await
            .unwrap();
        assert!(files.mono);
        assert_eq!(std::fs::read(&journal).unwrap(), br#"{"type":"finished"}"#);
        let mut receiver = entry.sender.subscribe();
        state.0.persistence.pool.close().await;

        let result = commit_job_update(&state, &entry, move |candidate| {
            candidate.status = JobStatus::Succeeded;
            candidate.files = files;
            candidate.finished_at = Some(Utc::now());
            true
        })
        .await;
        assert!(result.is_err());
        assert_eq!(entry.snapshot.read().await.status, JobStatus::Running);
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(std::fs::read(journal).unwrap(), br#"{"type":"finished"}"#);
        server.abort();
    }

    #[tokio::test]
    async fn legacy_google_only_worker_rejects_deepseek_capability() {
        let mock_app = Router::new()
            .route("/internal/health", get(mock_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(AppConfig {
            port: 0,
            data_root: temp.path().join("jobs"),
            state_root: temp.path().join("state"),
            frontend_dir: temp.path().join("frontend"),
            worker_url: format!("http://{address}"),
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        })
        .await
        .unwrap();

        assert!(worker_is_ready_for_provider(&state, TranslationProvider::Google).await);
        assert!(!worker_is_ready_for_provider(&state, TranslationProvider::DeepSeek).await);
        server.abort();
    }

    #[tokio::test]
    async fn unknown_cancel_and_release_are_durable_and_prevent_late_create() {
        let control = BlockingHealth {
            block: Arc::new(TestAtomicBool::new(false)),
            entered: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        };
        let mock_app = Router::new()
            .route("/internal/health", get(mock_blocking_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume))
            .with_state(control.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let state = AppState::new(AppConfig {
            port: 0,
            data_root: data_root.clone(),
            state_root: temp.path().join("state"),
            frontend_dir: temp.path().join("frontend"),
            worker_url: format!("http://{address}"),
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        })
        .await
        .unwrap();
        let app = build_router(state.clone());

        // Hold create_job in its initial worker-health await so release reaches
        // the server before a PendingUploadState has been registered.
        control.block.store(true, Ordering::SeqCst);
        let released_id = Uuid::new_v4();
        let create_request = Request::builder()
            .method("POST")
            .uri("/api/jobs")
            .header("x-job-id", released_id.to_string())
            .header(CONTENT_TYPE, "multipart/form-data; boundary=prestart-race")
            .body(Body::from("--prestart-race--\r\n"))
            .unwrap();
        let create_app = app.clone();
        let late_create =
            tokio::spawn(async move { create_app.oneshot(create_request).await.unwrap() });
        tokio::time::timeout(Duration::from_secs(5), control.entered.notified())
            .await
            .expect("create request never reached the blocking worker health check");

        let release_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/jobs/{released_id}/release"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(release_response.status(), StatusCode::ACCEPTED);
        assert!(
            state
                .0
                .persistence
                .is_tombstoned(released_id)
                .await
                .unwrap()
        );

        control.block.store(false, Ordering::SeqCst);
        control.resume.notify_one();
        let create_response = late_create.await.unwrap();
        assert_eq!(create_response.status(), StatusCode::GONE);
        assert!(!state.0.jobs.read().await.contains_key(&released_id));
        assert!(pending_upload_token(&state, released_id).is_none());
        assert!(!data_root.join(released_id.to_string()).exists());

        let cancelled_id = Uuid::new_v4();
        let cancel_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/jobs/{cancelled_id}/cancel"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel_response.status(), StatusCode::ACCEPTED);
        assert!(
            state
                .0
                .persistence
                .is_tombstoned(cancelled_id)
                .await
                .unwrap()
        );
        server.abort();
    }

    async fn mock_events(State(worker): State<MockWorker>) -> String {
        worker.cancelled.notified().await;
        MOCK_WORKER_EPOCH.fetch_add(1, Ordering::SeqCst);
        "{\"type\":\"cancelled\",\"message\":\"cleaned\"}\n".to_owned()
    }

    async fn mock_broken_events() -> &'static str {
        ""
    }

    async fn mock_cancel(State(worker): State<MockWorker>) -> Json<serde_json::Value> {
        worker.cancel_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        MOCK_WORKER_EPOCH.fetch_add(1, Ordering::SeqCst);
        worker.cancelled.notify_one();
        Json(serde_json::json!({"status": "cancelled"}))
    }

    async fn wait_for_status(entry: &Arc<JobEntry>, expected: JobStatus) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if entry.snapshot.read().await.status == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("job did not reach expected state");
    }

    #[tokio::test]
    async fn cancellation_holds_the_permit_until_worker_cleanup_confirmation() {
        let mock = MockWorker {
            cancelled: Arc::new(Notify::new()),
            cancel_calls: Arc::new(AtomicUsize::new(0)),
        };
        let mock_app = Router::new()
            .route("/internal/health", get(mock_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/jobs/{id}", post(mock_start))
            .route("/internal/jobs/{id}/events", get(mock_events))
            .route("/internal/jobs/{id}/cancel", post(mock_cancel))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let config = AppConfig {
            port: 0,
            data_root: data_root.clone(),
            state_root: temp.path().join("state"),
            frontend_dir: temp.path().join("frontend"),
            worker_url: format!("http://{address}"),
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        };
        let state = AppState::new(config).await.unwrap();
        #[cfg(target_os = "linux")]
        let escaped_target = {
            use std::os::unix::fs::symlink;

            let target = temp.path().join("outside-secret");
            fs::write(&target, b"do-not-delete").await.unwrap();
            symlink(&target, data_root.join("worker-leftover-link")).unwrap();
            fs::write(data_root.join("worker-leftover-file"), b"junk")
                .await
                .unwrap();
            target
        };
        let id = Uuid::new_v4();
        let job_dir = data_root.join(id.to_string());
        fs::create_dir_all(&job_dir).await.unwrap();
        fs::write(job_dir.join("input.pdf"), b"%PDF-1.7\n")
            .await
            .unwrap();
        let snapshot = JobSnapshot::queued(
            id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        state.0.persistence.save(&snapshot).await.unwrap();
        let entry = JobEntry::new(snapshot, job_dir.clone());
        state.0.jobs.write().await.insert(id, entry.clone());

        let run_state = state.clone();
        let run_entry = entry.clone();
        let runner = tokio::spawn(async move { run_job(run_state, run_entry).await });
        wait_for_status(&entry, JobStatus::Running).await;
        assert_eq!(state.0.permits.available_permits(), 0);

        request_cancellation(&state, &entry, "test cancellation").await;
        assert_eq!(entry.snapshot.read().await.status, JobStatus::Cancelling);
        assert_eq!(state.0.permits.available_permits(), 0);

        wait_for_status(&entry, JobStatus::Cancelled).await;
        runner.await.unwrap();
        assert_eq!(state.0.permits.available_permits(), 1);
        assert_eq!(mock.cancel_calls.load(Ordering::SeqCst), 1);
        assert!(!job_dir.exists());
        assert!(
            fs::read_dir(&data_root)
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
        #[cfg(target_os = "linux")]
        assert_eq!(fs::read(escaped_target).await.unwrap(), b"do-not-delete");
        server.abort();
    }

    #[tokio::test]
    async fn broken_progress_stream_reconciles_before_releasing_the_permit() {
        let mock = MockWorker {
            cancelled: Arc::new(Notify::new()),
            cancel_calls: Arc::new(AtomicUsize::new(0)),
        };
        let mock_app = Router::new()
            .route("/internal/health", get(mock_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/jobs/{id}", post(mock_start))
            .route("/internal/jobs/{id}/events", get(mock_broken_events))
            .route("/internal/jobs/{id}/cancel", post(mock_cancel))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let state = AppState::new(AppConfig {
            port: 0,
            data_root: data_root.clone(),
            state_root: temp.path().join("state"),
            frontend_dir: temp.path().join("frontend"),
            worker_url: format!("http://{address}"),
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        })
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let job_dir = data_root.join(id.to_string());
        fs::create_dir_all(&job_dir).await.unwrap();
        fs::write(job_dir.join("input.pdf"), b"%PDF-1.7\n")
            .await
            .unwrap();
        let snapshot = JobSnapshot::queued(
            id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        state.0.persistence.save(&snapshot).await.unwrap();
        let entry = JobEntry::new(snapshot, job_dir);
        state.0.jobs.write().await.insert(id, entry.clone());

        let run_state = state.clone();
        let run_entry = entry.clone();
        let runner = tokio::spawn(async move { run_job(run_state, run_entry).await });
        wait_for_status(&entry, JobStatus::Cancelling).await;
        assert_eq!(state.0.permits.available_permits(), 0);

        wait_for_status(&entry, JobStatus::Failed).await;
        runner.await.unwrap();
        assert_eq!(state.0.permits.available_permits(), 1);
        assert_eq!(mock.cancel_calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn graceful_shutdown_has_one_worker_cancellation_owner() {
        let mock = MockWorker {
            cancelled: Arc::new(Notify::new()),
            cancel_calls: Arc::new(AtomicUsize::new(0)),
        };
        let mock_app = Router::new()
            .route("/internal/health", get(mock_health))
            .route("/internal/fail-stop", post(mock_fail_stop))
            .route("/internal/jobs/{id}", post(mock_start))
            .route("/internal/jobs/{id}/events", get(mock_events))
            .route("/internal/jobs/{id}/cancel", post(mock_cancel))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/resume", post(mock_resume))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("jobs");
        let state = AppState::new(AppConfig {
            port: 0,
            data_root: data_root.clone(),
            state_root: temp.path().join("state"),
            frontend_dir: temp.path().join("frontend"),
            worker_url: format!("http://{address}"),
            worker_token: "test-token".to_owned(),
            max_upload_bytes: 1024 * 1024,
            upload_timeout: Duration::from_secs(60),
            concurrency: 1,
            max_active_jobs: 1,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        })
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let job_dir = data_root.join(id.to_string());
        fs::create_dir_all(&job_dir).await.unwrap();
        fs::write(job_dir.join("input.pdf"), b"%PDF-1.7\n")
            .await
            .unwrap();
        let snapshot = JobSnapshot::queued(
            id,
            "paper.pdf".to_owned(),
            JobOptions {
                source_lang: "auto".to_owned(),
                target_lang: "zh".to_owned(),
                pages: None,
                output_mono: true,
                output_dual: true,
                provider: TranslationProvider::Google,
                deepseek_model: None,
                deepseek_thinking_mode: None,
                deepseek_reasoning_effort: None,
                provider_id: "google".to_owned(),
                provider_revision: None,
                model_id: None,
            },
        );
        let entry = JobEntry::new(snapshot, job_dir);
        state.0.jobs.write().await.insert(id, entry.clone());
        let run_state = state.clone();
        let run_entry = entry.clone();
        state
            .0
            .tasks
            .spawn(async move { run_job(run_state, run_entry).await });
        wait_for_status(&entry, JobStatus::Running).await;

        state.shutdown().await;

        assert_eq!(entry.snapshot.read().await.status, JobStatus::Cancelled);
        assert_eq!(mock.cancel_calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
