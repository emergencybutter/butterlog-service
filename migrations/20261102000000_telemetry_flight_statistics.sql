-- Each telemetry flight as the app's flights.statistics document, built by the service
-- (docs/sim-native-telemetry-rollout.md phase 5): rendered beside the app's to compare,
-- before the service writes it in place of the app.
ALTER TABLE telemetry_flights ADD COLUMN IF NOT EXISTS statistics JSONB;
