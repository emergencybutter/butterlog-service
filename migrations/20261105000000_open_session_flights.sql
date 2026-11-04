-- The service makes a flight once it is over (landed, its engine shut down), before the
-- session it is in ends (rollout phase 5): how far an open session was analysed, by its
-- chunks, and how many of its flights are made.
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS open_analysed_chunks INTEGER;
ALTER TABLE telemetry_sessions ADD COLUMN IF NOT EXISTS open_flights_made INTEGER NOT NULL DEFAULT 0;
