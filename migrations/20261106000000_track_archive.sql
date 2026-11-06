-- A flight's track, saved to R2 before its points are pruned, so every flight keeps its
-- page: there are no shares any more, each flight's page shows all of it.
ALTER TABLE flights ADD COLUMN IF NOT EXISTS track_key TEXT;
