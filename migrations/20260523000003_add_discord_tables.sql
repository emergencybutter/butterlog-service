-- Migration to add discord messages and notification channels tables for ButterLog service
CREATE TABLE IF NOT EXISTS flight_discord_messages (
    id BIGSERIAL PRIMARY KEY,
    flight_id BIGINT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
    discord_message_id VARCHAR(255) NOT NULL,
    discord_channel_id VARCHAR(255) NOT NULL
);

CREATE TABLE IF NOT EXISTS discord_notification_channels (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id VARCHAR(255) NOT NULL,
    UNIQUE(user_id, channel_id)
);

CREATE INDEX IF NOT EXISTS idx_flight_discord_messages_flight_id ON flight_discord_messages(flight_id);
CREATE INDEX IF NOT EXISTS idx_discord_notification_channels_user_id ON discord_notification_channels(user_id);
