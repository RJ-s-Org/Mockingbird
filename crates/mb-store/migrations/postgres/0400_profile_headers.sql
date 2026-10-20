-- Extra provider request headers per LLM profile (a JSON object of
-- name -> value; values may embed `${env:NAME}` secret references).

ALTER TABLE llm_profiles ADD COLUMN headers JSONB NOT NULL DEFAULT '{}';
