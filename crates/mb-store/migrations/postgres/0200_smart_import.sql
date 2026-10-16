-- Smart import (Postgres): draft metadata on contract versions and the import
-- job queue.

ALTER TABLE contract_versions ADD COLUMN draft_meta JSON;

CREATE TABLE import_jobs (
    id             UUID PRIMARY KEY,
    workspace      TEXT NOT NULL,
    mock_id        UUID NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    mock           TEXT NOT NULL,
    kind           TEXT NOT NULL,
    source_name    TEXT,
    -- Emptied once the job finishes.
    input          BYTEA NOT NULL,
    status         TEXT NOT NULL,
    progress       JSON NOT NULL,
    result_version BIGINT,
    error          TEXT,
    created_by     TEXT,
    created_at     TIMESTAMPTZ NOT NULL,
    started_at     TIMESTAMPTZ,
    finished_at    TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ NOT NULL
);

CREATE INDEX import_jobs_status ON import_jobs (status, created_at);
CREATE INDEX import_jobs_mock_id ON import_jobs (mock_id, created_at);
