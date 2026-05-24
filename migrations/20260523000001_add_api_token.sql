-- Migration to add api_token to users table
ALTER TABLE users ADD COLUMN IF NOT EXISTS api_token VARCHAR(255) UNIQUE;
UPDATE users SET api_token = md5(random()::text || clock_timestamp()::text) WHERE api_token IS NULL;
ALTER TABLE users ALTER COLUMN api_token SET NOT NULL;

CREATE INDEX IF NOT EXISTS idx_users_api_token ON users (api_token);
