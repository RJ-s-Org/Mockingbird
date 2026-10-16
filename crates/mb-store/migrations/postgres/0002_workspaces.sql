-- Workspaces and per-mock data-plane auth (Postgres).

CREATE TABLE workspaces (
    name         TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL
);

INSERT INTO workspaces (name, display_name, created_at)
    VALUES ('default', 'Default', date_trunc('microseconds', now()));

INSERT INTO workspaces (name, display_name, created_at)
    SELECT workspace, workspace, date_trunc('microseconds', now())
    FROM (SELECT workspace FROM mocks UNION SELECT workspace FROM llm_profiles) AS existing
    ON CONFLICT (name) DO NOTHING;

ALTER TABLE mocks ADD COLUMN data_plane_auth JSON NOT NULL
    DEFAULT '{"mode":"none","header":"x-api-key"}';
