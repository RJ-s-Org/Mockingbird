-- F7: OIDC console logins between `/auth/login` and `/auth/callback`
-- (PKCE verifier, nonce and return path), shared by every control node and
-- kept across restarts. Keyed by the hex sha256 of the `state`; single use
-- and short-lived (`expires_at`).

CREATE TABLE oidc_pending_logins (
    state_hash TEXT PRIMARY KEY,
    data       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX oidc_pending_logins_created_at ON oidc_pending_logins (created_at);
CREATE INDEX oidc_pending_logins_expires_at ON oidc_pending_logins (expires_at);
