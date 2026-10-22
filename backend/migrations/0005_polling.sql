-- Split the single next_fetch_at timer into three with separate owners:
--   next_poll_at          when the scheduler should next refresh the feed
--   manual_refresh_after  cooldown after any refresh; only manual refreshes wait for it
--   retry_not_before      failure backoff (including Retry-After); everyone waits for it
ALTER TABLE feeds RENAME COLUMN next_fetch_at TO next_poll_at;
ALTER TABLE feeds
    ADD COLUMN manual_refresh_after TIMESTAMPTZ,
    ADD COLUMN retry_not_before TIMESTAMPTZ;

-- A pending failure backoff carries over; success cooldowns were at most 30 seconds.
UPDATE feeds SET retry_not_before = next_poll_at
WHERE failure_count > 0 AND next_poll_at > now();

-- The scheduler only ever looks for due feeds that are not dead.
DROP INDEX feeds_next_fetch_at_idx;
CREATE INDEX feeds_due_idx ON feeds (next_poll_at, id) WHERE dead_at IS NULL;
