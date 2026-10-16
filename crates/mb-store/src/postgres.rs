//! Postgres [`ControlStore`]. Uuids are `UUID`, timestamps `TIMESTAMPTZ` and
//! JSON is `json` (bound as text and cast, read back as text).
//!
//! Change notifications are in-process only, like SQLite; cross-node
//! `LISTEN/NOTIFY` arrives with HA.

use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    ACQUIRE_TIMEOUT, AdminSession, ApiKey, AuditEvent, AuditFilter, CHANGE_CHANNEL_CAPACITY,
    ContractStatus, ContractVersion, ControlChange, ControlStore, DEFAULT_WORKSPACE, LlmProfile,
    Mock, NewAuditEvent, NewContractVersion, NewMock, NewRequestLogEntry, PruneCounter,
    RequestLogEntry, RequestLogFilter, ResourceSource, Scenario, ScenarioRules, StoreError,
    Workspace, missing_workspace, now, sha256_hex, validate_header_name, validate_mock_name,
    validate_route_prefix, validate_workspace_name,
};

/// Migrations table name, distinct from other crates' migrators that may share
/// the same database.
const MIGRATIONS_TABLE: &str = "_mb_store_migrations";

const MOCK_COLUMNS: &str = "id, workspace, name, route_prefix, persona, llm_profile, header_overrides, source, \
     data_plane_auth, created_at, updated_at";
/// [`MOCK_COLUMNS`] with JSON read back as text.
const MOCK_SELECT: &str = "id, workspace, name, route_prefix, persona, llm_profile, header_overrides, source, \
     data_plane_auth::text AS data_plane_auth, created_at, updated_at";
const WORKSPACE_COLUMNS: &str = "name, display_name, created_at";
const VERSION_COLUMNS: &str =
    "id, mock_id, version, source_digest, source, bundle, status, created_by, created_at";
/// [`VERSION_COLUMNS`] with JSON read back as text.
const VERSION_SELECT: &str = "id, mock_id, version, source_digest, source, bundle::text AS bundle, status, created_by, created_at";
const PROFILE_COLUMNS: &str = "workspace, name, base_url, model, secret_ref, json_schema, temperature, timeout_ms, \
     max_tokens, fallback_profile, source";
const API_KEY_COLUMNS: &str = "key_id, workspace, name, role, scopes, sysadmin, secret_hash, created_by, \
     created_at, expires_at, last_used_at, revoked_at";
/// [`API_KEY_COLUMNS`] with JSON read back as text.
const API_KEY_SELECT: &str = "key_id, workspace, name, role, scopes::text AS scopes, sysadmin, secret_hash, \
     created_by, created_at, expires_at, last_used_at, revoked_at";
const SESSION_SELECT: &str =
    "id_hash, principal::text AS principal, created_at, expires_at, last_seen";
const AUDIT_SELECT: &str = "id, workspace, actor_subject, actor_kind, action, target, \
     before_value::text AS before_value, after_value::text AS after_value, request_id, at";
const LOG_SELECT: &str = "id, mock_id, operation_id, session, method, path, status, source, latency_ms, \
     prompt_tokens, completion_tokens, request::text AS request, response::text AS response, at";

pub struct PgStore {
    pool: PgPool,
    changes: broadcast::Sender<ControlChange>,
    prune: PruneCounter,
}

impl PgStore {
    /// Opens `postgres://…` (or `postgresql://…`) and runs migrations under
    /// Postgres's advisory lock.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|e| StoreError::Invalid(format!("postgres url: {e}")))?;
        let pool = PgPoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await?;
        let mut migrator = sqlx::migrate!("./migrations/postgres");
        migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
        migrator.run(&pool).await?;
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Ok(Self {
            pool,
            changes,
            prune: PruneCounter::default(),
        })
    }

    /// A store whose pool connects on first use and which runs no
    /// migrations (the schema must already exist). Unlike [`Self::connect`]
    /// it succeeds while the database is unreachable.
    pub fn connect_lazy(url: &str) -> Result<Self, StoreError> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|e| StoreError::Invalid(format!("postgres url: {e}")))?;
        let pool = PgPoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_lazy_with(options);
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Ok(Self {
            pool,
            changes,
            prune: PruneCounter::default(),
        })
    }

    fn publish(&self, change: ControlChange) {
        // No subscribers is fine.
        let _ = self.changes.send(change);
    }

    fn publish_mock(&self, workspace: &str, name: &str) {
        self.publish(ControlChange::Mock {
            workspace: workspace.to_string(),
            name: name.to_string(),
        });
    }
}

/// Workspace and name of the mock with `id`, row-locked for the rest of `tx`
/// with `lock` (a `FOR …` clause) so it cannot be deleted or renamed under it.
async fn locked_mock_key(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    lock: &'static str,
) -> Result<Option<(String, String)>, StoreError> {
    let sql = format!("SELECT workspace, name FROM mocks WHERE id = $1 {lock}");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|r| Ok((r.try_get("workspace")?, r.try_get("name")?)))
        .transpose()
}

async fn require_locked_mock_key(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    lock: &'static str,
) -> Result<(String, String), StoreError> {
    locked_mock_key(tx, id, lock)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("mock {id}")))
}

/// `NotFound` unless workspace `ws` exists; row-locked (`FOR KEY SHARE`) for
/// the rest of `tx` so it cannot be deleted under the caller's write.
async fn lock_workspace(tx: &mut Transaction<'_, Postgres>, ws: &str) -> Result<(), StoreError> {
    let exists: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM workspaces WHERE name = $1 FOR KEY SHARE")
            .bind(ws)
            .fetch_optional(&mut **tx)
            .await?;
    exists.map(|_| ()).ok_or_else(|| missing_workspace(ws))
}

fn workspace_from_row(row: &PgRow) -> Result<Workspace, StoreError> {
    Ok(Workspace {
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Blocks concurrent version writers for the mock (serializing version
/// numbering and activation) without blocking request-log inserts.
const LOCK_FOR_VERSIONS: &str = "FOR NO KEY UPDATE";
/// Keeps the mock from being deleted until the transaction ends.
const LOCK_KEEP: &str = "FOR KEY SHARE";

fn to_i64<T: TryInto<i64>>(value: T, field: &str) -> Result<i64, StoreError> {
    value
        .try_into()
        .map_err(|_| StoreError::Invalid(format!("{field} is out of range")))
}

fn from_i64<T: TryFrom<i64>>(value: i64, field: &str) -> Result<T, StoreError> {
    T::try_from(value)
        .map_err(|_| StoreError::Db(format!("stored {field} {value} is out of range")))
}

fn mock_from_row(row: &PgRow) -> Result<Mock, StoreError> {
    let source: String = row.try_get("source")?;
    let data_plane_auth: String = row.try_get("data_plane_auth")?;
    Ok(Mock {
        id: row.try_get("id")?,
        workspace: row.try_get("workspace")?,
        name: row.try_get("name")?,
        route_prefix: row.try_get("route_prefix")?,
        persona: row.try_get("persona")?,
        llm_profile: row.try_get("llm_profile")?,
        header_overrides: row.try_get("header_overrides")?,
        source: ResourceSource::parse(&source)?,
        data_plane_auth: serde_json::from_str(&data_plane_auth)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn version_from_row(row: &PgRow) -> Result<ContractVersion, StoreError> {
    let bundle: String = row.try_get("bundle")?;
    let status: String = row.try_get("status")?;
    Ok(ContractVersion {
        id: row.try_get("id")?,
        mock_id: row.try_get("mock_id")?,
        version: row.try_get("version")?,
        source_digest: row.try_get("source_digest")?,
        source: row.try_get("source")?,
        bundle: serde_json::from_str(&bundle)?,
        status: ContractStatus::parse(&status)?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
    })
}

fn scenario_from_row(row: &PgRow) -> Result<Scenario, StoreError> {
    let rules: String = row.try_get("rules")?;
    let source: String = row.try_get("source")?;
    Ok(Scenario {
        mock_id: row.try_get("mock_id")?,
        name: row.try_get("name")?,
        instructions: row.try_get("instructions")?,
        rules: serde_json::from_str::<ScenarioRules>(&rules)?,
        source: ResourceSource::parse(&source)?,
    })
}

fn profile_from_row(row: &PgRow) -> Result<LlmProfile, StoreError> {
    let source: String = row.try_get("source")?;
    let temperature: f64 = row.try_get("temperature")?;
    Ok(LlmProfile {
        workspace: row.try_get("workspace")?,
        name: row.try_get("name")?,
        base_url: row.try_get("base_url")?,
        model: row.try_get("model")?,
        secret_ref: row.try_get("secret_ref")?,
        json_schema: row.try_get("json_schema")?,
        // Stored from an f32, so the narrowing is exact.
        temperature: temperature as f32,
        timeout_ms: from_i64(row.try_get("timeout_ms")?, "timeout_ms")?,
        max_tokens: from_i64(row.try_get("max_tokens")?, "max_tokens")?,
        fallback_profile: row.try_get("fallback_profile")?,
        source: ResourceSource::parse(&source)?,
    })
}

fn log_from_row(row: &PgRow) -> Result<RequestLogEntry, StoreError> {
    let request: String = row.try_get("request")?;
    let response: Option<String> = row.try_get("response")?;
    let status: i32 = row.try_get("status")?;
    Ok(RequestLogEntry {
        id: row.try_get("id")?,
        mock_id: row.try_get("mock_id")?,
        operation_id: row.try_get("operation_id")?,
        session: row.try_get("session")?,
        method: row.try_get("method")?,
        path: row.try_get("path")?,
        status: from_i64(i64::from(status), "status")?,
        source: row.try_get("source")?,
        latency_ms: from_i64(row.try_get("latency_ms")?, "latency_ms")?,
        prompt_tokens: from_i64(row.try_get("prompt_tokens")?, "prompt_tokens")?,
        completion_tokens: from_i64(row.try_get("completion_tokens")?, "completion_tokens")?,
        request: serde_json::from_str(&request)?,
        response: response.map(|r| serde_json::from_str(&r)).transpose()?,
        at: row.try_get("at")?,
    })
}

fn api_key_from_row(row: &PgRow) -> Result<ApiKey, StoreError> {
    let scopes: String = row.try_get("scopes")?;
    Ok(ApiKey {
        key_id: row.try_get("key_id")?,
        workspace: row.try_get("workspace")?,
        name: row.try_get("name")?,
        role: row.try_get("role")?,
        scopes: serde_json::from_str(&scopes)?,
        sysadmin: row.try_get("sysadmin")?,
        secret_hash: row.try_get("secret_hash")?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_used_at: row.try_get("last_used_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

fn session_from_row(row: &PgRow) -> Result<AdminSession, StoreError> {
    let principal: String = row.try_get("principal")?;
    Ok(AdminSession {
        id_hash: row.try_get("id_hash")?,
        principal: serde_json::from_str(&principal)?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_seen: row.try_get("last_seen")?,
    })
}

fn audit_from_row(row: &PgRow) -> Result<AuditEvent, StoreError> {
    let before: Option<String> = row.try_get("before_value")?;
    let after: Option<String> = row.try_get("after_value")?;
    Ok(AuditEvent {
        id: row.try_get("id")?,
        workspace: row.try_get("workspace")?,
        actor_subject: row.try_get("actor_subject")?,
        actor_kind: row.try_get("actor_kind")?,
        action: row.try_get("action")?,
        target: row.try_get("target")?,
        before: before.map(|v| serde_json::from_str(&v)).transpose()?,
        after: after.map(|v| serde_json::from_str(&v)).transpose()?,
        request_id: row.try_get("request_id")?,
        at: row.try_get("at")?,
    })
}

fn mock_name_conflict(name: &str) -> impl FnOnce(sqlx::Error) -> StoreError + '_ {
    move |e| match StoreError::from(e) {
        StoreError::Conflict(_) => StoreError::Conflict(format!("mock {name:?} already exists")),
        other => other,
    }
}

#[async_trait]
impl ControlStore for PgStore {
    async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM workspaces ORDER BY name COLLATE \"C\"");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(workspace_from_row)
            .collect()
    }

    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, StoreError> {
        let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM workspaces WHERE name = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(workspace_from_row)
            .transpose()
    }

    async fn create_workspace(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Workspace, StoreError> {
        validate_workspace_name(name)?;
        let ws = Workspace {
            name: name.to_string(),
            display_name: display_name.to_string(),
            created_at: now(),
        };
        let sql = format!("INSERT INTO workspaces ({WORKSPACE_COLUMNS}) VALUES ($1, $2, $3)");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&ws.name)
            .bind(&ws.display_name)
            .bind(ws.created_at)
            .execute(&self.pool)
            .await
            .map_err(|e| match StoreError::from(e) {
                StoreError::Conflict(_) => {
                    StoreError::Conflict(format!("workspace {name:?} already exists"))
                }
                other => other,
            })?;
        self.publish(ControlChange::Workspace {
            name: ws.name.clone(),
        });
        Ok(ws)
    }

    async fn update_workspace(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Workspace, StoreError> {
        let sql = format!(
            "UPDATE workspaces SET display_name = $1 WHERE name = $2 RETURNING {WORKSPACE_COLUMNS}"
        );
        let ws = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(display_name)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(workspace_from_row)
            .transpose()?
            .ok_or_else(|| missing_workspace(name))?;
        self.publish(ControlChange::Workspace {
            name: ws.name.clone(),
        });
        Ok(ws)
    }

    async fn delete_workspace(&self, name: &str, force: bool) -> Result<bool, StoreError> {
        if name == DEFAULT_WORKSPACE {
            return Err(StoreError::Conflict(
                "the default workspace cannot be deleted".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        // Waits for in-flight writers holding the row (see `lock_workspace`)
        // and keeps new ones out until the transaction ends.
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM workspaces WHERE name = $1 FOR UPDATE")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Ok(false);
        }
        let mocks: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM mocks WHERE workspace = $1 ORDER BY name COLLATE \"C\"",
        )
        .bind(name)
        .fetch_all(&mut *tx)
        .await?;
        let profiles: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM llm_profiles WHERE workspace = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await?;
        if !force && (!mocks.is_empty() || profiles > 0) {
            return Err(StoreError::Conflict(format!(
                "workspace {name:?} is not empty ({} mocks, {profiles} profiles); delete with force",
                mocks.len()
            )));
        }
        for sql in [
            "DELETE FROM mocks WHERE workspace = $1",
            "DELETE FROM llm_profiles WHERE workspace = $1",
            "DELETE FROM api_keys WHERE workspace = $1",
            "DELETE FROM workspaces WHERE name = $1",
        ] {
            sqlx::query(sql).bind(name).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if !mocks.is_empty() {
            self.prune.clear();
        }
        for mock in &mocks {
            self.publish_mock(name, mock);
        }
        self.publish(ControlChange::Workspace {
            name: name.to_string(),
        });
        Ok(true)
    }

    async fn list_all_mocks(&self) -> Result<Vec<Mock>, StoreError> {
        let sql = format!(
            "SELECT {MOCK_SELECT} FROM mocks ORDER BY workspace COLLATE \"C\", name COLLATE \"C\""
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(mock_from_row)
            .collect()
    }

    async fn list_mocks(&self, ws: &str) -> Result<Vec<Mock>, StoreError> {
        // Byte order, matching SQLite, regardless of the database collation.
        let sql = format!(
            "SELECT {MOCK_SELECT} FROM mocks WHERE workspace = $1 ORDER BY name COLLATE \"C\""
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(mock_from_row)
            .collect()
    }

    async fn get_mock(&self, ws: &str, name: &str) -> Result<Option<Mock>, StoreError> {
        let sql = format!("SELECT {MOCK_SELECT} FROM mocks WHERE workspace = $1 AND name = $2");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(mock_from_row)
            .transpose()
    }

    async fn create_mock(&self, ws: &str, m: NewMock) -> Result<Mock, StoreError> {
        validate_mock_name(&m.name)?;
        let route_prefix = m.route_prefix.unwrap_or_else(|| format!("/{}", m.name));
        validate_route_prefix(&route_prefix)?;
        validate_header_name(&m.data_plane_auth.header)?;
        let data_plane_auth = serde_json::to_string(&m.data_plane_auth)?;
        let at = now();
        let mock = Mock {
            id: Uuid::new_v4(),
            workspace: ws.to_string(),
            name: m.name,
            route_prefix,
            persona: m.persona,
            llm_profile: m.llm_profile,
            header_overrides: m.header_overrides,
            source: m.source,
            data_plane_auth: m.data_plane_auth,
            created_at: at,
            updated_at: at,
        };
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, ws).await?;
        let sql = format!(
            "INSERT INTO mocks ({MOCK_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::json, $10, $11)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock.id)
            .bind(&mock.workspace)
            .bind(&mock.name)
            .bind(&mock.route_prefix)
            .bind(&mock.persona)
            .bind(&mock.llm_profile)
            .bind(mock.header_overrides)
            .bind(mock.source.as_str())
            .bind(data_plane_auth)
            .bind(mock.created_at)
            .bind(mock.updated_at)
            .execute(&mut *tx)
            .await
            .map_err(mock_name_conflict(&mock.name))?;
        tx.commit().await?;
        self.publish_mock(&mock.workspace, &mock.name);
        Ok(mock)
    }

    async fn update_mock(&self, m: &Mock) -> Result<Mock, StoreError> {
        validate_mock_name(&m.name)?;
        validate_route_prefix(&m.route_prefix)?;
        validate_header_name(&m.data_plane_auth.header)?;
        let data_plane_auth = serde_json::to_string(&m.data_plane_auth)?;
        let mut tx = self.pool.begin().await?;
        let sql = format!("SELECT {MOCK_SELECT} FROM mocks WHERE id = $1 FOR UPDATE");
        let existing = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(m.id)
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(mock_from_row)
            .transpose()?
            .ok_or_else(|| StoreError::NotFound(format!("mock {}", m.id)))?;
        let updated = Mock {
            id: existing.id,
            workspace: existing.workspace.clone(),
            created_at: existing.created_at,
            updated_at: now().max(existing.updated_at),
            ..m.clone()
        };
        sqlx::query(
            "UPDATE mocks SET name = $1, route_prefix = $2, persona = $3, llm_profile = $4, \
             header_overrides = $5, source = $6, data_plane_auth = $7::json, updated_at = $8 \
             WHERE id = $9",
        )
        .bind(&updated.name)
        .bind(&updated.route_prefix)
        .bind(&updated.persona)
        .bind(&updated.llm_profile)
        .bind(updated.header_overrides)
        .bind(updated.source.as_str())
        .bind(data_plane_auth)
        .bind(updated.updated_at)
        .bind(updated.id)
        .execute(&mut *tx)
        .await
        .map_err(mock_name_conflict(&updated.name))?;
        tx.commit().await?;
        if existing.name != updated.name {
            self.publish_mock(&existing.workspace, &existing.name);
        }
        self.publish_mock(&updated.workspace, &updated.name);
        Ok(updated)
    }

    async fn delete_mock(&self, ws: &str, name: &str) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM mocks WHERE workspace = $1 AND name = $2")
            .bind(ws)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0;
        if deleted {
            // Drops the deleted mock's counter (the others restart at a prune).
            self.prune.clear();
            self.publish_mock(ws, name);
        }
        Ok(deleted)
    }

    async fn add_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
    ) -> Result<ContractVersion, StoreError> {
        let bundle = serde_json::to_string(&v.bundle)?;
        let mut tx = self.pool.begin().await?;
        let (ws, name) = require_locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS).await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM contract_versions WHERE mock_id = $1",
        )
        .bind(mock_id)
        .fetch_one(&mut *tx)
        .await?;
        if v.status == ContractStatus::Active {
            sqlx::query(
                "UPDATE contract_versions SET status = 'approved' WHERE mock_id = $1 AND status = 'active'",
            )
            .bind(mock_id)
            .execute(&mut *tx)
            .await?;
        }
        let version = ContractVersion {
            id: Uuid::new_v4(),
            mock_id,
            version: next,
            source_digest: sha256_hex(&v.source),
            source: v.source,
            bundle: v.bundle,
            status: v.status,
            created_by: v.created_by,
            created_at: now(),
        };
        let sql = format!(
            "INSERT INTO contract_versions ({VERSION_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6::json, $7, $8, $9)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(version.id)
            .bind(mock_id)
            .bind(version.version)
            .bind(&version.source_digest)
            .bind(&version.source)
            .bind(bundle)
            .bind(version.status.as_str())
            .bind(&version.created_by)
            .bind(version.created_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.publish_mock(&ws, &name);
        Ok(version)
    }

    async fn list_contract_versions(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ContractVersion>, StoreError> {
        let sql = format!(
            "SELECT {VERSION_SELECT} FROM contract_versions WHERE mock_id = $1 ORDER BY version DESC"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(version_from_row)
            .collect()
    }

    async fn get_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<Option<ContractVersion>, StoreError> {
        let sql = format!(
            "SELECT {VERSION_SELECT} FROM contract_versions WHERE mock_id = $1 AND version = $2"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .bind(version)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(version_from_row)
            .transpose()
    }

    async fn activate_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractVersion, StoreError> {
        let mut tx = self.pool.begin().await?;
        let (ws, name) = require_locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS).await?;
        let sql = format!(
            "SELECT {VERSION_SELECT} FROM contract_versions WHERE mock_id = $1 AND version = $2"
        );
        let mut target = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .bind(version)
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(version_from_row)
            .transpose()?
            .ok_or_else(|| {
                StoreError::NotFound(format!("contract version {version} of mock {name:?}"))
            })?;
        if target.status == ContractStatus::Draft {
            return Err(StoreError::Invalid(format!(
                "contract version {version} is a draft; approve it before activating"
            )));
        }
        sqlx::query(
            "UPDATE contract_versions SET status = 'approved' \
             WHERE mock_id = $1 AND status = 'active' AND version <> $2",
        )
        .bind(mock_id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE contract_versions SET status = 'active' WHERE id = $1")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        target.status = ContractStatus::Active;
        self.publish_mock(&ws, &name);
        Ok(target)
    }

    async fn active_contract(&self, mock_id: Uuid) -> Result<Option<ContractVersion>, StoreError> {
        let sql = format!(
            "SELECT {VERSION_SELECT} FROM contract_versions WHERE mock_id = $1 AND status = 'active'"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(version_from_row)
            .transpose()
    }

    async fn list_scenarios(&self, mock_id: Uuid) -> Result<Vec<Scenario>, StoreError> {
        sqlx::query(
            "SELECT mock_id, name, instructions, rules::text AS rules, source FROM scenarios \
             WHERE mock_id = $1 ORDER BY name COLLATE \"C\"",
        )
        .bind(mock_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(scenario_from_row)
        .collect()
    }

    async fn put_scenario(&self, s: &Scenario) -> Result<(), StoreError> {
        let rules = serde_json::to_string(&s.rules)?;
        let mut tx = self.pool.begin().await?;
        let (ws, name) = require_locked_mock_key(&mut tx, s.mock_id, LOCK_KEEP).await?;
        sqlx::query(
            "INSERT INTO scenarios (mock_id, name, instructions, rules, source) \
             VALUES ($1, $2, $3, $4::json, $5) \
             ON CONFLICT (mock_id, name) DO UPDATE SET instructions = excluded.instructions, \
             rules = excluded.rules, source = excluded.source",
        )
        .bind(s.mock_id)
        .bind(&s.name)
        .bind(&s.instructions)
        .bind(rules)
        .bind(s.source.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.publish_mock(&ws, &name);
        Ok(())
    }

    async fn delete_scenario(&self, mock_id: Uuid, name: &str) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let key = locked_mock_key(&mut tx, mock_id, LOCK_KEEP).await?;
        let deleted = sqlx::query("DELETE FROM scenarios WHERE mock_id = $1 AND name = $2")
            .bind(mock_id)
            .bind(name)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        if deleted && let Some((ws, mock_name)) = key {
            self.publish_mock(&ws, &mock_name);
        }
        Ok(deleted)
    }

    async fn list_profiles(&self, ws: &str) -> Result<Vec<LlmProfile>, StoreError> {
        let sql = format!(
            "SELECT {PROFILE_COLUMNS} FROM llm_profiles WHERE workspace = $1 ORDER BY name COLLATE \"C\""
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(profile_from_row)
            .collect()
    }

    async fn get_profile(&self, ws: &str, name: &str) -> Result<Option<LlmProfile>, StoreError> {
        let sql = format!(
            "SELECT {PROFILE_COLUMNS} FROM llm_profiles WHERE workspace = $1 AND name = $2"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(profile_from_row)
            .transpose()
    }

    async fn put_profile(&self, p: &LlmProfile) -> Result<(), StoreError> {
        let sql = format!(
            "INSERT INTO llm_profiles ({PROFILE_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (workspace, name) DO UPDATE SET base_url = excluded.base_url, model = excluded.model, \
             secret_ref = excluded.secret_ref, json_schema = excluded.json_schema, \
             temperature = excluded.temperature, timeout_ms = excluded.timeout_ms, \
             max_tokens = excluded.max_tokens, fallback_profile = excluded.fallback_profile, \
             source = excluded.source"
        );
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, &p.workspace).await?;
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&p.workspace)
            .bind(&p.name)
            .bind(&p.base_url)
            .bind(&p.model)
            .bind(&p.secret_ref)
            .bind(p.json_schema)
            .bind(f64::from(p.temperature))
            .bind(to_i64(p.timeout_ms, "timeout_ms")?)
            .bind(i64::from(p.max_tokens))
            .bind(&p.fallback_profile)
            .bind(p.source.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.publish(ControlChange::Profile {
            workspace: p.workspace.clone(),
            name: p.name.clone(),
        });
        Ok(())
    }

    async fn delete_profile(&self, ws: &str, name: &str) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM llm_profiles WHERE workspace = $1 AND name = $2")
            .bind(ws)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0;
        if deleted {
            self.publish(ControlChange::Profile {
                workspace: ws.to_string(),
                name: name.to_string(),
            });
        }
        Ok(deleted)
    }

    async fn append_request_log(
        &self,
        e: NewRequestLogEntry,
        retention: u32,
    ) -> Result<i64, StoreError> {
        let response = e.response.as_ref().map(serde_json::to_string).transpose()?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO request_log (mock_id, operation_id, session, method, path, status, source, \
             latency_ms, prompt_tokens, completion_tokens, request, response, at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::json, $12::json, $13) RETURNING id",
        )
        .bind(e.mock_id)
        .bind(&e.operation_id)
        .bind(&e.session)
        .bind(&e.method)
        .bind(&e.path)
        .bind(i32::from(e.status))
        .bind(&e.source)
        .bind(to_i64(e.latency_ms, "latency_ms")?)
        .bind(i64::from(e.prompt_tokens))
        .bind(i64::from(e.completion_tokens))
        .bind(serde_json::to_string(&e.request)?)
        .bind(response)
        .bind(e.at)
        .fetch_one(&self.pool)
        .await?;
        if self.prune.tick(e.mock_id) {
            sqlx::query(
                "DELETE FROM request_log WHERE mock_id = $1 AND id <= \
                 (SELECT id FROM request_log WHERE mock_id = $1 ORDER BY id DESC OFFSET $2 LIMIT 1)",
            )
            .bind(e.mock_id)
            .bind(i64::from(retention))
            .execute(&self.pool)
            .await?;
        }
        Ok(id)
    }

    async fn list_request_log(
        &self,
        mock_id: Uuid,
        f: &RequestLogFilter,
    ) -> Result<Vec<RequestLogEntry>, StoreError> {
        let mut qb = QueryBuilder::<Postgres>::new(format!(
            "SELECT {LOG_SELECT} FROM request_log WHERE mock_id = "
        ));
        qb.push_bind(mock_id);
        if let Some(op) = &f.operation_id {
            qb.push(" AND operation_id = ").push_bind(op.clone());
        }
        if let Some(session) = &f.session {
            qb.push(" AND session = ").push_bind(session.clone());
        }
        if let Some(source) = &f.source {
            qb.push(" AND source = ").push_bind(source.clone());
        }
        if let Some(before) = f.before_id {
            qb.push(" AND id < ").push_bind(before);
        }
        qb.push(" ORDER BY id DESC LIMIT ")
            .push_bind(i64::from(f.effective_limit()));
        qb.build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(log_from_row)
            .collect()
    }

    async fn create_api_key(&self, k: &ApiKey) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, &k.workspace).await?;
        let sql = format!(
            "INSERT INTO api_keys ({API_KEY_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5::json, $6, $7, $8, $9, $10, $11, $12)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&k.key_id)
            .bind(&k.workspace)
            .bind(&k.name)
            .bind(&k.role)
            .bind(serde_json::to_string(&k.scopes)?)
            .bind(k.sysadmin)
            .bind(&k.secret_hash)
            .bind(&k.created_by)
            .bind(k.created_at)
            .bind(k.expires_at)
            .bind(k.last_used_at)
            .bind(k.revoked_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| match StoreError::from(e) {
                StoreError::Conflict(_) => {
                    StoreError::Conflict(format!("api key {:?} already exists", k.key_id))
                }
                other => other,
            })?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_api_key(&self, key_id: &str) -> Result<Option<ApiKey>, StoreError> {
        let sql = format!("SELECT {API_KEY_SELECT} FROM api_keys WHERE key_id = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(api_key_from_row)
            .transpose()
    }

    async fn list_api_keys(&self, ws: &str) -> Result<Vec<ApiKey>, StoreError> {
        let sql = format!(
            "SELECT {API_KEY_SELECT} FROM api_keys WHERE workspace = $1 \
             ORDER BY created_at DESC, key_id COLLATE \"C\""
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(api_key_from_row)
            .collect()
    }

    async fn revoke_api_key(&self, ws: &str, key_id: &str) -> Result<bool, StoreError> {
        let revoked = sqlx::query(
            "UPDATE api_keys SET revoked_at = $1 \
             WHERE workspace = $2 AND key_id = $3 AND revoked_at IS NULL",
        )
        .bind(now())
        .bind(ws)
        .bind(key_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;
        Ok(revoked)
    }

    async fn touch_api_key(&self, key_id: &str, at: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("UPDATE api_keys SET last_used_at = $1 WHERE key_id = $2")
            .bind(at)
            .bind(key_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_admin_session(&self, s: &AdminSession) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO admin_sessions (id_hash, principal, created_at, expires_at, last_seen) \
             VALUES ($1, $2::json, $3, $4, $5)",
        )
        .bind(&s.id_hash)
        .bind(serde_json::to_string(&s.principal)?)
        .bind(s.created_at)
        .bind(s.expires_at)
        .bind(s.last_seen)
        .execute(&self.pool)
        .await
        .map_err(|e| match StoreError::from(e) {
            StoreError::Conflict(_) => StoreError::Conflict("admin session already exists".into()),
            other => other,
        })?;
        Ok(())
    }

    async fn get_admin_session(&self, id_hash: &str) -> Result<Option<AdminSession>, StoreError> {
        let sql = format!("SELECT {SESSION_SELECT} FROM admin_sessions WHERE id_hash = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id_hash)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(session_from_row)
            .transpose()
    }

    async fn touch_admin_session(
        &self,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE admin_sessions SET last_seen = $1 WHERE id_hash = $2")
            .bind(at)
            .bind(id_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_admin_session(&self, id_hash: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query("DELETE FROM admin_sessions WHERE id_hash = $1")
            .bind(id_hash)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    async fn delete_expired_admin_sessions(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(
            sqlx::query("DELETE FROM admin_sessions WHERE expires_at <= $1")
                .bind(now)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    async fn append_audit(&self, e: NewAuditEvent) -> Result<AuditEvent, StoreError> {
        let before = e.before.as_ref().map(serde_json::to_string).transpose()?;
        let after = e.after.as_ref().map(serde_json::to_string).transpose()?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO audit_events (workspace, actor_subject, actor_kind, action, target, \
             before_value, after_value, request_id, at) \
             VALUES ($1, $2, $3, $4, $5, $6::json, $7::json, $8, $9) RETURNING id",
        )
        .bind(&e.workspace)
        .bind(&e.actor_subject)
        .bind(&e.actor_kind)
        .bind(&e.action)
        .bind(&e.target)
        .bind(before)
        .bind(after)
        .bind(&e.request_id)
        .bind(e.at)
        .fetch_one(&self.pool)
        .await?;
        Ok(AuditEvent {
            id,
            workspace: e.workspace,
            actor_subject: e.actor_subject,
            actor_kind: e.actor_kind,
            action: e.action,
            target: e.target,
            before: e.before,
            after: e.after,
            request_id: e.request_id,
            at: e.at,
        })
    }

    async fn list_audit(&self, f: &AuditFilter) -> Result<Vec<AuditEvent>, StoreError> {
        let mut qb = QueryBuilder::<Postgres>::new(format!(
            "SELECT {AUDIT_SELECT} FROM audit_events WHERE TRUE"
        ));
        if let Some(ws) = &f.workspace {
            qb.push(" AND workspace = ").push_bind(ws.clone());
        }
        if let Some(action) = &f.action {
            qb.push(" AND action = ").push_bind(action.clone());
        }
        if let Some(actor) = &f.actor {
            qb.push(" AND actor_subject = ").push_bind(actor.clone());
        }
        if let Some(before) = f.before_id {
            qb.push(" AND id < ").push_bind(before);
        }
        qb.push(" ORDER BY id DESC LIMIT ")
            .push_bind(i64::from(f.effective_limit()));
        qb.build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(audit_from_row)
            .collect()
    }

    async fn prune_audit(&self, older_than: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(sqlx::query("DELETE FROM audit_events WHERE at < $1")
            .bind(older_than)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    fn subscribe(&self) -> broadcast::Receiver<ControlChange> {
        self.changes.subscribe()
    }
}
