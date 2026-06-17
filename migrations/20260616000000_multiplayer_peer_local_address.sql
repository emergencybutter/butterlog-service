-- Add a LAN candidate address for multiplayer peers. Two players behind the
-- same NAT share a public IP and many routers don't hairpin, so peers also
-- publish their local (LAN) address; clients on the same NAT use it to connect
-- directly. Nullable: older clients that don't publish a local address are fine.
ALTER TABLE multiplayer_peers ADD COLUMN IF NOT EXISTS local_udp_address TEXT;
