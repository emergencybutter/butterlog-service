-- Screenshots uploaded against a telemetry session (design §5.5): the service places
-- each on the flight whose time contains it, so its flight can be none yet. One upload per
-- user and image, whichever session it came with.
ALTER TABLE screenshots ALTER COLUMN flight_id DROP NOT NULL;
ALTER TABLE screenshots ADD COLUMN IF NOT EXISTS session_id UUID REFERENCES telemetry_sessions(id) ON DELETE CASCADE;
ALTER TABLE screenshots ADD COLUMN IF NOT EXISTS user_id BIGINT REFERENCES users(id) ON DELETE CASCADE;
ALTER TABLE screenshots ADD COLUMN IF NOT EXISTS taken_at TIMESTAMPTZ;
CREATE UNIQUE INDEX IF NOT EXISTS idx_screenshots_user_hash ON screenshots (user_id, hash) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_screenshots_user_taken ON screenshots (user_id, taken_at) WHERE session_id IS NOT NULL;
