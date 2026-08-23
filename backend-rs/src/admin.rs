use std::collections::{HashMap, HashSet};
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::header::{CACHE_CONTROL, COOKIE, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tower_http::set_header::SetResponseHeaderLayer;
use url::{Host, Url};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{AppState, BoxError};

const SESSION_COOKIE_SECURE: &str = "__Host-pdf2zh_session";
const SESSION_COOKIE_INSECURE: &str = "pdf2zh_session";
const CSRF_COOKIE_SECURE: &str = "__Host-pdf2zh_csrf";
const CSRF_COOKIE_INSECURE: &str = "pdf2zh_csrf";
const SESSION_IDLE_SECONDS: i64 = 12 * 60 * 60;
const SESSION_ABSOLUTE_SECONDS: i64 = 7 * 24 * 60 * 60;
const LOGIN_WINDOW: Duration = Duration::from_secs(10 * 60);
const LOGIN_ATTEMPTS: usize = 5;
const MAX_RATE_BUCKETS: usize = 1024;
const ARGON2_CONCURRENCY: usize = 2;
const SETUP_CONCURRENCY: usize = 1;
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";
const DEEPSEEK_MODEL_FLASH: &str = "deepseek-v4-flash";
const DEEPSEEK_MODEL_PRO: &str = "deepseek-v4-pro";

#[derive(Clone)]
pub(crate) struct AdminService(Arc<AdminServiceInner>);

struct AdminServiceInner {
    pool: SqlitePool,
    public_origin: String,
    secure_cookies: bool,
    master_key: Option<Arc<Zeroizing<[u8; 32]>>>,
    setup_token_hash: Option<[u8; 32]>,
    dummy_password_hash: String,
    trust_forwarded_for: bool,
    rate_limits: FailureRateLimiter,
    argon2_slots: Arc<Semaphore>,
    setup_slots: Arc<Semaphore>,
}

pub(crate) struct RuntimeProvider {
    pub provider_id: String,
    pub provider_name: String,
    pub kind: String,
    pub revision: i64,
    pub model: String,
    pub base_url: String,
    pub sealed_api_key_ciphertext: Vec<u8>,
    pub sealed_api_key_nonce: Vec<u8>,
    pub sealed_key_id: &'static str,
}

#[derive(Default)]
struct FailureRateLimiter {
    buckets: StdMutex<HashMap<String, Vec<Instant>>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicProviderConfig {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub models: Vec<PublicProviderModel>,
    pub default_model: Option<String>,
    pub thinking_modes: Vec<String>,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct PublicProviderModel {
    pub id: String,
    pub name: String,
}

pub(crate) enum PaidJobAuthorizationError {
    Unauthorized,
    Forbidden,
    Internal,
}

pub(crate) struct ConfigSession {
    pub csrf_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, thiserror::Error)]
enum AdminError {
    #[error("请求内容无效")]
    BadRequest,
    #[error("用户名或密码错误")]
    Unauthorized,
    #[error("管理员会话或 CSRF 校验失败")]
    Forbidden,
    #[error("初始化已经完成")]
    SetupComplete,
    #[error("配置已被其他管理员修改，请刷新后重试")]
    RevisionConflict,
    #[error("请求过于频繁，请稍后重试")]
    RateLimited,
    #[error("模型配置不存在")]
    NotFound,
    #[error("服务器尚未配置凭据加密主密钥")]
    MasterKeyUnavailable,
    #[error("服务器内部错误")]
    Internal,
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::SetupComplete | Self::RevisionConflict => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MasterKeyUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut response = (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
        response
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    setup_required: bool,
    setup_token_required: bool,
    authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetupRequest {
    setup_token: Option<String>,
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Clone)]
struct AdminPrincipal {
    session_id: String,
    user_id: String,
    username: String,
    csrf_hash: Vec<u8>,
}

struct NewSession {
    id: String,
    token: String,
    token_hash: Vec<u8>,
    csrf_token: String,
    csrf_hash: Vec<u8>,
    created_at: i64,
    idle_expires_at: i64,
    absolute_expires_at: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminProvider {
    id: String,
    kind: String,
    name: String,
    base_url: String,
    enabled: bool,
    revision: i64,
    has_api_key: bool,
    models: Vec<AdminProviderModel>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AdminProviderModel {
    id: String,
    name: String,
    is_default: bool,
}

#[derive(Serialize)]
struct ProvidersResponse {
    providers: Vec<AdminProvider>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PutDeepSeekRequest {
    kind: String,
    name: String,
    base_url: String,
    expected_revision: i64,
    enabled: bool,
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
    models: Vec<AdminProviderModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateProviderRequest {
    kind: String,
    name: String,
    base_url: String,
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
    enabled: bool,
    models: Vec<AdminProviderModel>,
    expected_revision: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateProviderRequest {
    kind: String,
    expected_revision: i64,
    name: String,
    base_url: String,
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
    enabled: bool,
    models: Vec<AdminProviderModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteProviderQuery {
    expected_revision: i64,
}

struct EncryptedSecret {
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_id: &'static str,
    version: i64,
}

impl AdminService {
    pub(crate) async fn new(pool: SqlitePool, port: u16) -> Result<Self, BoxError> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        // The credential-history scrub migration runs with secure_delete. A
        // truncate checkpoint also removes pre-migration WAL frames before the
        // service starts accepting requests.
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await?;

        let origin_text =
            env::var("PDF2ZH_PUBLIC_ORIGIN").unwrap_or_else(|_| format!("http://127.0.0.1:{port}"));
        let origin = Url::parse(&origin_text)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host().is_none()
            || origin.username() != ""
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.path() != "/"
        {
            return Err("PDF2ZH_PUBLIC_ORIGIN must be an HTTP(S) origin without a path".into());
        }
        let public_origin = origin.origin().ascii_serialization();
        let secure_cookies = match env::var("PDF2ZH_SECURE_COOKIES") {
            Ok(value) => parse_bool_env("PDF2ZH_SECURE_COOKIES", &value)?,
            Err(_) => origin.scheme() == "https",
        };
        let trust_forwarded_for = match env::var("PDF2ZH_TRUST_X_FORWARDED_FOR") {
            Ok(value) => parse_bool_env("PDF2ZH_TRUST_X_FORWARDED_FOR", &value)?,
            Err(_) => false,
        };

        let master_key_file = env::var_os("PDF2ZH_MASTER_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/secrets/pdf2zh_master_key"));
        let master_key = load_master_key(master_key_file).await?;
        verify_or_initialize_master_key(&pool, master_key.as_ref().map(|key| &key[..])).await?;

        let setup_token_hash = if let Some(path) = env::var_os("PDF2ZH_SETUP_TOKEN_FILE") {
            let token = tokio::fs::read_to_string(PathBuf::from(path)).await?;
            let token = token.trim();
            if token.is_empty() {
                return Err("PDF2ZH_SETUP_TOKEN_FILE cannot be blank".into());
            }
            Some(hash_bytes(token.as_bytes()))
        } else {
            None
        };

        let dummy_token = random_token(24)
            .map_err(|_| std::io::Error::other("could not generate dummy authentication secret"))?;
        let dummy_password_hash = hash_password(dummy_token).await?;
        Ok(Self(Arc::new(AdminServiceInner {
            pool,
            public_origin,
            secure_cookies,
            master_key,
            setup_token_hash,
            dummy_password_hash,
            trust_forwarded_for,
            rate_limits: FailureRateLimiter::default(),
            argon2_slots: Arc::new(Semaphore::new(ARGON2_CONCURRENCY)),
            setup_slots: Arc::new(Semaphore::new(SETUP_CONCURRENCY)),
        })))
    }

    pub(crate) async fn public_providers(&self) -> Result<Vec<PublicProviderConfig>, BoxError> {
        let providers = self.list_providers().await?;
        Ok(providers
            .into_iter()
            .map(|provider| {
                let configured = provider.has_api_key;
                let is_deepseek = provider.kind == "deepseek";
                PublicProviderConfig {
                    id: provider.id,
                    kind: provider.kind,
                    name: provider.name,
                    available: provider.enabled && configured,
                    unavailable_reason: (!(provider.enabled && configured)).then(|| {
                        if configured {
                            "disabled".to_owned()
                        } else {
                            "not_configured".to_owned()
                        }
                    }),
                    models: provider
                        .models
                        .iter()
                        .map(|model| PublicProviderModel {
                            id: model.id.clone(),
                            name: model.name.clone(),
                        })
                        .collect(),
                    default_model: provider
                        .models
                        .iter()
                        .find(|model| model.is_default)
                        .map(|model| model.id.clone()),
                    thinking_modes: if is_deepseek {
                        vec!["enabled".to_owned(), "disabled".to_owned()]
                    } else {
                        Vec::new()
                    },
                    reasoning_efforts: if is_deepseek {
                        vec!["high".to_owned(), "max".to_owned()]
                    } else {
                        Vec::new()
                    },
                    default_reasoning_effort: is_deepseek.then(|| "high".to_owned()),
                }
            })
            .collect())
    }

    pub(crate) async fn config_session(&self, headers: &HeaderMap) -> Option<ConfigSession> {
        let principal = self.authenticate(headers).await.ok()?;
        let csrf_token = cookie_value(headers, self.csrf_cookie_name())
            .filter(|token| constant_eq(&hash_bytes(token.as_bytes()), &principal.csrf_hash));
        Some(ConfigSession { csrf_token })
    }

    pub(crate) async fn authorize_paid_job(
        &self,
        headers: &HeaderMap,
    ) -> Result<(), PaidJobAuthorizationError> {
        self.require_write(headers)
            .await
            .map(|_| ())
            .map_err(|error| match error {
                AdminError::Unauthorized => PaidJobAuthorizationError::Unauthorized,
                AdminError::Internal => PaidJobAuthorizationError::Internal,
                _ => PaidJobAuthorizationError::Forbidden,
            })
    }

    pub(crate) async fn resolve_runtime(
        &self,
        job_id: Uuid,
        provider_id: &str,
        model_id: &str,
    ) -> Result<RuntimeProvider, String> {
        // Keep the profile row, encrypted credential, and model membership on
        // one SQLite snapshot. Without an explicit read transaction an admin
        // update could commit between the profile and model SELECTs and create
        // a runtime assembled from two different revisions.
        let mut transaction = self
            .0
            .pool
            .begin()
            .await
            .map_err(|_| "无法开始翻译渠道配置快照".to_owned())?;
        let row = sqlx::query(
            "SELECT id, kind, display_name, base_url, revision, secret_version, \
                    api_key_ciphertext, api_key_nonce, encryption_key_id \
             FROM provider_profiles \
             WHERE id = ? AND enabled = 1 AND public_enabled = 1 AND deleted_at IS NULL",
        )
        .bind(provider_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| "无法读取翻译渠道配置".to_owned())?
        .ok_or_else(|| "翻译渠道未启用".to_owned())?;
        let kind: String = row.get("kind");
        let model_exists = if kind == "deepseek" {
            matches!(model_id, DEEPSEEK_MODEL_FLASH | DEEPSEEK_MODEL_PRO)
        } else {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM provider_models \
                 WHERE profile_id = ? AND upstream_model = ? AND enabled = 1",
            )
            .bind(provider_id)
            .bind(model_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|_| "无法读取模型配置".to_owned())?
                > 0
        };
        if !model_exists {
            return Err("所选模型不存在或未启用".to_owned());
        }
        let secret_version: i64 = row.get("secret_version");
        let ciphertext: Option<Vec<u8>> = row.get("api_key_ciphertext");
        let nonce: Option<Vec<u8>> = row.get("api_key_nonce");
        let key_id: Option<String> = row.get("encryption_key_id");
        let revision: i64 = row.get("revision");
        transaction
            .commit()
            .await
            .map_err(|_| "无法完成翻译渠道配置快照".to_owned())?;
        let api_key = self
            .decrypt_secret(
                provider_id,
                &kind,
                secret_version,
                ciphertext,
                nonce,
                key_id,
            )
            .map_err(|_| "翻译渠道密钥不可用".to_owned())?;
        let (sealed_api_key_ciphertext, sealed_api_key_nonce, sealed_key_id) = self
            .seal_runtime_secret(job_id, provider_id, &kind, revision, api_key.as_str())
            .map_err(|_| "无法保护任务凭据".to_owned())?;
        Ok(RuntimeProvider {
            provider_id: provider_id.to_owned(),
            provider_name: row.get("display_name"),
            kind,
            revision,
            model: model_id.to_owned(),
            base_url: row.get("base_url"),
            sealed_api_key_ciphertext,
            sealed_api_key_nonce,
            sealed_key_id,
        })
    }

    fn seal_runtime_secret(
        &self,
        job_id: Uuid,
        provider_id: &str,
        kind: &str,
        revision: i64,
        value: &str,
    ) -> Result<(Vec<u8>, Vec<u8>, &'static str), AdminError> {
        let master_key = self
            .0
            .master_key
            .as_ref()
            .ok_or(AdminError::MasterKeyUnavailable)?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(&master_key[..]).map_err(|_| AdminError::Internal)?;
        let mut nonce = [0_u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| AdminError::Internal)?;
        let aad = runtime_secret_aad(job_id, provider_id, kind, revision);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| AdminError::Internal)?;
        Ok((ciphertext, nonce.to_vec(), "primary-v1"))
    }

    pub(crate) fn open_runtime_secret(
        &self,
        job_id: Uuid,
        runtime: &RuntimeProvider,
    ) -> Result<Zeroizing<String>, String> {
        if runtime.sealed_key_id != "primary-v1" || runtime.sealed_api_key_nonce.len() != 24 {
            return Err("任务凭据封装元数据无效".to_owned());
        }
        let master_key = self
            .0
            .master_key
            .as_ref()
            .ok_or_else(|| "任务凭据加密主密钥不可用".to_owned())?;
        let cipher = XChaCha20Poly1305::new_from_slice(&master_key[..])
            .map_err(|_| "任务凭据加密主密钥无效".to_owned())?;
        let aad = runtime_secret_aad(
            job_id,
            &runtime.provider_id,
            &runtime.kind,
            runtime.revision,
        );
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(&runtime.sealed_api_key_nonce),
                Payload {
                    msg: &runtime.sealed_api_key_ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "任务凭据无法解封".to_owned())?;
        let plaintext = Zeroizing::new(plaintext);
        let value = std::str::from_utf8(plaintext.as_slice())
            .map_err(|_| "任务凭据内容无效".to_owned())?
            .to_owned();
        Ok(Zeroizing::new(value))
    }

    async fn setup_required(&self) -> Result<bool, AdminError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bootstrap_state")
            .fetch_one(&self.0.pool)
            .await
            .map_err(|_| AdminError::Internal)?;
        Ok(count == 0)
    }

    fn check_origin(&self, headers: &HeaderMap) -> Result<(), AdminError> {
        let origin = headers
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .ok_or(AdminError::Forbidden)?;
        if origin != self.0.public_origin {
            return Err(AdminError::Forbidden);
        }
        Ok(())
    }

    fn session_cookie_name(&self) -> &'static str {
        if self.0.secure_cookies {
            SESSION_COOKIE_SECURE
        } else {
            SESSION_COOKIE_INSECURE
        }
    }

    fn csrf_cookie_name(&self) -> &'static str {
        if self.0.secure_cookies {
            CSRF_COOKIE_SECURE
        } else {
            CSRF_COOKIE_INSECURE
        }
    }

    fn append_session_cookies(
        &self,
        response: &mut Response,
        session_token: &str,
        csrf_token: &str,
    ) -> Result<(), AdminError> {
        let secure = if self.0.secure_cookies {
            "; Secure"
        } else {
            ""
        };
        let session = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
            self.session_cookie_name(),
            session_token,
            SESSION_ABSOLUTE_SECONDS,
            secure
        );
        let csrf = format!(
            "{}={}; Path=/; SameSite=Strict; Max-Age={}{}",
            self.csrf_cookie_name(),
            csrf_token,
            SESSION_ABSOLUTE_SECONDS,
            secure
        );
        response.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_str(&session).map_err(|_| AdminError::Internal)?,
        );
        response.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_str(&csrf).map_err(|_| AdminError::Internal)?,
        );
        Ok(())
    }

    fn append_cleared_cookies(&self, response: &mut Response) -> Result<(), AdminError> {
        let secure = if self.0.secure_cookies {
            "; Secure"
        } else {
            ""
        };
        for name in [self.session_cookie_name(), self.csrf_cookie_name()] {
            let cookie = format!(
                "{name}=; Path=/; SameSite=Strict; Max-Age=0{}{}",
                if name == self.session_cookie_name() {
                    "; HttpOnly"
                } else {
                    ""
                },
                secure
            );
            response.headers_mut().append(
                SET_COOKIE,
                HeaderValue::from_str(&cookie).map_err(|_| AdminError::Internal)?,
            );
        }
        Ok(())
    }

    async fn authenticate(&self, headers: &HeaderMap) -> Result<AdminPrincipal, AdminError> {
        let token =
            cookie_value(headers, self.session_cookie_name()).ok_or(AdminError::Unauthorized)?;
        let token_hash = hash_bytes(token.as_bytes()).to_vec();
        let now = Utc::now().timestamp();
        let row = sqlx::query(
            "SELECT s.id AS session_id, s.csrf_hash, s.last_seen_at, s.absolute_expires_at, \
                    u.id AS user_id, u.username \
             FROM admin_sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ? AND s.revoked_at IS NULL \
               AND s.idle_expires_at > ? AND s.absolute_expires_at > ? \
               AND u.disabled_at IS NULL AND u.role = 'admin' \
               AND s.session_version = u.session_version",
        )
        .bind(token_hash)
        .bind(now)
        .bind(now)
        .fetch_optional(&self.0.pool)
        .await
        .map_err(|_| AdminError::Internal)?
        .ok_or(AdminError::Unauthorized)?;
        let session_id: String = row.get("session_id");
        let last_seen_at: i64 = row.get("last_seen_at");
        let absolute_expires_at: i64 = row.get("absolute_expires_at");
        if now.saturating_sub(last_seen_at) >= 300 {
            let idle_expires_at = now
                .saturating_add(SESSION_IDLE_SECONDS)
                .min(absolute_expires_at);
            let _ = sqlx::query(
                "UPDATE admin_sessions SET last_seen_at = ?, idle_expires_at = ? WHERE id = ?",
            )
            .bind(now)
            .bind(idle_expires_at)
            .bind(&session_id)
            .execute(&self.0.pool)
            .await;
        }
        Ok(AdminPrincipal {
            session_id,
            user_id: row.get("user_id"),
            username: row.get("username"),
            csrf_hash: row.get("csrf_hash"),
        })
    }

    async fn require_write(&self, headers: &HeaderMap) -> Result<AdminPrincipal, AdminError> {
        let principal = self.authenticate(headers).await?;
        self.check_origin(headers)?;
        let cookie = cookie_value(headers, self.csrf_cookie_name()).ok_or(AdminError::Forbidden)?;
        let header = headers
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok())
            .ok_or(AdminError::Forbidden)?;
        if !constant_eq(cookie.as_bytes(), header.as_bytes())
            || !constant_eq(&hash_bytes(header.as_bytes()), &principal.csrf_hash)
        {
            return Err(AdminError::Forbidden);
        }
        Ok(principal)
    }

    fn request_source(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
        if self.0.trust_forwarded_for
            && let Some(address) = forwarded_client_ip(headers)
        {
            return address.to_string();
        }
        peer.map(|address| address.ip().to_string())
            .unwrap_or_else(|| "global".to_owned())
    }

    fn check_rate_limits(&self, keys: &[String]) -> Result<(), AdminError> {
        self.0.rate_limits.check(keys)
    }

    fn record_rate_limit_failure(&self, keys: &[String]) -> Result<(), AdminError> {
        self.0.rate_limits.record_failure(keys)
    }

    fn clear_rate_limits(&self, keys: &[String]) {
        self.0.rate_limits.clear(keys);
    }

    fn encrypt_secret(
        &self,
        profile_id: &str,
        kind: &str,
        version: i64,
        value: String,
    ) -> Result<EncryptedSecret, AdminError> {
        let master_key = self
            .0
            .master_key
            .as_ref()
            .ok_or(AdminError::MasterKeyUnavailable)?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(&master_key[..]).map_err(|_| AdminError::Internal)?;
        let mut nonce = [0_u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| AdminError::Internal)?;
        let aad = secret_aad(profile_id, kind, version);
        let value = Zeroizing::new(value);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| AdminError::Internal)?;
        Ok(EncryptedSecret {
            ciphertext,
            nonce: nonce.to_vec(),
            key_id: "primary-v1",
            version,
        })
    }

    fn decrypt_secret(
        &self,
        profile_id: &str,
        kind: &str,
        version: i64,
        ciphertext: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
        key_id: Option<String>,
    ) -> Result<Zeroizing<String>, AdminError> {
        let ciphertext = ciphertext.ok_or(AdminError::MasterKeyUnavailable)?;
        let nonce = nonce.ok_or(AdminError::Internal)?;
        if key_id.as_deref() != Some("primary-v1") || nonce.len() != 24 {
            return Err(AdminError::Internal);
        }
        let master_key = self
            .0
            .master_key
            .as_ref()
            .ok_or(AdminError::MasterKeyUnavailable)?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(&master_key[..]).map_err(|_| AdminError::Internal)?;
        let aad = secret_aad(profile_id, kind, version);
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| AdminError::Internal)?;
        let plaintext = Zeroizing::new(plaintext);
        let value = std::str::from_utf8(plaintext.as_slice())
            .map_err(|_| AdminError::Internal)?
            .to_owned();
        Ok(Zeroizing::new(value))
    }

    async fn list_providers(&self) -> Result<Vec<AdminProvider>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, kind, display_name, base_url, enabled, revision, \
                    api_key_ciphertext IS NOT NULL AS has_api_key \
             FROM provider_profiles WHERE deleted_at IS NULL ORDER BY is_builtin DESC, created_at, id",
        )
        .fetch_all(&self.0.pool)
        .await?;
        let mut providers = Vec::with_capacity(rows.len().saturating_add(1));
        let mut saw_deepseek = false;
        for row in rows {
            let id: String = row.get("id");
            let kind: String = row.get("kind");
            let models = if kind == "deepseek" {
                saw_deepseek = true;
                deepseek_models()
            } else {
                let model_rows = sqlx::query(
                    "SELECT upstream_model, display_name, is_default FROM provider_models \
                     WHERE profile_id = ? AND enabled = 1 ORDER BY sort_order, upstream_model",
                )
                .bind(&id)
                .fetch_all(&self.0.pool)
                .await?;
                model_rows
                    .into_iter()
                    .map(|model| AdminProviderModel {
                        id: model.get("upstream_model"),
                        name: model.get("display_name"),
                        is_default: model.get::<i64, _>("is_default") != 0,
                    })
                    .collect()
            };
            providers.push(AdminProvider {
                id,
                kind,
                name: row.get("display_name"),
                base_url: row.get("base_url"),
                enabled: row.get::<i64, _>("enabled") != 0,
                revision: row.get("revision"),
                has_api_key: row.get::<i64, _>("has_api_key") != 0,
                models,
            });
        }
        if !saw_deepseek {
            providers.insert(
                0,
                AdminProvider {
                    id: "deepseek".to_owned(),
                    kind: "deepseek".to_owned(),
                    name: "DeepSeek".to_owned(),
                    base_url: DEEPSEEK_BASE_URL.to_owned(),
                    enabled: false,
                    revision: 0,
                    has_api_key: false,
                    models: deepseek_models(),
                },
            );
        }
        Ok(providers)
    }
}

impl FailureRateLimiter {
    fn check(&self, keys: &[String]) -> Result<(), AdminError> {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_rate_buckets(&mut buckets, now);
        if keys.iter().any(|key| {
            buckets
                .get(key)
                .is_some_and(|attempts| attempts.len() >= LOGIN_ATTEMPTS)
        }) {
            return Err(AdminError::RateLimited);
        }
        let new_keys = keys
            .iter()
            .map(String::as_str)
            .filter(|key| !buckets.contains_key(*key))
            .collect::<HashSet<_>>()
            .len();
        if buckets.len().saturating_add(new_keys) > MAX_RATE_BUCKETS {
            return Err(AdminError::RateLimited);
        }
        Ok(())
    }

    fn record_failure(&self, keys: &[String]) -> Result<(), AdminError> {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_rate_buckets(&mut buckets, now);
        let unique_keys = keys.iter().map(String::as_str).collect::<HashSet<_>>();
        let new_keys = unique_keys
            .iter()
            .filter(|key| !buckets.contains_key(**key))
            .count();
        if buckets.len().saturating_add(new_keys) > MAX_RATE_BUCKETS {
            return Err(AdminError::RateLimited);
        }
        for key in unique_keys {
            buckets.entry(key.to_owned()).or_default().push(now);
        }
        Ok(())
    }

    fn clear(&self, keys: &[String]) {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in keys {
            buckets.remove(key);
        }
    }
}

fn prune_rate_buckets(buckets: &mut HashMap<String, Vec<Instant>>, now: Instant) {
    buckets.retain(|_, attempts| {
        attempts.retain(|attempt| now.duration_since(*attempt) <= LOGIN_WINDOW);
        !attempts.is_empty()
    });
}

fn forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/session", get(session))
        .route("/setup", post(setup))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/providers", get(list_providers).post(create_provider))
        .route("/providers/deepseek", put(put_deepseek))
        .route(
            "/providers/{id}",
            put(update_provider).delete(delete_provider),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
}

async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let service = &state.0.admin;
    let setup_required = service.setup_required().await?;
    let principal = match service.authenticate(&headers).await {
        Ok(principal) => Some(principal),
        Err(AdminError::Unauthorized) => None,
        Err(error) => return Err(error),
    };
    let mut set_csrf = None;
    let csrf_token = if let Some(principal) = principal.as_ref() {
        let current = cookie_value(&headers, service.csrf_cookie_name());
        if current
            .as_ref()
            .is_some_and(|value| constant_eq(&hash_bytes(value.as_bytes()), &principal.csrf_hash))
        {
            current
        } else {
            let token = random_token(32).map_err(|_| AdminError::Internal)?;
            let hash = hash_bytes(token.as_bytes()).to_vec();
            sqlx::query("UPDATE admin_sessions SET csrf_hash = ? WHERE id = ?")
                .bind(hash)
                .bind(&principal.session_id)
                .execute(&service.0.pool)
                .await
                .map_err(|_| AdminError::Internal)?;
            set_csrf = Some(token.clone());
            Some(token)
        }
    } else {
        None
    };
    let payload = SessionResponse {
        setup_required,
        setup_token_required: service.0.setup_token_hash.is_some(),
        authenticated: principal.is_some(),
        username: principal.as_ref().map(|item| item.username.clone()),
        csrf_token,
    };
    let mut response = Json(payload).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    if let Some(csrf) = set_csrf {
        let secure = if service.0.secure_cookies {
            "; Secure"
        } else {
            ""
        };
        let cookie = format!(
            "{}={csrf}; Path=/; SameSite=Strict; Max-Age={}{}",
            service.csrf_cookie_name(),
            SESSION_ABSOLUTE_SECONDS,
            secure
        );
        response.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_str(&cookie).map_err(|_| AdminError::Internal)?,
        );
    }
    Ok(response)
}

async fn setup(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(request): Json<SetupRequest>,
) -> Result<Response, AdminError> {
    let service = &state.0.admin;
    service.check_origin(&headers)?;
    let peer = peer.map(|Extension(ConnectInfo(address))| address);
    let source = service.request_source(&headers, peer);
    let setup_rate_keys = [format!("setup:source:{source}")];
    service.check_rate_limits(&setup_rate_keys)?;
    if !service.setup_required().await? {
        return Err(AdminError::SetupComplete);
    }
    if let Some(expected_hash) = service.0.setup_token_hash {
        let supplied = request.setup_token.as_deref().unwrap_or("");
        if !constant_eq(&hash_bytes(supplied.as_bytes()), &expected_hash) {
            service.record_rate_limit_failure(&setup_rate_keys)?;
            return Err(AdminError::Unauthorized);
        }
    }
    let username_norm = validate_username(&request.username)?;
    validate_password(&request.password)?;
    let _setup_permit = service
        .0
        .setup_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AdminError::RateLimited)?;
    if !service.setup_required().await? {
        return Err(AdminError::SetupComplete);
    }
    let argon2_permit = service
        .0
        .argon2_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| AdminError::RateLimited)?;
    let password_hash = hash_password(request.password)
        .await
        .map_err(|_| AdminError::Internal)?;
    drop(argon2_permit);
    let user_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let new_session = new_session(now).map_err(|_| AdminError::Internal)?;
    let mut transaction = service
        .0
        .pool
        .begin()
        .await
        .map_err(|_| AdminError::Internal)?;
    let claimed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bootstrap_state")
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| AdminError::Internal)?;
    if claimed != 0 {
        return Err(AdminError::SetupComplete);
    }
    sqlx::query(
        "INSERT INTO users \
         (id, username, username_norm, password_hash, role, session_version, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'admin', 1, ?, ?)",
    )
    .bind(&user_id)
    .bind(request.username.trim())
    .bind(username_norm)
    .bind(password_hash)
    .bind(now)
    .bind(now)
    .execute(&mut *transaction)
    .await
    .map_err(|_| AdminError::SetupComplete)?;
    sqlx::query(
        "INSERT INTO bootstrap_state (singleton, admin_user_id, completed_at) VALUES (1, ?, ?)",
    )
    .bind(&user_id)
    .bind(now)
    .execute(&mut *transaction)
    .await
    .map_err(|_| AdminError::SetupComplete)?;
    insert_session(&mut transaction, &new_session, &user_id, 1).await?;
    transaction
        .commit()
        .await
        .map_err(|_| AdminError::Internal)?;
    service.clear_rate_limits(&setup_rate_keys);
    let mut response = (
        StatusCode::CREATED,
        Json(SessionResponse {
            setup_required: false,
            setup_token_required: service.0.setup_token_hash.is_some(),
            authenticated: true,
            username: Some(request.username.trim().to_owned()),
            csrf_token: Some(new_session.csrf_token.clone()),
        }),
    )
        .into_response();
    service.append_session_cookies(&mut response, &new_session.token, &new_session.csrf_token)?;
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

async fn login(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, AdminError> {
    let service = &state.0.admin;
    service.check_origin(&headers)?;
    let username_norm = validate_username(&request.username)?;
    validate_password(&request.password)?;
    let peer = peer.map(|Extension(ConnectInfo(address))| address);
    let source = service.request_source(&headers, peer);
    let rate_keys = [
        format!("login:source:{source}"),
        format!("login:source-username:{source}:{username_norm}"),
    ];
    service.check_rate_limits(&rate_keys)?;
    let row = sqlx::query(
        "SELECT id, username, password_hash, session_version \
         FROM users WHERE username_norm = ? AND role = 'admin' AND disabled_at IS NULL",
    )
    .bind(&username_norm)
    .fetch_optional(&service.0.pool)
    .await
    .map_err(|_| AdminError::Internal)?;
    let candidate_hash = row
        .as_ref()
        .map(|item| item.get::<String, _>("password_hash"))
        .unwrap_or_else(|| service.0.dummy_password_hash.clone());
    let argon2_permit = service
        .0
        .argon2_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| AdminError::RateLimited)?;
    let verified = verify_password(request.password, candidate_hash)
        .await
        .map_err(|_| AdminError::Internal)?;
    drop(argon2_permit);
    let Some(row) = row.filter(|_| verified) else {
        service.record_rate_limit_failure(&rate_keys)?;
        return Err(AdminError::Unauthorized);
    };
    let user_id: String = row.get("id");
    let username: String = row.get("username");
    let session_version: i64 = row.get("session_version");
    let now = Utc::now().timestamp();
    let new_session = new_session(now).map_err(|_| AdminError::Internal)?;
    let mut transaction = service
        .0
        .pool
        .begin()
        .await
        .map_err(|_| AdminError::Internal)?;
    insert_session(&mut transaction, &new_session, &user_id, session_version).await?;
    transaction
        .commit()
        .await
        .map_err(|_| AdminError::Internal)?;
    service.clear_rate_limits(&rate_keys);
    let mut response = Json(SessionResponse {
        setup_required: false,
        setup_token_required: service.0.setup_token_hash.is_some(),
        authenticated: true,
        username: Some(username),
        csrf_token: Some(new_session.csrf_token.clone()),
    })
    .into_response();
    service.append_session_cookies(&mut response, &new_session.token, &new_session.csrf_token)?;
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AdminError> {
    let service = &state.0.admin;
    let principal = service.require_write(&headers).await?;
    sqlx::query("UPDATE admin_sessions SET revoked_at = ? WHERE id = ?")
        .bind(Utc::now().timestamp())
        .bind(principal.session_id)
        .execute(&service.0.pool)
        .await
        .map_err(|_| AdminError::Internal)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    service.append_cleared_cookies(&mut response)?;
    Ok(response)
}

async fn list_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ProvidersResponse>, AdminError> {
    state.0.admin.authenticate(&headers).await?;
    let providers = state
        .0
        .admin
        .list_providers()
        .await
        .map_err(|_| AdminError::Internal)?;
    Ok(Json(ProvidersResponse { providers }))
}

async fn put_deepseek(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PutDeepSeekRequest>,
) -> Result<Json<AdminProvider>, AdminError> {
    let service = &state.0.admin;
    let principal = service.require_write(&headers).await?;
    if request.expected_revision < 0
        || request.kind != "deepseek"
        || request.name.trim() != "DeepSeek"
        || request.base_url.trim_end_matches('/') != DEEPSEEK_BASE_URL
        || request.models != deepseek_models()
    {
        return Err(AdminError::BadRequest);
    }
    validate_secret_write(
        request.api_key.as_deref(),
        request.clear_api_key,
        request.enabled,
    )?;
    let existing = sqlx::query(
        "SELECT revision, secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id \
         FROM provider_profiles WHERE id = 'deepseek' AND deleted_at IS NULL",
    )
    .fetch_optional(&service.0.pool)
    .await
    .map_err(|_| AdminError::Internal)?;
    let current_revision = existing
        .as_ref()
        .map(|row| row.get::<i64, _>("revision"))
        .unwrap_or(0);
    if current_revision != request.expected_revision {
        return Err(AdminError::RevisionConflict);
    }
    let current_secret_version = existing
        .as_ref()
        .map(|row| row.get::<i64, _>("secret_version"))
        .unwrap_or(0);
    let next_revision = current_revision.saturating_add(1);
    let next_secret_version = current_secret_version.saturating_add(1);
    let encrypted = match (request.api_key, request.clear_api_key) {
        (_, true) => None,
        (Some(value), false) => Some(service.encrypt_secret(
            "deepseek",
            "deepseek",
            next_secret_version,
            value.trim().to_owned(),
        )?),
        (None, false) => existing.as_ref().and_then(|row| {
            let ciphertext: Option<Vec<u8>> = row.get("api_key_ciphertext");
            ciphertext.map(|ciphertext| EncryptedSecret {
                ciphertext,
                nonce: row.get("api_key_nonce"),
                key_id: "primary-v1",
                version: current_secret_version,
            })
        }),
    };
    if request.enabled && encrypted.is_none() {
        return Err(AdminError::BadRequest);
    }
    let now = Utc::now().timestamp();
    let mut tx = service
        .0
        .pool
        .begin()
        .await
        .map_err(|_| AdminError::Internal)?;
    let result = sqlx::query(
        "INSERT INTO provider_profiles \
         (id, slug, kind, display_name, base_url, enabled, public_enabled, is_builtin, revision, \
          secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id, created_by, updated_by, created_at, updated_at) \
         VALUES ('deepseek', 'deepseek', 'deepseek', 'DeepSeek', ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled, public_enabled = excluded.public_enabled, \
           revision = excluded.revision, secret_version = excluded.secret_version, \
           api_key_ciphertext = excluded.api_key_ciphertext, api_key_nonce = excluded.api_key_nonce, \
           encryption_key_id = excluded.encryption_key_id, updated_by = excluded.updated_by, updated_at = excluded.updated_at \
         WHERE provider_profiles.revision = ? AND provider_profiles.deleted_at IS NULL",
    )
    .bind(DEEPSEEK_BASE_URL)
    .bind(bool_i64(request.enabled))
    .bind(bool_i64(request.enabled))
    .bind(next_revision)
    .bind(encrypted.as_ref().map(|secret| secret.version).unwrap_or(0))
    .bind(encrypted.as_ref().map(|secret| secret.ciphertext.clone()))
    .bind(encrypted.as_ref().map(|secret| secret.nonce.clone()))
    .bind(encrypted.as_ref().map(|secret| secret.key_id))
    .bind(&principal.user_id)
    .bind(&principal.user_id)
    .bind(now)
    .bind(now)
    .bind(current_revision)
    .execute(&mut *tx)
    .await
    .map_err(|_| AdminError::Internal)?;
    if result.rows_affected() != 1 {
        return Err(AdminError::RevisionConflict);
    }
    insert_provider_version(
        &mut tx,
        "deepseek",
        next_revision,
        "deepseek",
        "DeepSeek",
        DEEPSEEK_BASE_URL,
        &deepseek_models(),
        now,
    )
    .await?;
    audit(&mut tx, &principal.user_id, "provider.update", "deepseek").await?;
    tx.commit().await.map_err(|_| AdminError::Internal)?;
    Ok(Json(provider_by_id(service, "deepseek").await?))
}

async fn create_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateProviderRequest>,
) -> Result<(StatusCode, Json<AdminProvider>), AdminError> {
    let service = &state.0.admin;
    let principal = service.require_write(&headers).await?;
    if request.kind != "openai_compatible"
        || request
            .expected_revision
            .is_some_and(|revision| revision != 0)
    {
        return Err(AdminError::BadRequest);
    }
    validate_secret_write(
        request.api_key.as_deref(),
        request.clear_api_key,
        request.enabled,
    )?;
    let name = validate_display_name(&request.name)?;
    let base_url = validate_openai_base_url(&request.base_url)?;
    validate_models(&request.models)?;
    let id = Uuid::new_v4().to_string();
    let encrypted = match (request.api_key, request.clear_api_key) {
        (Some(value), false) => {
            Some(service.encrypt_secret(&id, "openai_compatible", 1, value.trim().to_owned())?)
        }
        _ => None,
    };
    if request.enabled && encrypted.is_none() {
        return Err(AdminError::BadRequest);
    }
    let now = Utc::now().timestamp();
    let mut tx = service
        .0
        .pool
        .begin()
        .await
        .map_err(|_| AdminError::Internal)?;
    sqlx::query(
        "INSERT INTO provider_profiles \
         (id, slug, kind, display_name, base_url, enabled, public_enabled, is_builtin, revision, \
          secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id, created_by, updated_by, created_at, updated_at) \
         VALUES (?, ?, 'openai_compatible', ?, ?, ?, ?, 0, 1, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&id)
    .bind(&name)
    .bind(&base_url)
    .bind(bool_i64(request.enabled))
    .bind(bool_i64(request.enabled))
    .bind(encrypted.as_ref().map(|secret| secret.version).unwrap_or(0))
    .bind(encrypted.as_ref().map(|secret| secret.ciphertext.clone()))
    .bind(encrypted.as_ref().map(|secret| secret.nonce.clone()))
    .bind(encrypted.as_ref().map(|secret| secret.key_id))
    .bind(&principal.user_id)
    .bind(&principal.user_id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|_| AdminError::Internal)?;
    replace_models(&mut tx, &id, &request.models).await?;
    insert_provider_version(
        &mut tx,
        &id,
        1,
        "openai_compatible",
        &name,
        &base_url,
        &request.models,
        now,
    )
    .await?;
    audit(&mut tx, &principal.user_id, "provider.create", &id).await?;
    tx.commit().await.map_err(|_| AdminError::Internal)?;
    Ok((
        StatusCode::CREATED,
        Json(provider_by_id(service, &id).await?),
    ))
}

async fn update_provider(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<UpdateProviderRequest>,
) -> Result<Json<AdminProvider>, AdminError> {
    if id == "deepseek" {
        return Err(AdminError::BadRequest);
    }
    let service = &state.0.admin;
    let principal = service.require_write(&headers).await?;
    if request.kind != "openai_compatible" {
        return Err(AdminError::BadRequest);
    }
    validate_secret_write(
        request.api_key.as_deref(),
        request.clear_api_key,
        request.enabled,
    )?;
    let name = validate_display_name(&request.name)?;
    let base_url = validate_openai_base_url(&request.base_url)?;
    validate_models(&request.models)?;
    let existing = sqlx::query(
        "SELECT revision, secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id \
         FROM provider_profiles WHERE id = ? AND kind = 'openai_compatible' AND deleted_at IS NULL",
    )
    .bind(&id)
    .fetch_optional(&service.0.pool)
    .await
    .map_err(|_| AdminError::Internal)?
    .ok_or(AdminError::NotFound)?;
    let current_revision: i64 = existing.get("revision");
    if current_revision != request.expected_revision {
        return Err(AdminError::RevisionConflict);
    }
    let current_secret_version: i64 = existing.get("secret_version");
    let encrypted = match (request.api_key, request.clear_api_key) {
        (Some(value), false) => Some(service.encrypt_secret(
            &id,
            "openai_compatible",
            current_secret_version.saturating_add(1),
            value.trim().to_owned(),
        )?),
        (_, true) => None,
        (None, false) => {
            let ciphertext: Option<Vec<u8>> = existing.get("api_key_ciphertext");
            ciphertext.map(|ciphertext| EncryptedSecret {
                ciphertext,
                nonce: existing.get("api_key_nonce"),
                key_id: "primary-v1",
                version: current_secret_version,
            })
        }
    };
    if request.enabled && encrypted.is_none() {
        return Err(AdminError::BadRequest);
    }
    let next_revision = current_revision.saturating_add(1);
    let now = Utc::now().timestamp();
    let mut tx = service
        .0
        .pool
        .begin()
        .await
        .map_err(|_| AdminError::Internal)?;
    let result = sqlx::query(
        "UPDATE provider_profiles SET display_name = ?, base_url = ?, enabled = ?, public_enabled = ?, \
         revision = ?, secret_version = ?, api_key_ciphertext = ?, api_key_nonce = ?, encryption_key_id = ?, \
         updated_by = ?, updated_at = ? WHERE id = ? AND revision = ? AND deleted_at IS NULL",
    )
    .bind(&name)
    .bind(&base_url)
    .bind(bool_i64(request.enabled))
    .bind(bool_i64(request.enabled))
    .bind(next_revision)
    .bind(encrypted.as_ref().map(|secret| secret.version).unwrap_or(0))
    .bind(encrypted.as_ref().map(|secret| secret.ciphertext.clone()))
    .bind(encrypted.as_ref().map(|secret| secret.nonce.clone()))
    .bind(encrypted.as_ref().map(|secret| secret.key_id))
    .bind(&principal.user_id)
    .bind(now)
    .bind(&id)
    .bind(current_revision)
    .execute(&mut *tx)
    .await
    .map_err(|_| AdminError::Internal)?;
    if result.rows_affected() != 1 {
        return Err(AdminError::RevisionConflict);
    }
    replace_models(&mut tx, &id, &request.models).await?;
    insert_provider_version(
        &mut tx,
        &id,
        next_revision,
        "openai_compatible",
        &name,
        &base_url,
        &request.models,
        now,
    )
    .await?;
    audit(&mut tx, &principal.user_id, "provider.update", &id).await?;
    tx.commit().await.map_err(|_| AdminError::Internal)?;
    Ok(Json(provider_by_id(service, &id).await?))
}

async fn delete_provider(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DeleteProviderQuery>,
    headers: HeaderMap,
) -> Result<StatusCode, AdminError> {
    if id == "deepseek" {
        return Err(AdminError::BadRequest);
    }
    let service = &state.0.admin;
    let principal = service.require_write(&headers).await?;
    let now = Utc::now().timestamp();
    let mut tx = service
        .0
        .pool
        .begin()
        .await
        .map_err(|_| AdminError::Internal)?;
    let result = sqlx::query(
        "UPDATE provider_profiles SET enabled = 0, public_enabled = 0, deleted_at = ?, \
         revision = revision + 1, secret_version = 0, api_key_ciphertext = NULL, \
         api_key_nonce = NULL, encryption_key_id = NULL, updated_by = ?, updated_at = ? \
         WHERE id = ? AND kind = 'openai_compatible' AND revision = ? AND deleted_at IS NULL",
    )
    .bind(now)
    .bind(&principal.user_id)
    .bind(now)
    .bind(&id)
    .bind(query.expected_revision)
    .execute(&mut *tx)
    .await
    .map_err(|_| AdminError::Internal)?;
    if result.rows_affected() != 1 {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM provider_profiles WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| AdminError::Internal)?;
        return Err(if exists == 0 {
            AdminError::NotFound
        } else {
            AdminError::RevisionConflict
        });
    }
    audit(&mut tx, &principal.user_id, "provider.delete", &id).await?;
    tx.commit().await.map_err(|_| AdminError::Internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn provider_by_id(service: &AdminService, id: &str) -> Result<AdminProvider, AdminError> {
    service
        .list_providers()
        .await
        .map_err(|_| AdminError::Internal)?
        .into_iter()
        .find(|provider| provider.id == id)
        .ok_or(AdminError::NotFound)
}

async fn replace_models(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    profile_id: &str,
    models: &[AdminProviderModel],
) -> Result<(), AdminError> {
    sqlx::query("DELETE FROM provider_models WHERE profile_id = ?")
        .bind(profile_id)
        .execute(&mut **transaction)
        .await
        .map_err(|_| AdminError::Internal)?;
    for (index, model) in models.iter().enumerate() {
        sqlx::query(
            "INSERT INTO provider_models \
             (id, profile_id, upstream_model, display_name, enabled, is_default, sort_order) \
             VALUES (?, ?, ?, ?, 1, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(profile_id)
        .bind(model.id.trim())
        .bind(model.name.trim())
        .bind(bool_i64(model.is_default))
        .bind(i64::try_from(index).unwrap_or(i64::MAX))
        .execute(&mut **transaction)
        .await
        .map_err(|_| AdminError::Internal)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_provider_version(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    profile_id: &str,
    revision: i64,
    kind: &str,
    display_name: &str,
    base_url: &str,
    models: &[AdminProviderModel],
    created_at: i64,
) -> Result<(), AdminError> {
    let models_json = serde_json::to_string(models).map_err(|_| AdminError::Internal)?;
    sqlx::query(
        "INSERT INTO provider_versions \
         (profile_id, revision, kind, display_name, base_url, secret_version, \
           api_key_ciphertext, api_key_nonce, encryption_key_id, models_json, created_at) \
         VALUES (?, ?, ?, ?, ?, 0, NULL, NULL, NULL, ?, ?)",
    )
    .bind(profile_id)
    .bind(revision)
    .bind(kind)
    .bind(display_name)
    .bind(base_url)
    .bind(models_json)
    .bind(created_at)
    .execute(&mut **transaction)
    .await
    .map_err(|_| AdminError::Internal)?;
    Ok(())
}

async fn audit(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor_user_id: &str,
    action: &str,
    target_id: &str,
) -> Result<(), AdminError> {
    sqlx::query(
        "INSERT INTO admin_audit_log \
         (occurred_at, actor_user_id, action, target_type, target_id, outcome) \
         VALUES (?, ?, ?, 'provider', ?, 'success')",
    )
    .bind(Utc::now().timestamp())
    .bind(actor_user_id)
    .bind(action)
    .bind(target_id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| AdminError::Internal)?;
    Ok(())
}

async fn insert_session(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &NewSession,
    user_id: &str,
    session_version: i64,
) -> Result<(), AdminError> {
    sqlx::query(
        "INSERT INTO admin_sessions \
         (id, token_hash, csrf_hash, user_id, session_version, created_at, last_seen_at, \
          idle_expires_at, absolute_expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&session.id)
    .bind(&session.token_hash)
    .bind(&session.csrf_hash)
    .bind(user_id)
    .bind(session_version)
    .bind(session.created_at)
    .bind(session.created_at)
    .bind(session.idle_expires_at)
    .bind(session.absolute_expires_at)
    .execute(&mut **transaction)
    .await
    .map_err(|_| AdminError::Internal)?;
    Ok(())
}

fn new_session(now: i64) -> Result<NewSession, getrandom::Error> {
    let token = random_token(32)?;
    let csrf_token = random_token(32)?;
    Ok(NewSession {
        id: Uuid::new_v4().to_string(),
        token_hash: hash_bytes(token.as_bytes()).to_vec(),
        csrf_hash: hash_bytes(csrf_token.as_bytes()).to_vec(),
        token,
        csrf_token,
        created_at: now,
        idle_expires_at: now.saturating_add(SESSION_IDLE_SECONDS),
        absolute_expires_at: now.saturating_add(SESSION_ABSOLUTE_SECONDS),
    })
}

fn validate_username(value: &str) -> Result<String, AdminError> {
    let value = value.trim();
    if !(3..=32).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(AdminError::BadRequest);
    }
    Ok(normalize_username(value))
}

fn normalize_username(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn validate_password(value: &str) -> Result<(), AdminError> {
    if !(12..=128).contains(&value.chars().count()) || value.len() > 512 {
        return Err(AdminError::BadRequest);
    }
    Ok(())
}

fn validate_display_name(value: &str) -> Result<String, AdminError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 64 || value.chars().any(char::is_control) {
        return Err(AdminError::BadRequest);
    }
    Ok(value.to_owned())
}

fn validate_models(models: &[AdminProviderModel]) -> Result<(), AdminError> {
    if models.is_empty() || models.len() > 20 {
        return Err(AdminError::BadRequest);
    }
    let mut ids = HashSet::new();
    let mut defaults = 0;
    for model in models {
        let id = model.id.trim();
        let name = model.name.trim();
        if id.is_empty()
            || id.len() > 128
            || id.chars().any(char::is_control)
            || name.is_empty()
            || name.chars().count() > 64
            || name.chars().any(char::is_control)
            || !ids.insert(id.to_owned())
        {
            return Err(AdminError::BadRequest);
        }
        if model.is_default {
            defaults += 1;
        }
    }
    if defaults != 1 {
        return Err(AdminError::BadRequest);
    }
    Ok(())
}

fn validate_secret_write(
    api_key: Option<&str>,
    clear_api_key: bool,
    enabled: bool,
) -> Result<(), AdminError> {
    if clear_api_key && api_key.is_some() {
        return Err(AdminError::BadRequest);
    }
    if clear_api_key && enabled {
        return Err(AdminError::BadRequest);
    }
    if let Some(api_key) = api_key {
        let api_key = api_key.trim();
        if api_key.is_empty() || api_key.len() > 4096 || api_key.chars().any(char::is_control) {
            return Err(AdminError::BadRequest);
        }
    }
    Ok(())
}

fn validate_openai_base_url(value: &str) -> Result<String, AdminError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return Err(AdminError::BadRequest);
    }
    let mut url = Url::parse(value).map_err(|_| AdminError::BadRequest)?;
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AdminError::BadRequest);
    }
    match url.host().ok_or(AdminError::BadRequest)? {
        Host::Ipv4(ip) => {
            if ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.is_broadcast()
            {
                return Err(AdminError::BadRequest);
            }
        }
        Host::Ipv6(ip) => {
            if ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
            {
                return Err(AdminError::BadRequest);
            }
        }
        Host::Domain(domain) => {
            if domain.eq_ignore_ascii_case("localhost") || domain.ends_with(".localhost") {
                return Err(AdminError::BadRequest);
            }
        }
    }
    let normalized_path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if normalized_path.is_empty() {
        "/"
    } else {
        &normalized_path
    });
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn deepseek_models() -> Vec<AdminProviderModel> {
    vec![
        AdminProviderModel {
            id: DEEPSEEK_MODEL_FLASH.to_owned(),
            name: "DeepSeek V4 Flash".to_owned(),
            is_default: true,
        },
        AdminProviderModel {
            id: DEEPSEEK_MODEL_PRO.to_owned(),
            name: "DeepSeek V4 Pro".to_owned(),
            is_default: false,
        },
    ]
}

async fn load_master_key(path: PathBuf) -> Result<Option<Arc<Zeroizing<[u8; 32]>>>, BoxError> {
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let decoded = STANDARD
        .decode(text.trim())
        .map_err(|_| "master key must be a base64-encoded 32-byte value")?;
    let key: [u8; 32] = decoded
        .try_into()
        .map_err(|_| "master key must decode to exactly 32 bytes")?;
    Ok(Some(Arc::new(Zeroizing::new(key))))
}

async fn verify_or_initialize_master_key(
    pool: &SqlitePool,
    master_key: Option<&[u8]>,
) -> Result<(), BoxError> {
    const VERIFIER_PLAINTEXT: &[u8] = b"pdf2zh credential master key verifier v1";
    const VERIFIER_AAD: &[u8] = b"pdf2zh/master-key-verifier/v1";

    let verifier = sqlx::query(
        "SELECT encryption_key_id, nonce, ciphertext FROM credential_key_verifier WHERE singleton = 1",
    )
    .fetch_optional(pool)
    .await?;
    let encrypted_profiles: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_profiles WHERE api_key_ciphertext IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    let Some(master_key) = master_key else {
        if verifier.is_some() || encrypted_profiles > 0 {
            return Err(
                "credential state is bound to a master key but PDF2ZH_MASTER_KEY_FILE is unavailable"
                    .into(),
            );
        }
        return Ok(());
    };
    if master_key.len() != 32 {
        return Err("credential master key must contain exactly 32 bytes".into());
    }
    let cipher = XChaCha20Poly1305::new_from_slice(master_key)
        .map_err(|_| "credential master key has an invalid length")?;

    let verifier_exists = verifier.is_some();
    if let Some(verifier) = verifier {
        let key_id: String = verifier.get("encryption_key_id");
        let nonce: Vec<u8> = verifier.get("nonce");
        let ciphertext: Vec<u8> = verifier.get("ciphertext");
        if key_id != "primary-v1" || nonce.len() != 24 {
            return Err("credential master key verifier is invalid".into());
        }
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: VERIFIER_AAD,
                },
            )
            .map_err(|_| "credential master key does not match persisted state")?;
        if !constant_eq(&plaintext, VERIFIER_PLAINTEXT) {
            return Err("credential master key verifier plaintext is invalid".into());
        }
    }

    // Validate every live credential, not merely one sample. Historical
    // provider revisions deliberately never retain credential ciphertext.
    let encrypted_rows = sqlx::query(
        "SELECT id AS profile_id, kind, secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id \
         FROM provider_profiles WHERE api_key_ciphertext IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    for row in encrypted_rows {
        let profile_id: String = row.get("profile_id");
        let kind: String = row.get("kind");
        let secret_version: i64 = row.get("secret_version");
        let ciphertext: Vec<u8> = row.get("api_key_ciphertext");
        let nonce: Vec<u8> = row.get("api_key_nonce");
        let key_id: String = row.get("encryption_key_id");
        if key_id != "primary-v1" || nonce.len() != 24 {
            return Err(format!("provider credential metadata is invalid for {profile_id}").into());
        }
        let aad = secret_aad(&profile_id, &kind, secret_version);
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| format!("provider credential cannot be decrypted for {profile_id}"))?;
        if plaintext.is_empty() || String::from_utf8(plaintext).is_err() {
            return Err(
                format!("provider credential plaintext is invalid for {profile_id}").into(),
            );
        }
    }

    if !verifier_exists {
        let mut nonce = [0_u8; 24];
        getrandom::fill(&mut nonce)
            .map_err(|_| std::io::Error::other("could not generate master key verifier nonce"))?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: VERIFIER_PLAINTEXT,
                    aad: VERIFIER_AAD,
                },
            )
            .map_err(|_| "could not create credential master key verifier")?;
        sqlx::query(
            "INSERT INTO credential_key_verifier \
             (singleton, encryption_key_id, nonce, ciphertext, created_at) \
             VALUES (1, 'primary-v1', ?, ?, ?)",
        )
        .bind(nonce.to_vec())
        .bind(ciphertext)
        .bind(Utc::now().timestamp())
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn secret_aad(profile_id: &str, kind: &str, version: i64) -> String {
    format!("pdf2zh/provider-secret/v1\0{profile_id}\0{kind}\0{version}")
}

fn runtime_secret_aad(job_id: Uuid, profile_id: &str, kind: &str, revision: i64) -> String {
    format!("pdf2zh/job-provider-secret/v1\0{job_id}\0{profile_id}\0{kind}\0{revision}")
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(candidate, value)| (candidate == name).then(|| value.to_owned()))
}

fn random_token(bytes: usize) -> Result<String, getrandom::Error> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value)?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn hash_bytes(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn bool_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

async fn hash_password(password: String) -> Result<String, BoxError> {
    let mut salt_bytes = [0_u8; 16];
    getrandom::fill(&mut salt_bytes)
        .map_err(|_| std::io::Error::other("could not generate password salt"))?;
    tokio::task::spawn_blocking(move || -> Result<String, BoxError> {
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|_| std::io::Error::other("could not encode password salt"))?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| std::io::Error::other("could not hash password"))?;
        Ok(hash.to_string())
    })
    .await?
}

async fn verify_password(password: String, password_hash: String) -> Result<bool, BoxError> {
    tokio::task::spawn_blocking(move || -> Result<bool, BoxError> {
        let parsed = PasswordHash::new(&password_hash)
            .map_err(|_| std::io::Error::other("stored password hash is invalid"))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    })
    .await?
}

fn parse_bool_env(name: &str, value: &str) -> Result<bool, BoxError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{name} must be true or false").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn rejects_private_literal_provider_urls() {
        assert!(validate_openai_base_url("").is_err());
        assert!(validate_openai_base_url(&format!("https://{}", "a".repeat(2048))).is_err());
        assert!(validate_openai_base_url("http://api.example.com/v1").is_err());
        assert!(validate_openai_base_url("https://127.0.0.1/v1").is_err());
        assert!(validate_openai_base_url("https://10.0.0.1/v1").is_err());
        assert!(validate_openai_base_url("https://[::1]/v1").is_err());
        assert_eq!(
            validate_openai_base_url("https://api.example.com/v1/").unwrap(),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn model_validation_requires_one_default_and_unique_ids() {
        let valid = vec![AdminProviderModel {
            id: "model-a".to_owned(),
            name: "Model A".to_owned(),
            is_default: true,
        }];
        assert!(validate_models(&valid).is_ok());
        let mut duplicate = valid.clone();
        duplicate.push(valid[0].clone());
        assert!(validate_models(&duplicate).is_err());
        let no_default = vec![AdminProviderModel {
            is_default: false,
            ..valid[0].clone()
        }];
        assert!(validate_models(&no_default).is_err());
    }

    #[test]
    fn deepseek_contract_uses_only_current_models_and_efforts() {
        let models = deepseek_models();
        assert_eq!(models[0].id, "deepseek-v4-flash");
        assert_eq!(models[1].id, "deepseek-v4-pro");
        assert!(models[0].is_default);
    }

    #[test]
    fn secret_write_contract_rejects_ambiguous_or_unsafe_changes() {
        assert!(validate_secret_write(Some("valid-secret"), false, true).is_ok());
        assert!(validate_secret_write(None, false, true).is_ok());
        assert!(validate_secret_write(Some(""), false, false).is_err());
        assert!(validate_secret_write(Some("valid-secret"), true, false).is_err());
        assert!(validate_secret_write(None, true, true).is_err());
    }

    #[test]
    fn authentication_input_bounds_apply_before_password_work() {
        assert!(validate_username("adm").is_ok());
        assert!(validate_username(&"a".repeat(33)).is_err());
        assert!(validate_password("twelve-chars!").is_ok());
        assert!(validate_password(&"密".repeat(12)).is_ok());
        assert!(validate_password("short").is_err());
        assert!(validate_password(&"密".repeat(11)).is_err());
        assert!(validate_password(&"p".repeat(129)).is_err());
        assert!(validate_password(&"密".repeat(129)).is_err());
    }

    #[test]
    fn login_failures_block_source_rotation_without_enabling_global_username_lockout() {
        let limiter = FailureRateLimiter::default();
        let source_key = "login:source:192.0.2.10".to_owned();
        let first_username = "login:source-username:192.0.2.10:first".to_owned();
        let first_keys = [source_key.clone(), first_username.clone()];
        for _ in 0..LOGIN_ATTEMPTS {
            limiter.check(&first_keys).unwrap();
            limiter.record_failure(&first_keys).unwrap();
        }
        assert!(matches!(
            limiter.check(&[
                source_key.clone(),
                "login:source-username:192.0.2.10:rotated".to_owned(),
            ]),
            Err(AdminError::RateLimited)
        ));

        for index in 0..LOGIN_ATTEMPTS {
            let source = format!("198.51.100.{index}");
            let keys = [
                format!("login:source:{source}"),
                format!("login:source-username:{source}:target"),
            ];
            limiter.check(&keys).unwrap();
            limiter.record_failure(&keys).unwrap();
        }
        assert!(
            limiter
                .check(&[
                    "login:source:203.0.113.20".to_owned(),
                    "login:source-username:203.0.113.20:target".to_owned(),
                ])
                .is_ok()
        );

        let setup_a = ["setup:source:192.0.2.20".to_owned()];
        let setup_b = ["setup:source:192.0.2.21".to_owned()];
        for _ in 0..LOGIN_ATTEMPTS {
            limiter.check(&setup_a).unwrap();
            limiter.record_failure(&setup_a).unwrap();
        }
        assert!(matches!(
            limiter.check(&setup_a),
            Err(AdminError::RateLimited)
        ));
        assert!(limiter.check(&setup_b).is_ok());

        limiter.clear(&first_keys);
        assert!(limiter.check(&first_keys).is_ok());
    }

    #[test]
    fn forwarded_source_requires_a_valid_address_and_password_work_is_globally_bounded() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.8, 10.0.0.2"),
        );
        assert_eq!(
            forwarded_client_ip(&headers),
            Some("203.0.113.8".parse().unwrap())
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        assert_eq!(forwarded_client_ip(&headers), None);

        let semaphore = Arc::new(Semaphore::new(ARGON2_CONCURRENCY));
        let first = semaphore.clone().try_acquire_owned().unwrap();
        let second = semaphore.clone().try_acquire_owned().unwrap();
        assert!(semaphore.clone().try_acquire_owned().is_err());
        drop((first, second));
        assert!(semaphore.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn serialized_setup_candidate_rechecks_completion_after_waiting() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let slots = Arc::new(Semaphore::new(SETUP_CONCURRENCY));
        let first = slots.clone().acquire_owned().await.unwrap();
        let setup_complete = Arc::new(AtomicBool::new(false));
        let waiting_slots = slots.clone();
        let waiting_complete = setup_complete.clone();
        let loser = tokio::spawn(async move {
            let _permit = waiting_slots.acquire_owned().await.unwrap();
            waiting_complete.load(Ordering::Acquire)
        });
        tokio::task::yield_now().await;
        setup_complete.store(true, Ordering::Release);
        drop(first);

        assert!(loser.await.unwrap());
    }

    #[tokio::test]
    async fn config_session_exposes_csrf_only_for_a_valid_admin_session() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let service = AdminService(Arc::new(AdminServiceInner {
            pool: pool.clone(),
            public_origin: "https://example.test".to_owned(),
            secure_cookies: true,
            master_key: Some(Arc::new(Zeroizing::new([7_u8; 32]))),
            setup_token_hash: None,
            dummy_password_hash: "unused".to_owned(),
            trust_forwarded_for: false,
            rate_limits: FailureRateLimiter::default(),
            argon2_slots: Arc::new(Semaphore::new(ARGON2_CONCURRENCY)),
            setup_slots: Arc::new(Semaphore::new(SETUP_CONCURRENCY)),
        }));
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO users \
             (id, username, username_norm, password_hash, role, created_at, updated_at) \
             VALUES ('admin', 'admin', 'admin', 'unused', 'admin', ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let session_token = "valid-session-token";
        let csrf_token = "valid-csrf-token";
        sqlx::query(
            "INSERT INTO admin_sessions \
             (id, token_hash, csrf_hash, user_id, session_version, created_at, last_seen_at, \
              idle_expires_at, absolute_expires_at) \
             VALUES ('session', ?, ?, 'admin', 1, ?, ?, ?, ?)",
        )
        .bind(hash_bytes(session_token.as_bytes()).to_vec())
        .bind(hash_bytes(csrf_token.as_bytes()).to_vec())
        .bind(now)
        .bind(now)
        .bind(now + 3_600)
        .bind(now + 3_600)
        .execute(&pool)
        .await
        .unwrap();

        assert!(service.config_session(&HeaderMap::new()).await.is_none());
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE_SECURE}={session_token}; {CSRF_COOKIE_SECURE}={csrf_token}"
            ))
            .unwrap(),
        );
        let session = service.config_session(&headers).await.unwrap();
        assert_eq!(session.csrf_token.as_deref(), Some(csrf_token));

        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE_SECURE}={session_token}; {CSRF_COOKIE_SECURE}=wrong"
            ))
            .unwrap(),
        );
        assert!(
            service
                .config_session(&headers)
                .await
                .unwrap()
                .csrf_token
                .is_none()
        );

        assert!(matches!(
            service.authorize_paid_job(&HeaderMap::new()).await,
            Err(PaidJobAuthorizationError::Unauthorized)
        ));
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE_SECURE}={session_token}; {CSRF_COOKIE_SECURE}={csrf_token}"
            ))
            .unwrap(),
        );
        assert!(matches!(
            service.authorize_paid_job(&headers).await,
            Err(PaidJobAuthorizationError::Forbidden)
        ));
        headers.insert(ORIGIN, HeaderValue::from_static("https://example.test"));
        headers.insert("x-csrf-token", HeaderValue::from_static(csrf_token));
        assert!(service.authorize_paid_job(&headers).await.is_ok());
    }

    #[tokio::test]
    async fn queued_provider_secret_is_memory_sealed_and_history_remains_secret_free() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let service = AdminService(Arc::new(AdminServiceInner {
            pool: pool.clone(),
            public_origin: "https://example.test".to_owned(),
            secure_cookies: true,
            master_key: Some(Arc::new(Zeroizing::new([7_u8; 32]))),
            setup_token_hash: None,
            dummy_password_hash: "unused".to_owned(),
            trust_forwarded_for: false,
            rate_limits: FailureRateLimiter::default(),
            argon2_slots: Arc::new(Semaphore::new(ARGON2_CONCURRENCY)),
            setup_slots: Arc::new(Semaphore::new(SETUP_CONCURRENCY)),
        }));
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO users \
             (id, username, username_norm, password_hash, role, created_at, updated_at) \
             VALUES ('admin', 'admin', 'admin', 'unused', 'admin', ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let old_secret = service
            .encrypt_secret(
                "provider-a",
                "openai_compatible",
                1,
                "old-secret".to_owned(),
            )
            .unwrap();
        let models = vec![AdminProviderModel {
            id: "model-a".to_owned(),
            name: "Model A".to_owned(),
            is_default: true,
        }];
        sqlx::query(
            "INSERT INTO provider_profiles \
             (id, slug, kind, display_name, base_url, enabled, public_enabled, revision, \
              secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id, \
              created_by, updated_by, created_at, updated_at) \
             VALUES ('provider-a', 'provider-a', 'openai_compatible', 'Provider A', \
              'https://old.example/v1', 1, 1, 1, 1, ?, ?, ?, 'admin', 'admin', ?, ?)",
        )
        .bind(old_secret.ciphertext.clone())
        .bind(old_secret.nonce.clone())
        .bind(old_secret.key_id)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        replace_models(&mut tx, "provider-a", &models)
            .await
            .unwrap();
        insert_provider_version(
            &mut tx,
            "provider-a",
            1,
            "openai_compatible",
            "Provider A",
            "https://old.example/v1",
            &models,
            now,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let job_id = Uuid::new_v4();
        let accepted = service
            .resolve_runtime(job_id, "provider-a", "model-a")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE provider_profiles SET revision = 2, base_url = 'https://new.example/v1', \
             enabled = 0, public_enabled = 0, secret_version = 0, \
             api_key_ciphertext = NULL, api_key_nonce = NULL, encryption_key_id = NULL, \
             deleted_at = ? WHERE id = 'provider-a'",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(accepted.revision, 1);
        assert_eq!(accepted.base_url, "https://old.example/v1");
        assert_eq!(
            service
                .open_runtime_secret(job_id, &accepted)
                .unwrap()
                .as_str(),
            "old-secret"
        );
        let historical_secrets: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM provider_versions \
             WHERE api_key_ciphertext IS NOT NULL OR api_key_nonce IS NOT NULL \
                OR encryption_key_id IS NOT NULL OR secret_version != 0",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(historical_secrets, 0);
        assert!(
            sqlx::query(
                "INSERT INTO provider_versions \
                 (profile_id, revision, kind, display_name, base_url, secret_version, \
                  api_key_ciphertext, api_key_nonce, encryption_key_id, models_json, created_at) \
                 VALUES ('provider-a', 99, 'openai_compatible', 'Provider A', \
                  'https://old.example/v1', 1, X'01', zeroblob(24), 'primary-v1', '[]', ?)",
            )
            .bind(now)
            .execute(&pool)
            .await
            .is_err()
        );
        assert!(
            service
                .resolve_runtime(Uuid::new_v4(), "provider-a", "model-a")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn persisted_verifier_accepts_the_same_key_and_rejects_a_replacement() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let original_key = [11_u8; 32];
        let replacement_key = [12_u8; 32];

        verify_or_initialize_master_key(&pool, Some(&original_key))
            .await
            .unwrap();
        verify_or_initialize_master_key(&pool, Some(&original_key))
            .await
            .unwrap();
        assert!(
            verify_or_initialize_master_key(&pool, Some(&replacement_key))
                .await
                .is_err()
        );
    }
}
