use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::env;
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use uuid::Uuid;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const LANGUAGE_CODES: &[&str] = &[
    "auto", "en", "zh", "zh-TW", "ja", "ko", "fr", "de", "es", "ru", "it", "pt",
];
const MAX_FIELD_BYTES: usize = 1024;
const MAX_INTERNAL_EVENT_BYTES: usize = 256 * 1024;
const PRESTART_TOMBSTONE_RETENTION: Duration = Duration::from_secs(15 * 60);
const PRESTART_TOMBSTONE_CAPACITY: usize = 4096;

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
            env::var("PDF2ZH_WORKER_TOKEN").map_err(|_| "PDF2ZH_WORKER_TOKEN must be set")?;
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
        let max_active_jobs = env_parse("PDF2ZH_MAX_ACTIVE_JOBS", 8_usize)?;
        if max_active_jobs == 0 {
            return Err("PDF2ZH_MAX_ACTIVE_JOBS must be at least one".into());
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOptions {
    pub source_lang: String,
    pub target_lang: String,
    pub pages: Option<String>,
    pub output_mono: bool,
    pub output_dual: bool,
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
        Self::new_with_admission(snapshot, job_dir, None)
    }

    fn new_with_admission(
        snapshot: JobSnapshot,
        job_dir: PathBuf,
        admission_permit: Option<OwnedSemaphorePermit>,
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
                Ok(snapshot) if snapshot.id.to_string() == row_id => snapshots.push(snapshot),
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
    worker_epoch: RwLock<Option<String>>,
    client: Client,
    persistence: Persistence,
}

#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl AppState {
    pub async fn new(config: AppConfig) -> Result<Self, BoxError> {
        fs::create_dir_all(&config.data_root).await?;
        let persistence = Persistence::open(&config.state_root).await?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()?;
        let state = Self(Arc::new(AppStateInner {
            permits: Arc::new(Semaphore::new(config.concurrency)),
            admission: Arc::new(Semaphore::new(config.max_active_jobs)),
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            runner_gate: Mutex::new(()),
            pending_uploads: StdMutex::new(HashMap::new()),
            worker_epoch: RwLock::new(None),
            jobs: RwLock::new(HashMap::new()),
            client,
            persistence,
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
                && health.status == "ok"
                && health.provider == "google"
            {
                *self.0.worker_epoch.write().await = Some(health.epoch.clone());
                info!(epoch = %health.epoch, "connected to Python worker epoch");
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
        {
            let mut jobs = self.0.jobs.write().await;
            for snapshot in persisted.snapshots {
                let id = snapshot.id;
                let was_cancelling = snapshot.status == JobStatus::Cancelling;
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
                let entry = JobEntry::new_with_admission(snapshot, job_dir, admission_permit);
                if active {
                    interrupted.push((entry.clone(), was_cancelling));
                }
                jobs.insert(id, entry);
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
        }
        for id in persisted.corrupt_ids {
            if self.0.jobs.read().await.contains_key(&id) {
                error!(%id, "refusing to clean a corrupt row id that is also a valid restored job");
                continue;
            }
            warn!(%id, "cancelling the worker id recovered from a corrupt state row");
            let _ = cancel_worker_until_terminal(self, id).await;
            safe_remove_job_dir(self, id).await;
        }
        if !drain_worker_until_idle(self).await {
            warn!("startup drain exceeded its deadline; recycling the Python worker");
            let _ = force_recycle_worker(self, Uuid::nil()).await;
        }
        cleanup_orphan_job_dirs(self).await;
        resume_worker(self).await?;
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
            Self::Gone => StatusCode::GONE,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
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

#[derive(Deserialize)]
struct WorkerHealth {
    status: String,
    provider: String,
    active_jobs: usize,
    accepting_jobs: bool,
    epoch: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicConfig {
    provider: &'static str,
    languages: Vec<Language>,
    max_upload_bytes: u64,
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

    Router::new()
        .route("/api/health", get(health))
        .route("/api/config", get(public_config))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(get_job).delete(dispose_job))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/jobs/{id}/heartbeat", post(heartbeat_job))
        .route("/api/jobs/{id}/release", post(release_job))
        .route("/api/jobs/{id}/files/{kind}", get(download_file))
        .route("/api", any(api_not_found))
        .route("/api/{*path}", any(api_not_found))
        .fallback_service(static_files)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(CompressionLayer::new())
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("same-origin"),
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
    let Some(health) = fetch_worker_health(state).await else {
        return false;
    };
    let ready = health.status == "ok"
        && health.provider == "google"
        && health.accepting_jobs
        && health.active_jobs <= state.0.config.concurrency;
    if ready {
        *state.0.worker_epoch.write().await = Some(health.epoch);
    }
    ready
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

async fn public_config(State(state): State<AppState>) -> Json<PublicConfig> {
    Json(PublicConfig {
        provider: "google",
        max_upload_bytes: state.0.config.max_upload_bytes,
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
    })
}

#[derive(Default)]
struct PendingUpload {
    filename: Option<String>,
    source_lang: Option<String>,
    target_lang: Option<String>,
    pages: Option<String>,
    output_mono: Option<bool>,
    output_dual: Option<bool>,
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
    let job_dir = state.0.config.data_root.join(id.to_string());
    if let Err(error) = fs::create_dir(&job_dir).await {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            let existing = state.0.jobs.read().await.get(&id).cloned();
            if let Some(entry) = existing {
                pending_guard.commit();
                return Ok((StatusCode::OK, Json(entry.snapshot.read().await.clone())));
            }
            pending_guard.commit();
            return Err(ApiError::Conflict("任务编号已被占用".to_owned()));
        }
        return Err(internal_error(error));
    }
    pending_guard.mark_created_dir();
    let upload_result = tokio::time::timeout(
        state.0.config.upload_timeout,
        receive_upload(&state, &job_dir, &mut multipart, &upload_state.cancellation),
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

    let options = match validate_upload_options(&pending) {
        Ok(options) => options,
        Err(error) => {
            safe_remove_job_dir(&state, id).await;
            return Err(error);
        }
    };
    let filename = pending
        .filename
        .unwrap_or_else(|| "document.pdf".to_owned());
    let snapshot = JobSnapshot::queued(id, filename, options);
    let entry = JobEntry::new_with_admission(
        snapshot.clone(),
        job_dir,
        Some(pending_guard.take_admission_permit()),
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

async fn receive_upload(
    state: &AppState,
    job_dir: &Path,
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
            let partial_path = job_dir.join("input.partial");
            let final_path = job_dir.join("input.pdf");
            let mut output = File::create(&partial_path).await.map_err(internal_error)?;
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
                output.write_all(&chunk).await.map_err(internal_error)?;
            }
            output.flush().await.map_err(internal_error)?;
            output.sync_all().await.map_err(internal_error)?;
            drop(output);
            if written == 0 || prefix.as_ref() != b"%PDF-" {
                return Err(ApiError::BadRequest("文件不是有效的 PDF".to_owned()));
            }
            fs::rename(partial_path, final_path)
                .await
                .map_err(internal_error)?;
        } else {
            if !matches!(
                name.as_str(),
                "source_lang" | "target_lang" | "pages" | "output_mono" | "output_dual"
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
    Ok(JobOptions {
        source_lang,
        target_lang,
        pages,
        output_mono,
        output_dual,
    })
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
    let (available, filename, content_type) = match kind.as_str() {
        "mono" => (
            snapshot.files.mono,
            "translated-mono.pdf",
            "application/pdf",
        ),
        "dual" => (
            snapshot.files.dual,
            "translated-dual.pdf",
            "application/pdf",
        ),
        "glossary" => (
            snapshot.files.glossary,
            "translated-glossary.csv",
            "text/csv; charset=utf-8",
        ),
        _ => return Err(ApiError::NotFound),
    };
    if !available {
        return Err(ApiError::NotFound);
    }
    let extension = if kind == "glossary" { "csv" } else { "pdf" };
    let file_path = entry
        .job_dir
        .join("output")
        .join(format!("{kind}.{extension}"));
    if !path_belongs_to_job(&state, id, &file_path) {
        return Err(ApiError::Internal);
    }
    let file = File::open(&file_path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let metadata = file.metadata().await.map_err(internal_error)?;
    if !metadata.is_file() {
        return Err(ApiError::NotFound);
    }
    let body = Body::from_stream(ReaderStream::new(file));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, metadata.len())
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
    let snapshot = {
        let mut snapshot = entry.snapshot.write().await;
        if snapshot.status.is_terminal() || entry.cancellation.is_cancelled() {
            return false;
        }
        entry.cancellation.cancel();
        snapshot.status = JobStatus::Cancelling;
        snapshot.stage = "正在取消".to_owned();
        snapshot.message = message.to_owned();
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.clone()
    };
    persist_and_publish(state, entry, snapshot).await;
    true
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
}

#[derive(Debug, Deserialize)]
struct WorkerEvent {
    #[serde(rename = "type")]
    kind: String,
    progress: Option<f64>,
    stage: Option<String>,
    message: Option<String>,
    files: Option<JobFiles>,
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
                        return;
                    }
                }
            }
            _ = entry.cancellation.cancelled() => {}
        }
    };

    let starting_snapshot = {
        let mut snapshot = entry.snapshot.write().await;
        if entry.cancellation.is_cancelled() || state.0.shutdown.is_cancelled() {
            None
        } else {
            snapshot.status = JobStatus::Running;
            snapshot.stage = "启动翻译引擎".to_owned();
            snapshot.message = "正在连接内部翻译服务".to_owned();
            snapshot.started_at = Some(Utc::now());
            snapshot.revision = snapshot.revision.saturating_add(1);
            Some(snapshot.clone())
        }
    };
    let Some(starting_snapshot) = starting_snapshot else {
        mark_cancelled(&state, &entry, "任务已取消").await;
        safe_remove_job_dir(&state, id).await;
        drop(permit);
        return;
    };
    persist_and_publish(&state, &entry, starting_snapshot).await;

    let snapshot = entry.snapshot.read().await.clone();
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
    };

    match start_worker_job(&state, id, &request).await {
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
            drop(permit);
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
            drop(permit);
            return;
        }
    }

    update_job(&state, &entry, |snapshot| {
        if !entry.cancellation.is_cancelled() {
            snapshot.status = JobStatus::Running;
            snapshot.stage = "准备文档".to_owned();
            snapshot.message = "Google 翻译引擎已启动".to_owned();
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
    // This permit is intentionally held until the Python event stream confirms
    // a terminal state, which also confirms subprocess/IPC cleanup.
    drop(permit);
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
    let mut previous_epoch = state.0.worker_epoch.read().await.clone();
    warn!(
        %id,
        epoch = previous_epoch.as_deref().unwrap_or("unknown"),
        "worker cleanup deadline expired; requesting a fail-stop recycle"
    );

    let current_epoch = fetch_worker_health(state)
        .await
        .filter(|health| health.status == "ok" && health.provider == "google")
        .map(|health| health.epoch);
    if current_epoch.as_deref() == previous_epoch.as_deref() || current_epoch.is_none() {
        request_worker_fail_stop(state).await;
    }
    let recycle_started = Instant::now();
    let mut last_fail_stop = Instant::now();
    loop {
        if recycle_started.elapsed() > state.0.config.worker_recycle_timeout {
            fail_stop_coordinator("Python worker could not be recycled before the hard deadline");
        }
        if let Some(health) = fetch_worker_health(state).await
            && health.status == "ok"
            && health.provider == "google"
            && Some(health.epoch.as_str()) != previous_epoch.as_deref()
        {
            info!(%id, epoch = %health.epoch, "new Python worker epoch confirmed");
            let epoch = health.epoch;
            *state.0.worker_epoch.write().await = Some(epoch.clone());
            if drain_worker_until_idle(state).await {
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
            warn!(epoch = %epoch, "replacement worker could not be drained and resumed; recycling it again");
            previous_epoch = Some(epoch);
            request_worker_fail_stop(state).await;
            last_fail_stop = Instant::now();
        }
        if last_fail_stop.elapsed() >= Duration::from_secs(10) {
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
                    if let Some(files) = inspect_finished_files(entry).await {
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

async fn inspect_finished_files(entry: &Arc<JobEntry>) -> Option<JobFiles> {
    let output_dir = entry.job_dir.join("output");
    let mono = valid_pdf_file(&output_dir.join("mono.pdf")).await;
    let dual = valid_pdf_file(&output_dir.join("dual.pdf")).await;
    if !mono && !dual {
        return None;
    }
    let glossary = fs::metadata(output_dir.join("glossary.csv"))
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    Some(JobFiles {
        mono,
        dual,
        glossary,
    })
}

async fn valid_pdf_file(path: &Path) -> bool {
    let Ok(mut file) = File::open(path).await else {
        return false;
    };
    let Ok(metadata) = file.metadata().await else {
        return false;
    };
    if !metadata.is_file() || metadata.len() < 5 {
        return false;
    }
    let mut prefix = [0_u8; 5];
    file.read_exact(&mut prefix).await.is_ok() && &prefix == b"%PDF-"
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
                    if let Some(stage) = event.stage {
                        snapshot.stage = stage;
                    }
                    if let Some(message) = event.message {
                        snapshot.message = message;
                    }
                }
            })
            .await;
            false
        }
        "finished" => {
            update_job(state, entry, |snapshot| {
                if snapshot.status.is_terminal() {
                    return;
                }
                snapshot.status = JobStatus::Succeeded;
                snapshot.progress = 100.0;
                snapshot.stage = event.stage.unwrap_or_else(|| "翻译完成".to_owned());
                snapshot.message = event
                    .message
                    .unwrap_or_else(|| "文件已经可以下载".to_owned());
                snapshot.files = event.files.unwrap_or_default();
                snapshot.finished_at = Some(Utc::now());
            })
            .await;
            true
        }
        "cancelled" => {
            mark_cancelled(
                state,
                entry,
                event.message.as_deref().unwrap_or("翻译任务已安全取消"),
            )
            .await;
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
    let snapshot = {
        let mut snapshot = entry.snapshot.write().await;
        update(&mut snapshot);
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.clone()
    };
    let terminal = snapshot.status.is_terminal();
    persist_and_publish(state, entry, snapshot).await;
    if terminal {
        entry
            .admission_permit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

async fn persist_and_publish(state: &AppState, entry: &Arc<JobEntry>, snapshot: JobSnapshot) {
    let _publish_guard = entry.publish_lock.lock().await;
    if entry.disposed.load(AtomicOrdering::Acquire) {
        return;
    }
    if entry.snapshot.read().await.revision > snapshot.revision {
        return;
    }
    if let Err(error) = state.0.persistence.save(&snapshot).await {
        error!(id = %snapshot.id, %error, "failed to persist job snapshot");
    }
    let _ = entry.sender.send(snapshot);
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
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = single_line.chars().take(240).collect::<String>();
    if truncated.is_empty() {
        "翻译服务发生未知错误".to_owned()
    } else {
        truncated
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
    let snapshot = {
        let mut snapshot = entry.snapshot.write().await;
        if !snapshot.status.is_active() || entry.cancellation.is_cancelled() {
            return false;
        }
        entry.cancellation.cancel();
        snapshot.status = JobStatus::Cancelling;
        snapshot.stage = "正在取消".to_owned();
        snapshot.message = if release_expired {
            "页面离开后未恢复，正在回收任务".to_owned()
        } else {
            "页面连接已失效，正在回收任务".to_owned()
        };
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.clone()
    };
    info!(id = %snapshot.id, release_expired, "job lease expired; requesting cancellation");
    if let Err(error) = state.0.persistence.save(&snapshot).await {
        error!(id = %snapshot.id, %error, "failed to persist lease-expired cancellation");
    }
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

async fn remove_job_files(state: &AppState, id: Uuid) -> Result<(), BoxError> {
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

        let invalid = PendingUpload {
            source_lang: Some("en".to_owned()),
            target_lang: Some("en".to_owned()),
            ..PendingUpload::default()
        };
        assert!(validate_upload_options(&invalid).is_err());
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
            max_active_jobs: 8,
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
        Json(serde_json::json!({
            "status": "ok",
            "provider": "google",
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": "mock-worker-epoch"
        }))
    }

    async fn mock_blocking_health(
        State(control): State<BlockingHealth>,
    ) -> Json<serde_json::Value> {
        if control.block.load(Ordering::SeqCst) {
            control.entered.notify_one();
            control.resume.notified().await;
        }
        Json(serde_json::json!({
            "status": "ok",
            "provider": "google",
            "active_jobs": 0,
            "accepting_jobs": true,
            "epoch": "blocking-worker-epoch"
        }))
    }

    async fn mock_resume() -> Json<serde_json::Value> {
        Json(serde_json::json!({"status": "ready", "active_jobs": 0}))
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
            max_active_jobs: 8,
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
        "{\"type\":\"cancelled\",\"message\":\"cleaned\"}\n".to_owned()
    }

    async fn mock_broken_events() -> &'static str {
        ""
    }

    async fn mock_cancel(State(worker): State<MockWorker>) -> Json<serde_json::Value> {
        worker.cancel_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
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
            max_active_jobs: 8,
            lease_timeout: Duration::from_secs(60),
            release_grace: Duration::from_secs(10),
            cancel_reconcile_timeout: Duration::from_secs(30),
            worker_recycle_timeout: Duration::from_secs(60),
            terminal_retention: Duration::from_secs(60),
            id_tombstone_retention: Duration::from_secs(60),
        };
        let state = AppState::new(config).await.unwrap();
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
            max_active_jobs: 8,
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
            max_active_jobs: 8,
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
