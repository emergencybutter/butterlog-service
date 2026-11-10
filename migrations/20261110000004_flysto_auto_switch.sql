-- Whether ended flights go to FlySto by themselves (`flysto.rs`). Off, the pilot still
-- sends the flights they choose with the flight page's button.
ALTER TABLE users ADD COLUMN IF NOT EXISTS flysto_auto BOOLEAN NOT NULL DEFAULT TRUE;
