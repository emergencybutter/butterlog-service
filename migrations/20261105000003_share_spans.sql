-- The time span of a share's track, so a share the app uploads without a flight, for one
-- the service made, can be matched to it (rollout phase 5, `shares.rs`).
ALTER TABLE flight_shares ADD COLUMN IF NOT EXISTS starts_at TIMESTAMPTZ;
ALTER TABLE flight_shares ADD COLUMN IF NOT EXISTS ends_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS idx_flight_shares_unlinked ON flight_shares (created_at) WHERE remote_flight_id IS NULL;
