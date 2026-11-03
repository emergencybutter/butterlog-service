-- The session whose flight this one picks up after a client restart
-- (analysis/resume.rs). The flights of such a chain are stored under its first session.
ALTER TABLE telemetry_sessions
    ADD COLUMN IF NOT EXISTS continues UUID REFERENCES telemetry_sessions(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS telemetry_sessions_continues ON telemetry_sessions (continues);
