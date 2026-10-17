-- Hardening (P7): per-mock chaos and limits, and daily token usage.

ALTER TABLE mocks ADD COLUMN chaos JSONB;
ALTER TABLE mocks ADD COLUMN limits JSONB NOT NULL DEFAULT '{}';

CREATE TABLE token_usage (
    mock_id UUID NOT NULL REFERENCES mocks (id) ON DELETE CASCADE,
    day     DATE NOT NULL,
    tokens  BIGINT NOT NULL,
    PRIMARY KEY (mock_id, day)
);
