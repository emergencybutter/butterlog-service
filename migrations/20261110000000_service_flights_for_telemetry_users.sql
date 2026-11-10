-- Rollout phase 5 for every user sending telemetry: the service makes their flights from
-- it, from now on (flights already flown keep the app's documents). Users whose first
-- session arrives later are switched on as it opens (`sessions.rs`).
UPDATE users SET service_flights_since = NOW()
 WHERE service_flights_since IS NULL
   AND EXISTS (SELECT 1 FROM telemetry_sessions s WHERE s.user_id = users.id);
