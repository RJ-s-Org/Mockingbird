-- Event channels (P5): request kinds, per-channel settings, per-mock event
-- settings and the scheduler lease.

ALTER TABLE request_log ADD COLUMN kind TEXT NOT NULL DEFAULT 'http';

CREATE TABLE channel_settings (
    mock_id    UUID NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    channel_id TEXT NOT NULL,
    enabled    BOOLEAN NOT NULL,
    triggers   JSON,
    source     TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (mock_id, channel_id)
);

CREATE TABLE mock_event_settings (
    mock_id    UUID PRIMARY KEY REFERENCES mocks (id) ON DELETE CASCADE,
    broker     TEXT,
    source     TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE scheduler_leases (
    mock_id    UUID PRIMARY KEY REFERENCES mocks (id) ON DELETE CASCADE,
    holder     TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
