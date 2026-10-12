-- The ended-flights feed pages through finished flights in (ended_at, id)
-- order. Without this it sorts every ended flight on each poll.
CREATE INDEX IF NOT EXISTS idx_flights_ended_feed ON flights (ended_at, id) WHERE status = 'ended';
