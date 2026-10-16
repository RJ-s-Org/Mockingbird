-- Smart import (SQLite): draft metadata on contract versions and the import
-- job queue. Timestamps are sqlx's RFC 3339 text, which orders as text.

ALTER TABLE contract_versions ADD COLUMN draft_meta TEXT;

CREATE TABLE import_jobs (
    id             TEXT PRIMARY KEY NOT NULL,
    workspace      TEXT NOT NULL,
    mock_id        TEXT NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    mock           TEXT NOT NULL,
    kind           TEXT NOT NULL,
    source_name    TEXT,
    -- Emptied once the job finishes.
    input          BLOB NOT NULL,
    status         TEXT NOT NULL,
    progress       TEXT NOT NULL,
    result_version INTEGER,
    error          TEXT,
    created_by     TEXT,
    created_at     TEXT NOT NULL,
    started_at     TEXT,
    finished_at    TEXT,
    updated_at     TEXT NOT NULL
);

CREATE INDEX import_jobs_status ON import_jobs (status, created_at);
CREATE INDEX import_jobs_mock_id ON import_jobs (mock_id, created_at);
