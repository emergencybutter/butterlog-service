-- Channels a pilot asked not to have their flights posted in, though a server they are in
-- chose them (`discord::set_channel_posting`). Kept apart from discord_notification_channels,
-- which is rebuilt from the pilot's server memberships every hour: an opt-out outlives
-- leaving the server and joining again, and an admin unticking the channel and ticking it
-- again.
CREATE TABLE IF NOT EXISTS discord_channel_optouts (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id VARCHAR(255) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, channel_id)
);
