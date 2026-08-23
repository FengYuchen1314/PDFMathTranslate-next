CREATE TABLE IF NOT EXISTS credential_key_verifier (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    encryption_key_id TEXT NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 24),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
