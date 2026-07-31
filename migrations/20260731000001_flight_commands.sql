-- Commands sent from the web to a running simulator.
--
-- This is the only owner-only part of the live-flight feature: reads are public,
-- but nobody gets to touch somebody else's simulator. Both the issue and the
-- long-poll check the caller against flights.user_id.
CREATE TABLE IF NOT EXISTS flight_commands (
    id           UUID PRIMARY KEY,
    flight_id    BIGINT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
    issued_by    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    type         TEXT NOT NULL,
    params       JSONB NOT NULL DEFAULT '{}'::jsonb,
    stability    TEXT NOT NULL,          -- stable | beta
    issued_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- A command issued while the app was offline must not fire when it
    -- reconnects ten minutes later.
    expires_at   TIMESTAMPTZ NOT NULL,
    -- Stamped when the long-poll hands the command over. At-most-once: a
    -- delivered command is never redelivered, even if the ack never arrives.
    delivered_at TIMESTAMPTZ,
    acked_at     TIMESTAMPTZ,
    result       TEXT,                   -- applied | unsupported | rejected | expired
    detail       TEXT
);

-- The long-poll's hot query: undelivered, unexpired commands for one flight.
CREATE INDEX IF NOT EXISTS idx_flight_commands_pending
    ON flight_commands (flight_id, issued_at)
    WHERE delivered_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_flight_commands_issued_at
    ON flight_commands (flight_id, issued_at DESC);
