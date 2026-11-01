-- Server-side analysis of telemetry sessions (docs/sim-native-telemetry.md §5.2, rollout
-- phase 3), run as each session ends, before it is compacted.

-- The analyser version a session was last analysed with, and why it failed, if it did.
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS analyser_version INTEGER;
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS analysis_error TEXT;
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS analysed_at TIMESTAMPTZ;

-- The flights found in a session, as the analyser version that found them saw them.
CREATE TABLE IF NOT EXISTS telemetry_flights (
    session_id       UUID NOT NULL REFERENCES telemetry_sessions(id) ON DELETE CASCADE,
    idx              INTEGER NOT NULL,
    analyser_version INTEGER NOT NULL,
    first_at         TIMESTAMPTZ NOT NULL,
    last_at          TIMESTAMPTZ NOT NULL,
    takeoff_at       TIMESTAMPTZ,
    departure        TEXT NOT NULL,
    arrival          TEXT NOT NULL,
    summary          JSONB NOT NULL,
    -- The same flight as the app uploaded it the old way, and how the two compare:
    -- the phase 3 parity check.
    flight_id        BIGINT REFERENCES flights(id) ON DELETE SET NULL,
    parity           JSONB,
    PRIMARY KEY (session_id, idx)
);

CREATE INDEX IF NOT EXISTS idx_telemetry_flights_flight ON telemetry_flights (flight_id);
