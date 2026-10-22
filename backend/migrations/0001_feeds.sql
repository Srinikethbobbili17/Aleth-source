CREATE TABLE feeds (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    url TEXT NOT NULL UNIQUE CHECK (char_length(url) BETWEEN 1 AND 4096),
    title TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    etag TEXT,
    last_modified TEXT,
    last_fetched_at TIMESTAMPTZ,
    next_fetch_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    failure_count INTEGER NOT NULL DEFAULT 0 CHECK (failure_count >= 0)
);

CREATE INDEX feeds_next_fetch_at_idx ON feeds (next_fetch_at, id);

