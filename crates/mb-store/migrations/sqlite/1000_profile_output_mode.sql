-- How each LLM profile asks for JSON: `none`, `json_object` or
-- `json_schema`. NULL (a row written before this column) reads as the
-- `json_schema` flag, which is still written for older servers.

ALTER TABLE llm_profiles ADD COLUMN output_mode TEXT;
