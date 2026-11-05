-- The flight an open session is flying, as its last analysis made it, and how many of
-- its chunks that flight's track has: between analyses, each chunk since is added to its
-- track and position, so the live page moves with the chunks (rollout phase 5).
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS open_flight_id BIGINT;
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS open_tailed_chunks INTEGER;
