-- Why the faker answered a request (an LLM failure, or no LLM configured),
-- and extra JSON merged into an LLM profile's request body (provider routing,
-- reasoning settings).

ALTER TABLE request_log ADD COLUMN fallback_reason TEXT;
ALTER TABLE llm_profiles ADD COLUMN extra_body JSONB NOT NULL DEFAULT '{}';
