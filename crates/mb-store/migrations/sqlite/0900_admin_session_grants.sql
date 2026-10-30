-- D2: what a console session keeps to re-derive its roles after login.
-- `claims` are the identity provider's claims at login (or the last
-- refresh), `mapping_hash` names the role mapping that turned them into the
-- stored principal, `refresh_token` is the provider's refresh token sealed
-- with a key only the session cookie yields (opaque to the store), and
-- `refreshed_at` is when the roles were last refreshed. NULL for sessions
-- without a grant (created before this migration, or without OIDC).

ALTER TABLE admin_sessions ADD COLUMN claims TEXT;
ALTER TABLE admin_sessions ADD COLUMN mapping_hash TEXT;
ALTER TABLE admin_sessions ADD COLUMN refresh_token TEXT;
ALTER TABLE admin_sessions ADD COLUMN refreshed_at TEXT;
