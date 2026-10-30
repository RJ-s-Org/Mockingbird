-- The tenant an import job's LLM calls are billed to, resolved when the job
-- was created, so the node that runs it (after a restart, or another node)
-- bills the same one. NULL (a job queued before this column) bills the
-- default tenant.

ALTER TABLE import_jobs ADD COLUMN tenant TEXT;
