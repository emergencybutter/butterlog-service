-- The pilot's FlySto API token (www.flysto.net/account/api), with which the service
-- uploads their flights to FlySto when they ask (`flysto.rs`). NULL: not connected.
-- Never sent back to the browser.
ALTER TABLE users ADD COLUMN IF NOT EXISTS flysto_token TEXT;
