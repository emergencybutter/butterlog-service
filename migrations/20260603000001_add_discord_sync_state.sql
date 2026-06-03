-- Per-flight state for throttling Discord notification updates and detecting when the
-- screenshot set changed (so we can skip re-downloading unchanged screenshots from R2).
ALTER TABLE flights ADD COLUMN IF NOT EXISTS discord_last_synced_at TIMESTAMPTZ;
ALTER TABLE flights ADD COLUMN IF NOT EXISTS discord_screenshot_sig TEXT;
