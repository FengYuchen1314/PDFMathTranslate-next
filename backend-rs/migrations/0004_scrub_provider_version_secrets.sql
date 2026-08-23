UPDATE provider_versions
SET secret_version = 0,
    api_key_ciphertext = NULL,
    api_key_nonce = NULL,
    encryption_key_id = NULL;

CREATE TRIGGER IF NOT EXISTS provider_versions_reject_secret_insert
BEFORE INSERT ON provider_versions
WHEN NEW.secret_version != 0
  OR NEW.api_key_ciphertext IS NOT NULL
  OR NEW.api_key_nonce IS NOT NULL
  OR NEW.encryption_key_id IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'provider version history must not contain credentials');
END;

CREATE TRIGGER IF NOT EXISTS provider_versions_reject_secret_update
BEFORE UPDATE OF secret_version, api_key_ciphertext, api_key_nonce, encryption_key_id
ON provider_versions
WHEN NEW.secret_version != 0
  OR NEW.api_key_ciphertext IS NOT NULL
  OR NEW.api_key_nonce IS NOT NULL
  OR NEW.encryption_key_id IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'provider version history must not contain credentials');
END;
