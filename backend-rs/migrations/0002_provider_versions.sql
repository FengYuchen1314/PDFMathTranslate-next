CREATE TABLE IF NOT EXISTS provider_versions (
    profile_id TEXT NOT NULL REFERENCES provider_profiles(id),
    revision INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('deepseek', 'openai_compatible')),
    display_name TEXT NOT NULL,
    base_url TEXT NOT NULL,
    secret_version INTEGER NOT NULL,
    api_key_ciphertext BLOB,
    api_key_nonce BLOB,
    encryption_key_id TEXT,
    models_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (profile_id, revision),
    CHECK (
        (api_key_ciphertext IS NULL AND api_key_nonce IS NULL AND encryption_key_id IS NULL)
        OR
        (api_key_ciphertext IS NOT NULL AND api_key_nonce IS NOT NULL AND encryption_key_id IS NOT NULL)
    )
) STRICT;
