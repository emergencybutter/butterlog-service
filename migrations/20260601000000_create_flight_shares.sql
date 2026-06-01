CREATE TABLE IF NOT EXISTS flight_shares (
    id TEXT PRIMARY KEY,
    user_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    remote_flight_id BIGINT,
    r2_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_flight_shares_user_id ON flight_shares (user_id);
