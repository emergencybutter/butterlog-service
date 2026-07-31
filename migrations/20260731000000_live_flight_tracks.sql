-- Live flight tracks: one row per telemetry sample.
--
-- The sample timestamp is the natural key, so uploads are idempotent by
-- construction: a client replaying a batch after a timeout, resuming from a
-- stale cursor, or briefly racing another client is a no-op rather than an
-- error. That removes any need for a sequence number or resync handshake.
CREATE TABLE IF NOT EXISTS flight_track_points (
    flight_id    BIGINT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
    sample_epoch BIGINT NOT NULL,   -- absolute unix seconds
    latitude     REAL   NOT NULL,
    longitude    REAL   NOT NULL,
    altitude     REAL,
    ias          REAL,
    vspeed       REAL,
    pitch        REAL,
    roll         REAL,
    PRIMARY KEY (flight_id, sample_epoch)
);

-- Explicit flight lifecycle, so "landed" is distinguishable from "the app died".
ALTER TABLE flights ADD COLUMN IF NOT EXISTS status       TEXT NOT NULL DEFAULT 'active';
ALTER TABLE flights ADD COLUMN IF NOT EXISTS ended_at     TIMESTAMPTZ;
ALTER TABLE flights ADD COLUMN IF NOT EXISTS end_reason   TEXT;
ALTER TABLE flights ADD COLUMN IF NOT EXISTS share_id     TEXT REFERENCES flight_shares(id) ON DELETE SET NULL;
ALTER TABLE flights ADD COLUMN IF NOT EXISTS track_points INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_flights_status_updated ON flights (status, updated_at DESC);

-- Every pre-existing flight is history, not a flight in progress. Without this
-- the new DEFAULT would present the entire archive as currently active until
-- the reaper caught up with it.
UPDATE flights
   SET status = 'ended', ended_at = updated_at, end_reason = 'abandoned'
 WHERE status = 'active'
   AND updated_at < NOW() - INTERVAL '30 minutes';

-- Backfill the share link for flights that were already shared.
UPDATE flights f
   SET share_id = s.id
  FROM flight_shares s
 WHERE s.remote_flight_id = f.id
   AND f.share_id IS NULL;
