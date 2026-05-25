-- Migration to add allowlisted channels for Discord servers
CREATE TABLE IF NOT EXISTS allowlisted_channels (
    channel_id VARCHAR(255) PRIMARY KEY,
    channel_name VARCHAR(255) NOT NULL,
    guild_id VARCHAR(255) NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_allowlisted_channels_guild_id ON allowlisted_channels(guild_id);
