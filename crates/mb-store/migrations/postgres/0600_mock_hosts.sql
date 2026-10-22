-- WS4: host names a mock answers for (a JSON array of strings); empty
-- answers for any host.

ALTER TABLE mocks ADD COLUMN hosts JSONB NOT NULL DEFAULT '[]';
