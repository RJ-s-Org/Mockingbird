-- Numbered revisions of each contract version's authored world description
-- (the overlay on its tier-0 description). They go with their version, and
-- with the mock.

CREATE TABLE world_revisions (
    id         TEXT PRIMARY KEY NOT NULL,
    mock_id    TEXT NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    version    INTEGER NOT NULL,
    revision   INTEGER NOT NULL,
    author     TEXT NOT NULL,
    document   TEXT NOT NULL,
    created_by TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (mock_id, version, revision),
    FOREIGN KEY (mock_id, version)
        REFERENCES contract_versions (mock_id, version) ON DELETE CASCADE
);
