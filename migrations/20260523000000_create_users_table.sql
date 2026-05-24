-- Migration to create the users table for ButterLog service
CREATE TABLE IF NOT EXISTS users (
    id BIGSERIAL PRIMARY KEY,
    discord_id VARCHAR(255) NOT NULL UNIQUE,
    username VARCHAR(255) NOT NULL,
    global_name VARCHAR(255),
    avatar VARCHAR(255),
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    last_login TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_users_discord_id ON users (discord_id);
