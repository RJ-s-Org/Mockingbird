-- Compilation warnings of a contract version (schema too large for the
-- generation prompt) and the merge/replace mode of an import job.

ALTER TABLE contract_versions ADD COLUMN warnings JSON;
ALTER TABLE import_jobs ADD COLUMN mode TEXT;
