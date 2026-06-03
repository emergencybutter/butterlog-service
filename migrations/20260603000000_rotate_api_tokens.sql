-- Rotate all API tokens off the old weak generator (md5(random()...)) onto
-- cryptographically strong values. gen_random_uuid() is built in on PostgreSQL 13+.
-- NOTE: this invalidates every existing client's stored token; clients must re-login.
UPDATE users
SET api_token = replace(gen_random_uuid()::text, '-', '') || replace(gen_random_uuid()::text, '-', '');
