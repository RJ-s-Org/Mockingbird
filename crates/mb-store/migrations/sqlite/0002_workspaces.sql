-- Workspaces and per-mock data-plane auth (SQLite).

CREATE TABLE workspaces (
    name         TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

-- Same text format sqlx writes for a `DateTime<Utc>`.
INSERT INTO workspaces (name, display_name, created_at)
    VALUES ('default', 'Default', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'));

INSERT OR IGNORE INTO workspaces (name, display_name, created_at)
    SELECT workspace, workspace, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    FROM (SELECT workspace FROM mocks UNION SELECT workspace FROM llm_profiles);

ALTER TABLE mocks ADD COLUMN data_plane_auth TEXT NOT NULL
    DEFAULT '{"mode":"none","header":"x-api-key"}';
