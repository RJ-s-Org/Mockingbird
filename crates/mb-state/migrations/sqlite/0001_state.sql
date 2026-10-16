-- Session-scoped runtime state. Timestamps are Unix epoch milliseconds.
CREATE TABLE state_sessions (
    workspace    TEXT    NOT NULL,
    mock         TEXT    NOT NULL,
    session      TEXT    NOT NULL,
    last_touched INTEGER NOT NULL,
    -- Last recency sequence handed to a resource of this session.
    seq          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace, mock, session)
);

CREATE INDEX state_sessions_last_touched ON state_sessions (last_touched);

CREATE TABLE state_resources (
    workspace     TEXT    NOT NULL,
    mock          TEXT    NOT NULL,
    session       TEXT    NOT NULL,
    resource_type TEXT    NOT NULL,
    id            TEXT    NOT NULL,
    seq           INTEGER NOT NULL,
    data          TEXT    NOT NULL,
    PRIMARY KEY (workspace, mock, session, resource_type, id),
    FOREIGN KEY (workspace, mock, session)
        REFERENCES state_sessions (workspace, mock, session) ON DELETE CASCADE
);

CREATE INDEX state_resources_recency
    ON state_resources (workspace, mock, session, resource_type, seq);
