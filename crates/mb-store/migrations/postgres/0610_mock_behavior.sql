-- Per-mock answer consistency: the response cache and list seeding
-- (consistency), unknown-id handling and the default scenario.

ALTER TABLE mocks ADD COLUMN behavior JSONB NOT NULL DEFAULT '{}';
