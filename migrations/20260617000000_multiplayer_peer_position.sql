-- Store each peer's position so the ping endpoint can return only nearby peers
-- instead of every online user (cuts the all-to-all fan-out). Nullable: a peer
-- without a position fix simply isn't matched by the proximity filter.
ALTER TABLE multiplayer_peers ADD COLUMN IF NOT EXISTS latitude  DOUBLE PRECISION;
ALTER TABLE multiplayer_peers ADD COLUMN IF NOT EXISTS longitude DOUBLE PRECISION;
