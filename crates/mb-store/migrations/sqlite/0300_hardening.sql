-- Hardening (P7): per-mock chaos and limits, and daily token usage.

ALTER TABLE mocks ADD COLUMN chaos TEXT;
ALTER TABLE mocks ADD COLUMN limits TEXT NOT NULL DEFAULT '{}';

CREATE TABLE token_usage (
    mock_id TEXT NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    day     TEXT NOT NULL,
    tokens  BIGINT NOT NULL,
    PRIMARY KEY (mock_id, day)
);
