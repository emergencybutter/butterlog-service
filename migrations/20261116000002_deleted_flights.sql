-- Flights their pilots deleted (`erase::delete_own`), kept only so the analysis never makes
-- one again: analysing a session afresh (a new analyser version, a session that continues
-- it) matches each flight it finds against the pilot's flights, and creates those it can't
-- find. It matches against these too, by the same takeoff or start time, and leaves a match
-- deleted. `statistics` is the flight's document as it was, which that match reads.
CREATE TABLE IF NOT EXISTS deleted_flights (
    flight_id     BIGINT PRIMARY KEY,
    user_id       BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    statistics    JSONB NOT NULL,
    service_owned BOOLEAN NOT NULL,
    -- The flight's own created_at, which the analysis looks around.
    created_at    TIMESTAMPTZ NOT NULL,
    deleted_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_deleted_flights_user_created ON deleted_flights (user_id, created_at);
