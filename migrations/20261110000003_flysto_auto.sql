-- Ended flights go to FlySto by themselves for pilots who connected it (`flysto.rs`):
-- those ended since they connected (`flysto_since`), not their whole history.
ALTER TABLE users ADD COLUMN IF NOT EXISTS flysto_since TIMESTAMPTZ;
UPDATE users SET flysto_since = NOW() WHERE flysto_token IS NOT NULL AND flysto_since IS NULL;

-- Automatic uploads that failed, retried later, at most 5 times.
CREATE TABLE IF NOT EXISTS flysto_auto_attempts (
    flight_id  BIGINT PRIMARY KEY REFERENCES flights(id) ON DELETE CASCADE,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    tried_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
