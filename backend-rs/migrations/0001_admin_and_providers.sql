CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL,
    username_norm TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    session_version INTEGER NOT NULL DEFAULT 1,
    disabled_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS bootstrap_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    admin_user_id TEXT NOT NULL UNIQUE REFERENCES users(id),
    completed_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS admin_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    csrf_hash BLOB NOT NULL CHECK (length(csrf_hash) = 32),
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_version INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    idle_expires_at INTEGER NOT NULL,
    absolute_expires_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS admin_sessions_user_expiry
    ON admin_sessions(user_id, absolute_expires_at);

CREATE TABLE IF NOT EXISTS provider_profiles (
    id TEXT PRIMARY KEY NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('deepseek', 'openai_compatible')),
    display_name TEXT NOT NULL,
    base_url TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    public_enabled INTEGER NOT NULL DEFAULT 0 CHECK (public_enabled IN (0, 1)),
    is_builtin INTEGER NOT NULL DEFAULT 0 CHECK (is_builtin IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1,
    secret_version INTEGER NOT NULL DEFAULT 0,
    api_key_ciphertext BLOB,
    api_key_nonce BLOB,
    encryption_key_id TEXT,
    created_by TEXT NOT NULL REFERENCES users(id),
    updated_by TEXT NOT NULL REFERENCES users(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    CHECK (
        (api_key_ciphertext IS NULL AND api_key_nonce IS NULL AND encryption_key_id IS NULL)
        OR
        (api_key_ciphertext IS NOT NULL AND api_key_nonce IS NOT NULL AND encryption_key_id IS NOT NULL)
    )
) STRICT;

CREATE TABLE IF NOT EXISTS provider_models (
    id TEXT PRIMARY KEY NOT NULL,
    profile_id TEXT NOT NULL REFERENCES provider_profiles(id) ON DELETE CASCADE,
    upstream_model TEXT NOT NULL,
    display_name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    sort_order INTEGER NOT NULL DEFAULT 0,
    options_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (profile_id, upstream_model)
) STRICT;

CREATE INDEX IF NOT EXISTS provider_models_profile_order
    ON provider_models(profile_id, sort_order, upstream_model);

CREATE TABLE IF NOT EXISTS admin_audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at INTEGER NOT NULL,
    actor_user_id TEXT REFERENCES users(id),
    action TEXT NOT NULL,
    target_type TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}'
) STRICT;
