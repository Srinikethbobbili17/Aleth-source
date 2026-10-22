-- Stable failure category for the latest error, e.g. 'connect_timeout' or 'http_status'.
ALTER TABLE feeds ADD COLUMN last_error_code TEXT;
