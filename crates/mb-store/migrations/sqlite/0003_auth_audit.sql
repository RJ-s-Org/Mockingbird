-- API keys, admin sessions and the audit log (SQLite).
--
-- Timestamps are sqlx's RFC 3339 text for a `DateTime<Utc>` (fractional
-- seconds in groups of three digits, always `+00:00`), which orders
-- correctly as text, so range filters compare the columns directly.

CREATE TABLE api_keys (
    key_id       TEXT PRIMARY KEY NOT NULL,
    workspace    TEXT NOT NULL REFERENCES workspaces (name) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    role         TEXT NOT NULL,
    scopes       TEXT NOT NULL,
    sysadmin     BOOLEAN NOT NULL DEFAULT 0,
    secret_hash  TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    expires_at   TEXT,
    last_used_at TEXT,
    revoked_at   TEXT
);

CREATE INDEX api_keys_workspace ON api_keys (workspace, created_at);

CREATE TABLE admin_sessions (
    id_hash    TEXT PRIMARY KEY NOT NULL,
    principal  TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    last_seen  TEXT NOT NULL
);

CREATE INDEX admin_sessions_expires_at ON admin_sessions (expires_at);

-- No foreign key to `workspaces`: events outlive the workspace they describe.
CREATE TABLE audit_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace     TEXT,
    actor_subject TEXT NOT NULL,
    actor_kind    TEXT NOT NULL,
    action        TEXT NOT NULL,
    target        TEXT NOT NULL,
    before_value  TEXT,
    after_value   TEXT,
    request_id    TEXT,
    at            TEXT NOT NULL
);

CREATE INDEX audit_events_workspace ON audit_events (workspace, id);
CREATE INDEX audit_events_at ON audit_events (at);
