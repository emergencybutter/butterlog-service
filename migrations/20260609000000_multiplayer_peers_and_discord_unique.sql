-- Multiplayer peer presence moved from process memory to the database so it
-- works across multiple Cloud Run instances and survives restarts.
CREATE TABLE IF NOT EXISTS multiplayer_peers (
    user_id BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    udp_address TEXT NOT NULL,
    last_seen TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_multiplayer_peers_last_seen ON multiplayer_peers (last_seen);

-- Enforce one Discord message per flight per channel so concurrent syncs
-- cannot post duplicates. Deduplicate existing rows first (keep the oldest).
DELETE FROM flight_discord_messages a
USING flight_discord_messages b
WHERE a.id > b.id
  AND a.flight_id = b.flight_id
  AND a.discord_channel_id = b.discord_channel_id;

CREATE UNIQUE INDEX IF NOT EXISTS uq_flight_discord_messages_flight_channel
    ON flight_discord_messages (flight_id, discord_channel_id);

-- Used to expire 'pending' placeholder rows left behind if a process dies
-- between claiming a channel slot and recording the sent message id.
ALTER TABLE flight_discord_messages
    ADD COLUMN IF NOT EXISTS created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL;
