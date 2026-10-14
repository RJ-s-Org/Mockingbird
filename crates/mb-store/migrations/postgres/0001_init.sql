-- Control-plane schema (Postgres). JSON columns use `json` (not `jsonb`) so
-- object key order round-trips exactly.

CREATE TABLE mocks (
    id               UUID PRIMARY KEY,
    workspace        TEXT NOT NULL,
    name             TEXT NOT NULL,
    route_prefix     TEXT NOT NULL,
    persona          TEXT,
    llm_profile      TEXT,
    header_overrides BOOLEAN NOT NULL DEFAULT FALSE,
    source           TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL,
    UNIQUE (workspace, name)
);

CREATE TABLE contract_versions (
    id            UUID PRIMARY KEY,
    mock_id       UUID NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    version       BIGINT NOT NULL,
    source_digest TEXT NOT NULL,
    source        BYTEA NOT NULL,
    bundle        JSON NOT NULL,
    status        TEXT NOT NULL,
    created_by    TEXT,
    created_at    TIMESTAMPTZ NOT NULL,
    UNIQUE (mock_id, version)
);

-- At most one active version per mock.
CREATE UNIQUE INDEX contract_versions_one_active
    ON contract_versions (mock_id) WHERE status = 'active';

CREATE TABLE scenarios (
    mock_id      UUID NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    instructions TEXT,
    rules        JSON NOT NULL,
    source       TEXT NOT NULL,
    PRIMARY KEY (mock_id, name)
);

CREATE TABLE llm_profiles (
    workspace        TEXT NOT NULL,
    name             TEXT NOT NULL,
    base_url         TEXT NOT NULL,
    model            TEXT NOT NULL,
    secret_ref       TEXT,
    json_schema      BOOLEAN NOT NULL,
    temperature      DOUBLE PRECISION NOT NULL,
    timeout_ms       BIGINT NOT NULL,
    max_tokens       BIGINT NOT NULL,
    fallback_profile TEXT,
    source           TEXT NOT NULL,
    PRIMARY KEY (workspace, name)
);

CREATE TABLE request_log (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    mock_id           UUID NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    operation_id      TEXT,
    session           TEXT NOT NULL,
    method            TEXT NOT NULL,
    path              TEXT NOT NULL,
    status            INTEGER NOT NULL,
    source            TEXT,
    latency_ms        BIGINT NOT NULL,
    prompt_tokens     BIGINT NOT NULL,
    completion_tokens BIGINT NOT NULL,
    request           JSON NOT NULL,
    response          JSON,
    at                TIMESTAMPTZ NOT NULL
);

CREATE INDEX request_log_mock_id ON request_log (mock_id, id);
