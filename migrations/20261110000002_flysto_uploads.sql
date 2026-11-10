-- Where each flight went on FlySto (`flysto.rs`): the file FlySto gave the upload, then,
-- once FlySto has processed it, the logs it made of it, each at
-- https://www.flysto.net/logs/<id>. One a flight: uploading it again finds the same
-- file (FlySto's 409).
CREATE TABLE IF NOT EXISTS flysto_uploads (
    flight_id   BIGINT PRIMARY KEY REFERENCES flights(id) ON DELETE CASCADE,
    user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    file_id     TEXT NOT NULL,
    processed   BOOLEAN NOT NULL DEFAULT FALSE,
    log_ids     TEXT[] NOT NULL DEFAULT '{}',
    uploaded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    checked_at  TIMESTAMPTZ
);
