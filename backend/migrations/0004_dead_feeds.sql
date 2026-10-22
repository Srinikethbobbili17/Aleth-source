-- Set when a feed reaches 10 consecutive failures; cleared by a successful refresh.
-- Automatic polling must skip feeds where this is not null.
ALTER TABLE feeds ADD COLUMN dead_at TIMESTAMPTZ;
