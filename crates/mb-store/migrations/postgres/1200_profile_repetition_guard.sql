-- Whether replies of each LLM profile that fall into a loop are stopped
-- early and retried (mb_llm::repetition). A row written before this column
-- is on, the default.

ALTER TABLE llm_profiles ADD COLUMN repetition_guard BOOLEAN NOT NULL DEFAULT TRUE;
