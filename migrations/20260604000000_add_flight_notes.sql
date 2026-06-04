-- Add an optional, user-provided notes field to flights (max 500 chars).
ALTER TABLE flights ADD COLUMN IF NOT EXISTS notes VARCHAR(500);
