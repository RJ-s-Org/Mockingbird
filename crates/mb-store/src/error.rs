#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("database: {0}")]
    Db(String),
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict(db.message().to_string())
            }
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                StoreError::NotFound(db.message().to_string())
            }
            _ => StoreError::Db(err.to_string()),
        }
    }
}

impl From<sqlx::migrate::MigrateError> for StoreError {
    fn from(err: sqlx::migrate::MigrateError) -> Self {
        StoreError::Db(format!("migration failed: {err}"))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        StoreError::Db(format!("json: {err}"))
    }
}
