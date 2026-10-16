-- API keys, admin sessions and the audit log (Postgres).

CREATE TABLE api_keys (
    key_id       TEXT PRIMARY KEY,
    workspace    TEXT NOT NULL REFERENCES workspaces (name) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    role         TEXT NOT NULL,
    scopes       JSON NOT NULL,
    sysadmin     BOOLEAN NOT NULL DEFAULT FALSE,
    secret_hash  TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ
);

CREATE INDEX api_keys_workspace ON api_keys (workspace, created_at);

CREATE TABLE admin_sessions (
    id_hash    TEXT PRIMARY KEY,
    principal  JSON NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    last_seen  TIMESTAMPTZ NOT NULL
);

CREATE INDEX admin_sessions_expires_at ON admin_sessions (expires_at);

-- No foreign key to `workspaces`: events outlive the workspace they describe.
CREATE TABLE audit_events (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace     TEXT,
    actor_subject TEXT NOT NULL,
    actor_kind    TEXT NOT NULL,
    action        TEXT NOT NULL,
    target        TEXT NOT NULL,
    before_value  JSON,
    after_value   JSON,
    request_id    TEXT,
    at            TIMESTAMPTZ NOT NULL
);

CREATE INDEX audit_events_workspace ON audit_events (workspace, id);
CREATE INDEX audit_events_at ON audit_events (at);
