use mb_store::StoreError;

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    /// No valid credential (maps to 401).
    #[error("{0}")]
    Unauthenticated(String),
    /// Authenticated but not allowed (maps to 403).
    #[error("{0}")]
    Forbidden(String),
    /// A malformed request (maps to 400).
    #[error("{0}")]
    Invalid(String),
    /// The identity provider failed or misbehaved.
    #[error("identity provider: {0}")]
    Upstream(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}
