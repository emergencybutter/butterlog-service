-- Rollout phase 5: the service makes a user's flights from their telemetry.
--
-- From `service_flights_since` (NULL: never), each flight the analysis of a user's
-- telemetry finds is theirs on the service: the app's flight it matches takes the
-- service's statistics document, its own kept in `app_statistics`; one the app didn't
-- make is created (`service_made`), with its track. Either is `service_owned`: the app's
-- later updates go to `app_statistics`, and analysing the telemetry again rewrites it.
ALTER TABLE users ADD COLUMN IF NOT EXISTS service_flights_since TIMESTAMPTZ;

ALTER TABLE flights ADD COLUMN IF NOT EXISTS service_owned BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE flights ADD COLUMN IF NOT EXISTS service_made BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE flights ADD COLUMN IF NOT EXISTS app_statistics JSONB;
-- Written by an analysis, so its Discord post is synced once the analysis commits.
ALTER TABLE flights ADD COLUMN IF NOT EXISTS discord_sync_due BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_flights_discord_sync_due ON flights (id) WHERE discord_sync_due;
