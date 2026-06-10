-- Store API tokens hashed (SHA-256) instead of plaintext, and allow multiple
-- active tokens per user so a web login no longer has to share the exact same
-- credential as the desktop app. Existing tokens are migrated by hashing them,
-- so clients keep working without re-authenticating.
CREATE TABLE IF NOT EXISTS api_tokens (
    token_hash CHAR(64) PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    last_used_at TIMESTAMP WITH TIME ZONE
);

CREATE INDEX IF NOT EXISTS idx_api_tokens_user_id ON api_tokens (user_id);

-- Backfill: hash every existing plaintext token (sha256() is built-in since
-- PostgreSQL 11).
INSERT INTO api_tokens (token_hash, user_id)
SELECT encode(sha256(convert_to(api_token, 'UTF8')), 'hex'), id
FROM users
WHERE api_token IS NOT NULL AND api_token <> ''
ON CONFLICT (token_hash) DO NOTHING;

-- The plaintext column must not survive the migration.
ALTER TABLE users DROP COLUMN IF EXISTS api_token;
