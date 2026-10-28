-- WS6: where a mock POSTs webhook events after successful writes (a JSON
-- array of targets; secrets are references only).

ALTER TABLE mocks ADD COLUMN webhooks TEXT NOT NULL DEFAULT '[]';
