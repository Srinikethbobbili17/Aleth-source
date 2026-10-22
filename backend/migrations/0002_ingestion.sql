ALTER TABLE feeds
    ADD COLUMN validator_url TEXT,
    ADD COLUMN last_success_at TIMESTAMPTZ,
    ADD COLUMN last_error TEXT,
    ADD COLUMN refresh_token UUID,
    ADD COLUMN refresh_until TIMESTAMPTZ;

CREATE TABLE items (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    feed_id BIGINT NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    identity TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    current_version_id BIGINT,
    UNIQUE (feed_id, identity)
);

CREATE TABLE item_versions (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    item_id BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL,
    external_id TEXT,
    title TEXT,
    url TEXT,
    authors TEXT[] NOT NULL,
    summary TEXT,
    content TEXT,
    content_type TEXT,
    published_at TIMESTAMPTZ,
    source_updated_at TIMESTAMPTZ,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (item_id, id)
);

-- The composite reference prevents one item's current version pointing at another item.
ALTER TABLE items ADD CONSTRAINT items_current_version_fk
    FOREIGN KEY (id, current_version_id) REFERENCES item_versions(item_id, id)
    DEFERRABLE INITIALLY DEFERRED;

CREATE INDEX items_feed_id_id_idx ON items(feed_id, id DESC);
CREATE INDEX item_versions_item_id_id_idx ON item_versions(item_id, id DESC);
