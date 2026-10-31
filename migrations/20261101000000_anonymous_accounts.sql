-- Anonymous accounts (docs/sim-native-telemetry.md §6.2): a user who hasn't signed in with
-- Discord still streams. Their flights are reachable through a secret link and kept 14 days.

-- An anonymous user is an ordinary row with no Discord identity. UNIQUE ignores NULLs.
ALTER TABLE users ALTER COLUMN discord_id DROP NOT NULL;

-- Set while the user is anonymous. Never cleared: a claim moves everything to the Discord
-- account and deletes the row.
ALTER TABLE users ADD COLUMN IF NOT EXISTS anonymous_since TIMESTAMPTZ;

-- SHA-256 of the link secret, hex, like api_tokens. The token writes, the link only reads.
ALTER TABLE users ADD COLUMN IF NOT EXISTS link_secret_hash CHAR(64) UNIQUE;

CREATE INDEX IF NOT EXISTS idx_users_anonymous ON users (anonymous_since)
    WHERE anonymous_since IS NOT NULL;

-- Rate limit for creating anonymous accounts, per client address.
CREATE TABLE IF NOT EXISTS anonymous_signups (
    address    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_anonymous_signups ON anonymous_signups (address, created_at);
