-- Control-plane schema (SQLite). Uuids are TEXT, JSON columns are TEXT.

CREATE TABLE mocks (
    id               TEXT PRIMARY KEY NOT NULL,
    workspace        TEXT NOT NULL,
    name             TEXT NOT NULL,
    route_prefix     TEXT NOT NULL,
    persona          TEXT,
    llm_profile      TEXT,
    header_overrides INTEGER NOT NULL DEFAULT 0,
    source           TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    UNIQUE (workspace, name)
);

CREATE TABLE contract_versions (
    id            TEXT PRIMARY KEY NOT NULL,
    mock_id       TEXT NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    version       INTEGER NOT NULL,
    source_digest TEXT NOT NULL,
    source        BLOB NOT NULL,
    bundle        TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_by    TEXT,
    created_at    TEXT NOT NULL,
    UNIQUE (mock_id, version)
);

-- At most one active version per mock.
CREATE UNIQUE INDEX contract_versions_one_active
    ON contract_versions (mock_id) WHERE status = 'active';

CREATE TABLE scenarios (
    mock_id      TEXT NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    instructions TEXT,
    rules        TEXT NOT NULL,
    source       TEXT NOT NULL,
    PRIMARY KEY (mock_id, name)
);

CREATE TABLE llm_profiles (
    workspace        TEXT NOT NULL,
    name             TEXT NOT NULL,
    base_url         TEXT NOT NULL,
    model            TEXT NOT NULL,
    secret_ref       TEXT,
    json_schema      INTEGER NOT NULL,
    temperature      REAL NOT NULL,
    timeout_ms       INTEGER NOT NULL,
    max_tokens       INTEGER NOT NULL,
    fallback_profile TEXT,
    source           TEXT NOT NULL,
    PRIMARY KEY (workspace, name)
);

CREATE TABLE request_log (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    mock_id           TEXT NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    operation_id      TEXT,
    session           TEXT NOT NULL,
    method            TEXT NOT NULL,
    path              TEXT NOT NULL,
    status            INTEGER NOT NULL,
    source            TEXT,
    latency_ms        INTEGER NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    request           TEXT NOT NULL,
    response          TEXT,
    at                TEXT NOT NULL
);

CREATE INDEX request_log_mock_id ON request_log (mock_id, id);
