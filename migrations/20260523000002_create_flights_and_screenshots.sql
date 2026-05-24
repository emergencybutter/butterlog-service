-- Migration to create flights and screenshots tables for ButterLog service
CREATE TABLE IF NOT EXISTS flights (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    departure VARCHAR(10) NOT NULL,
    arrival VARCHAR(10),
    statistics JSONB NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TABLE IF NOT EXISTS screenshots (
    id BIGSERIAL PRIMARY KEY,
    flight_id BIGINT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
    hash VARCHAR(64) NOT NULL,
    url VARCHAR(512) NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    UNIQUE(flight_id, hash)
);

CREATE INDEX IF NOT EXISTS idx_flights_user_id ON flights (user_id);
CREATE INDEX IF NOT EXISTS idx_screenshots_flight_id ON screenshots (flight_id);
