-- Events the sim reported (crash, flight loaded, pause...), copied out of telemetry chunks
-- as they decode, so analysis can find them without reading segments. The chunks keep
-- them too.
CREATE TABLE IF NOT EXISTS telemetry_events (
    session_id UUID NOT NULL REFERENCES telemetry_sessions(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    -- Position among the chunk's events.
    idx        INTEGER NOT NULL,
    sim_time   DOUBLE PRECISION NOT NULL,
    wall_time  TIMESTAMPTZ NOT NULL,
    -- butterlog_wire::EventKind code.
    kind       SMALLINT NOT NULL,
    detail     TEXT NOT NULL,
    PRIMARY KEY (session_id, seq, idx)
);
