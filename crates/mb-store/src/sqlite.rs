//! SQLite [`ControlStore`]. Uuids, timestamps and JSON are stored as TEXT.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tokio::sync::broadcast;
use uuid::Uuid;

use chrono::{DateTime, Utc};

use mb_mcm::Bundle;
use serde_json::Value;

use crate::{
    ACQUIRE_TIMEOUT, AdminSession, ApiKey, AuditEvent, AuditFilter, CHANGE_CHANNEL_CAPACITY,
    ContractStatus, ContractVersion, ControlChange, ControlStore, DEFAULT_WORKSPACE, ImportJob,
    ImportJobStatus, ImportOutcome, ImportProgress, LlmProfile, MAX_IMPORT_JOB_LIST, Mock,
    NewAuditEvent, NewContractVersion, NewImportJob, NewMock, NewRequestLogEntry, PruneCounter,
    RequestLogEntry, RequestLogFilter, ResourceSource, STALE_IMPORT_ERROR, Scenario, ScenarioRules,
    StoreError, Workspace, missing_workspace, not_a_draft, now, queued_progress,
    require_draft_status, sha256_hex, validate_header_name, validate_mock_name,
    validate_route_prefix, validate_workspace_name,
};

/// Migrations table name, distinct from other crates' migrators that may share
/// the same database.
const MIGRATIONS_TABLE: &str = "_mb_store_migrations";

const MOCK_COLUMNS: &str = "id, workspace, name, route_prefix, persona, llm_profile, header_overrides, source, \
     data_plane_auth, created_at, updated_at";
const WORKSPACE_COLUMNS: &str = "name, display_name, created_at";
const VERSION_COLUMNS: &str = "id, mock_id, version, source_digest, source, bundle, status, created_by, created_at, \
     draft_meta";
const PROFILE_COLUMNS: &str = "workspace, name, base_url, model, secret_ref, json_schema, temperature, timeout_ms, \
     max_tokens, fallback_profile, source";
const API_KEY_COLUMNS: &str = "key_id, workspace, name, role, scopes, sysadmin, secret_hash, created_by, \
     created_at, expires_at, last_used_at, revoked_at";
const SESSION_COLUMNS: &str = "id_hash, principal, created_at, expires_at, last_seen";
const AUDIT_COLUMNS: &str = "id, workspace, actor_subject, actor_kind, action, target, before_value, after_value, \
     request_id, at";
/// Every `import_jobs` column but `input`.
const JOB_COLUMNS: &str = "id, workspace, mock_id, mock, kind, source_name, status, progress, result_version, \
     error, created_by, created_at, started_at, finished_at, updated_at";
const LOG_COLUMNS: &str = "id, mock_id, operation_id, session, method, path, status, source, latency_ms, \
     prompt_tokens, completion_tokens, request, response, at";

pub struct SqliteControlStore {
    pool: SqlitePool,
    changes: broadcast::Sender<ControlChange>,
    prune: PruneCounter,
}

impl SqliteControlStore {
    /// Opens `sqlite::memory:` or `sqlite://<path>` (created if missing) and
    /// runs migrations.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(|e| StoreError::Invalid(format!("sqlite url: {e}")))?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        let pool = if in_memory {
            // One connection that never closes, so every query sees the same
            // in-memory database for the store's whole lifetime.
            SqlitePoolOptions::new()
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(options)
                .await?
        } else {
            SqlitePoolOptions::new()
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .connect_with(options.journal_mode(SqliteJournalMode::Wal))
                .await?
        };
        let mut migrator = sqlx::migrate!("./migrations/sqlite");
        migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
        migrator.run(&pool).await?;
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

    /// Workspace and name of the mock with `id`.
    async fn mock_key(&self, id: Uuid) -> Result<Option<(String, String)>, StoreError> {
        let row = sqlx::query("SELECT workspace, name FROM mocks WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| Ok((r.try_get("workspace")?, r.try_get("name")?)))
            .transpose()
    }

    async fn require_mock_key(&self, id: Uuid) -> Result<(String, String), StoreError> {
        self.mock_key(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("mock {id}")))
    }

    /// Inserts `v` as the mock's next version, demoting the active version
    /// when `v` is `Active`. Publishes nothing.
    async fn insert_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Option<Value>,
    ) -> Result<ContractVersion, StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let version = Self::insert_version_in(&mut tx, mock_id, v, draft_meta).await?;
        tx.commit().await?;
        Ok(version)
    }

    /// [`Self::insert_version`] inside the caller's transaction.
    async fn insert_version_in(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Option<Value>,
    ) -> Result<ContractVersion, StoreError> {
        let bundle = serde_json::to_string(&v.bundle)?;
        let meta = draft_meta.as_ref().map(serde_json::to_string).transpose()?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM contract_versions WHERE mock_id = ?",
        )
        .bind(mock_id.to_string())
        .fetch_one(&mut **tx)
        .await?;
        if v.status == ContractStatus::Active {
            sqlx::query("UPDATE contract_versions SET status = 'approved' WHERE mock_id = ? AND status = 'active'")
                .bind(mock_id.to_string())
                .execute(&mut **tx)
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
            draft_meta,
        };
        let sql = format!(
            "INSERT INTO contract_versions ({VERSION_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(version.id.to_string())
            .bind(mock_id.to_string())
            .bind(version.version)
            .bind(&version.source_digest)
            .bind(&version.source)
            .bind(bundle)
            .bind(version.status.as_str())
            .bind(&version.created_by)
            .bind(version.created_at)
            .bind(meta)
            .execute(&mut **tx)
            .await?;
        Ok(version)
    }

    /// The version's status inside `tx`; `NotFound` if it does not exist.
    async fn version_status(
        conn: &mut sqlx::SqliteConnection,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractStatus, StoreError> {
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM contract_versions WHERE mock_id = ? AND version = ?",
        )
        .bind(mock_id.to_string())
        .bind(version)
        .fetch_optional(conn)
        .await?;
        let status = status.ok_or_else(|| {
            StoreError::NotFound(format!("contract version {version} of mock {mock_id}"))
        })?;
        ContractStatus::parse(&status)
    }
}

/// `NotFound` unless workspace `ws` exists.
async fn require_workspace(conn: &mut sqlx::SqliteConnection, ws: &str) -> Result<(), StoreError> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM workspaces WHERE name = ?")
        .bind(ws)
        .fetch_optional(conn)
        .await?;
    exists.map(|_| ()).ok_or_else(|| missing_workspace(ws))
}

fn workspace_from_row(row: &SqliteRow) -> Result<Workspace, StoreError> {
    Ok(Workspace {
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        created_at: row.try_get("created_at")?,
    })
}

fn to_i64<T: TryInto<i64>>(value: T, field: &str) -> Result<i64, StoreError> {
    value
        .try_into()
        .map_err(|_| StoreError::Invalid(format!("{field} is out of range")))
}

fn from_i64<T: TryFrom<i64>>(value: i64, field: &str) -> Result<T, StoreError> {
    T::try_from(value)
        .map_err(|_| StoreError::Db(format!("stored {field} {value} is out of range")))
}

fn parse_uuid(s: &str) -> Result<Uuid, StoreError> {
    Uuid::parse_str(s).map_err(|e| StoreError::Db(format!("stored uuid {s:?}: {e}")))
}

fn mock_from_row(row: &SqliteRow) -> Result<Mock, StoreError> {
    let id: String = row.try_get("id")?;
    let source: String = row.try_get("source")?;
    let data_plane_auth: String = row.try_get("data_plane_auth")?;
    Ok(Mock {
        id: parse_uuid(&id)?,
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

fn version_from_row(row: &SqliteRow) -> Result<ContractVersion, StoreError> {
    let id: String = row.try_get("id")?;
    let mock_id: String = row.try_get("mock_id")?;
    let bundle: String = row.try_get("bundle")?;
    let status: String = row.try_get("status")?;
    let draft_meta: Option<String> = row.try_get("draft_meta")?;
    Ok(ContractVersion {
        id: parse_uuid(&id)?,
        mock_id: parse_uuid(&mock_id)?,
        version: row.try_get("version")?,
        source_digest: row.try_get("source_digest")?,
        source: row.try_get("source")?,
        bundle: serde_json::from_str(&bundle)?,
        status: ContractStatus::parse(&status)?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        draft_meta: draft_meta.map(|m| serde_json::from_str(&m)).transpose()?,
    })
}

fn job_from_row(row: &SqliteRow) -> Result<ImportJob, StoreError> {
    let id: String = row.try_get("id")?;
    let mock_id: String = row.try_get("mock_id")?;
    let status: String = row.try_get("status")?;
    let progress: String = row.try_get("progress")?;
    Ok(ImportJob {
        id: parse_uuid(&id)?,
        workspace: row.try_get("workspace")?,
        mock_id: parse_uuid(&mock_id)?,
        mock: row.try_get("mock")?,
        kind: row.try_get("kind")?,
        source_name: row.try_get("source_name")?,
        status: ImportJobStatus::parse(&status)?,
        progress: serde_json::from_str::<ImportProgress>(&progress)?,
        result_version: row.try_get("result_version")?,
        error: row.try_get("error")?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn scenario_from_row(row: &SqliteRow) -> Result<Scenario, StoreError> {
    let mock_id: String = row.try_get("mock_id")?;
    let rules: String = row.try_get("rules")?;
    let source: String = row.try_get("source")?;
    Ok(Scenario {
        mock_id: parse_uuid(&mock_id)?,
        name: row.try_get("name")?,
        instructions: row.try_get("instructions")?,
        rules: serde_json::from_str::<ScenarioRules>(&rules)?,
        source: ResourceSource::parse(&source)?,
    })
}

fn profile_from_row(row: &SqliteRow) -> Result<LlmProfile, StoreError> {
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

fn api_key_from_row(row: &SqliteRow) -> Result<ApiKey, StoreError> {
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

fn session_from_row(row: &SqliteRow) -> Result<AdminSession, StoreError> {
    let principal: String = row.try_get("principal")?;
    Ok(AdminSession {
        id_hash: row.try_get("id_hash")?,
        principal: serde_json::from_str(&principal)?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_seen: row.try_get("last_seen")?,
    })
}

fn audit_from_row(row: &SqliteRow) -> Result<AuditEvent, StoreError> {
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

fn log_from_row(row: &SqliteRow) -> Result<RequestLogEntry, StoreError> {
    let mock_id: String = row.try_get("mock_id")?;
    let request: String = row.try_get("request")?;
    let response: Option<String> = row.try_get("response")?;
    Ok(RequestLogEntry {
        id: row.try_get("id")?,
        mock_id: parse_uuid(&mock_id)?,
        operation_id: row.try_get("operation_id")?,
        session: row.try_get("session")?,
        method: row.try_get("method")?,
        path: row.try_get("path")?,
        status: from_i64(row.try_get("status")?, "status")?,
        source: row.try_get("source")?,
        latency_ms: from_i64(row.try_get("latency_ms")?, "latency_ms")?,
        prompt_tokens: from_i64(row.try_get("prompt_tokens")?, "prompt_tokens")?,
        completion_tokens: from_i64(row.try_get("completion_tokens")?, "completion_tokens")?,
        request: serde_json::from_str(&request)?,
        response: response.map(|r| serde_json::from_str(&r)).transpose()?,
        at: row.try_get("at")?,
    })
}

#[async_trait]
impl ControlStore for SqliteControlStore {
    async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM workspaces ORDER BY name");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(workspace_from_row)
            .collect()
    }

    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, StoreError> {
        let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM workspaces WHERE name = ?");
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
        let sql = format!("INSERT INTO workspaces ({WORKSPACE_COLUMNS}) VALUES (?, ?, ?)");
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
            "UPDATE workspaces SET display_name = ? WHERE name = ? RETURNING {WORKSPACE_COLUMNS}"
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM workspaces WHERE name = ?")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Ok(false);
        }
        let mocks: Vec<String> =
            sqlx::query_scalar("SELECT name FROM mocks WHERE workspace = ? ORDER BY name")
                .bind(name)
                .fetch_all(&mut *tx)
                .await?;
        let profiles: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM llm_profiles WHERE workspace = ?")
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
            "DELETE FROM mocks WHERE workspace = ?",
            "DELETE FROM llm_profiles WHERE workspace = ?",
            "DELETE FROM api_keys WHERE workspace = ?",
            "DELETE FROM workspaces WHERE name = ?",
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
        let sql = format!("SELECT {MOCK_COLUMNS} FROM mocks ORDER BY workspace, name");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(mock_from_row)
            .collect()
    }

    async fn list_mocks(&self, ws: &str) -> Result<Vec<Mock>, StoreError> {
        let sql = format!("SELECT {MOCK_COLUMNS} FROM mocks WHERE workspace = ? ORDER BY name");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(mock_from_row)
            .collect()
    }

    async fn get_mock(&self, ws: &str, name: &str) -> Result<Option<Mock>, StoreError> {
        let sql = format!("SELECT {MOCK_COLUMNS} FROM mocks WHERE workspace = ? AND name = ?");
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_workspace(&mut tx, ws).await?;
        let sql =
            format!("INSERT INTO mocks ({MOCK_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock.id.to_string())
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
            .map_err(|e| match StoreError::from(e) {
                StoreError::Conflict(_) => {
                    StoreError::Conflict(format!("mock {:?} already exists", mock.name))
                }
                other => other,
            })?;
        tx.commit().await?;
        self.publish_mock(&mock.workspace, &mock.name);
        Ok(mock)
    }

    async fn update_mock(&self, m: &Mock) -> Result<Mock, StoreError> {
        validate_mock_name(&m.name)?;
        validate_route_prefix(&m.route_prefix)?;
        validate_header_name(&m.data_plane_auth.header)?;
        let data_plane_auth = serde_json::to_string(&m.data_plane_auth)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sql = format!("SELECT {MOCK_COLUMNS} FROM mocks WHERE id = ?");
        let existing = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(m.id.to_string())
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
            "UPDATE mocks SET name = ?, route_prefix = ?, persona = ?, llm_profile = ?, header_overrides = ?, \
             source = ?, data_plane_auth = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&updated.name)
        .bind(&updated.route_prefix)
        .bind(&updated.persona)
        .bind(&updated.llm_profile)
        .bind(updated.header_overrides)
        .bind(updated.source.as_str())
        .bind(data_plane_auth)
        .bind(updated.updated_at)
        .bind(updated.id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| match StoreError::from(e) {
            StoreError::Conflict(_) => StoreError::Conflict(format!("mock {:?} already exists", updated.name)),
            other => other,
        })?;
        tx.commit().await?;
        if existing.name != updated.name {
            self.publish_mock(&existing.workspace, &existing.name);
        }
        self.publish_mock(&updated.workspace, &updated.name);
        Ok(updated)
    }

    async fn delete_mock(&self, ws: &str, name: &str) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM mocks WHERE workspace = ? AND name = ?")
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
        let (ws, name) = self.require_mock_key(mock_id).await?;
        let version = self.insert_version(mock_id, v, None).await?;
        self.publish_mock(&ws, &name);
        Ok(version)
    }

    async fn list_contract_versions(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ContractVersion>, StoreError> {
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM contract_versions WHERE mock_id = ? ORDER BY version DESC"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id.to_string())
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
            "SELECT {VERSION_COLUMNS} FROM contract_versions WHERE mock_id = ? AND version = ?"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id.to_string())
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
        let (ws, name) = self.require_mock_key(mock_id).await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM contract_versions WHERE mock_id = ? AND version = ?"
        );
        let mut target = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id.to_string())
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
            "UPDATE contract_versions SET status = 'approved' WHERE mock_id = ? AND status = 'active' AND version <> ?",
        )
        .bind(mock_id.to_string())
        .bind(version)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE contract_versions SET status = 'active' WHERE id = ?")
            .bind(target.id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        target.status = ContractStatus::Active;
        self.publish_mock(&ws, &name);
        Ok(target)
    }

    async fn active_contract(&self, mock_id: Uuid) -> Result<Option<ContractVersion>, StoreError> {
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM contract_versions WHERE mock_id = ? AND status = 'active'"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(version_from_row)
            .transpose()
    }

    async fn list_scenarios(&self, mock_id: Uuid) -> Result<Vec<Scenario>, StoreError> {
        sqlx::query("SELECT mock_id, name, instructions, rules, source FROM scenarios WHERE mock_id = ? ORDER BY name")
            .bind(mock_id.to_string())
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(scenario_from_row)
            .collect()
    }

    async fn put_scenario(&self, s: &Scenario) -> Result<(), StoreError> {
        let (ws, name) = self.require_mock_key(s.mock_id).await?;
        sqlx::query(
            "INSERT INTO scenarios (mock_id, name, instructions, rules, source) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (mock_id, name) DO UPDATE SET instructions = excluded.instructions, \
             rules = excluded.rules, source = excluded.source",
        )
        .bind(s.mock_id.to_string())
        .bind(&s.name)
        .bind(&s.instructions)
        .bind(serde_json::to_string(&s.rules)?)
        .bind(s.source.as_str())
        .execute(&self.pool)
        .await?;
        self.publish_mock(&ws, &name);
        Ok(())
    }

    async fn delete_scenario(&self, mock_id: Uuid, name: &str) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM scenarios WHERE mock_id = ? AND name = ?")
            .bind(mock_id.to_string())
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0;
        if deleted && let Some((ws, mock_name)) = self.mock_key(mock_id).await? {
            self.publish_mock(&ws, &mock_name);
        }
        Ok(deleted)
    }

    async fn list_profiles(&self, ws: &str) -> Result<Vec<LlmProfile>, StoreError> {
        let sql =
            format!("SELECT {PROFILE_COLUMNS} FROM llm_profiles WHERE workspace = ? ORDER BY name");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ws)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(profile_from_row)
            .collect()
    }

    async fn get_profile(&self, ws: &str, name: &str) -> Result<Option<LlmProfile>, StoreError> {
        let sql =
            format!("SELECT {PROFILE_COLUMNS} FROM llm_profiles WHERE workspace = ? AND name = ?");
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
            "INSERT INTO llm_profiles ({PROFILE_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (workspace, name) DO UPDATE SET base_url = excluded.base_url, model = excluded.model, \
             secret_ref = excluded.secret_ref, json_schema = excluded.json_schema, \
             temperature = excluded.temperature, timeout_ms = excluded.timeout_ms, \
             max_tokens = excluded.max_tokens, fallback_profile = excluded.fallback_profile, \
             source = excluded.source"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_workspace(&mut tx, &p.workspace).await?;
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
        let deleted = sqlx::query("DELETE FROM llm_profiles WHERE workspace = ? AND name = ?")
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
            "INSERT INTO request_log (mock_id, operation_id, session, method, path, status, source, latency_ms, \
             prompt_tokens, completion_tokens, request, response, at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(e.mock_id.to_string())
        .bind(&e.operation_id)
        .bind(&e.session)
        .bind(&e.method)
        .bind(&e.path)
        .bind(i64::from(e.status))
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
                "DELETE FROM request_log WHERE mock_id = ?1 AND id NOT IN \
                 (SELECT id FROM request_log WHERE mock_id = ?1 ORDER BY id DESC LIMIT ?2)",
            )
            .bind(e.mock_id.to_string())
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
        let mut qb = QueryBuilder::<Sqlite>::new(format!(
            "SELECT {LOG_COLUMNS} FROM request_log WHERE mock_id = "
        ));
        qb.push_bind(mock_id.to_string());
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_workspace(&mut tx, &k.workspace).await?;
        let sql = format!(
            "INSERT INTO api_keys ({API_KEY_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
        let sql = format!("SELECT {API_KEY_COLUMNS} FROM api_keys WHERE key_id = ?");
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
            "SELECT {API_KEY_COLUMNS} FROM api_keys WHERE workspace = ? \
             ORDER BY created_at DESC, key_id"
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
            "UPDATE api_keys SET revoked_at = ? \
             WHERE workspace = ? AND key_id = ? AND revoked_at IS NULL",
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
        sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE key_id = ?")
            .bind(at)
            .bind(key_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_admin_session(&self, s: &AdminSession) -> Result<(), StoreError> {
        let sql = format!("INSERT INTO admin_sessions ({SESSION_COLUMNS}) VALUES (?, ?, ?, ?, ?)");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&s.id_hash)
            .bind(serde_json::to_string(&s.principal)?)
            .bind(s.created_at)
            .bind(s.expires_at)
            .bind(s.last_seen)
            .execute(&self.pool)
            .await
            .map_err(|e| match StoreError::from(e) {
                StoreError::Conflict(_) => {
                    StoreError::Conflict("admin session already exists".into())
                }
                other => other,
            })?;
        Ok(())
    }

    async fn get_admin_session(&self, id_hash: &str) -> Result<Option<AdminSession>, StoreError> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM admin_sessions WHERE id_hash = ?");
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
        sqlx::query("UPDATE admin_sessions SET last_seen = ? WHERE id_hash = ?")
            .bind(at)
            .bind(id_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_admin_session(&self, id_hash: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query("DELETE FROM admin_sessions WHERE id_hash = ?")
            .bind(id_hash)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    async fn delete_expired_admin_sessions(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(
            sqlx::query("DELETE FROM admin_sessions WHERE expires_at <= ?")
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
             before_value, after_value, request_id, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
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
        let mut qb = QueryBuilder::<Sqlite>::new(format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_events WHERE 1 = 1"
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
        Ok(sqlx::query("DELETE FROM audit_events WHERE at < ?")
            .bind(older_than)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    async fn add_draft_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError> {
        require_draft_status(&v)?;
        self.require_mock_key(mock_id).await?;
        self.insert_version(mock_id, v, Some(draft_meta)).await
    }

    async fn update_draft_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
        source: Vec<u8>,
        bundle: Bundle,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError> {
        let bundle = serde_json::to_string(&bundle)?;
        let meta = serde_json::to_string(&draft_meta)?;
        let digest = sha256_hex(&source);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if Self::version_status(&mut tx, mock_id, version).await? != ContractStatus::Draft {
            return Err(not_a_draft(version));
        }
        let sql = format!(
            "UPDATE contract_versions SET source = ?, source_digest = ?, bundle = ?, draft_meta = ? \
             WHERE mock_id = ? AND version = ? RETURNING {VERSION_COLUMNS}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(source)
            .bind(digest)
            .bind(bundle)
            .bind(meta)
            .bind(mock_id.to_string())
            .bind(version)
            .fetch_one(&mut *tx)
            .await?;
        let updated = version_from_row(&row)?;
        tx.commit().await?;
        Ok(updated)
    }

    async fn approve_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractVersion, StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if Self::version_status(&mut tx, mock_id, version).await? != ContractStatus::Draft {
            return Err(not_a_draft(version));
        }
        let sql = format!(
            "UPDATE contract_versions SET status = 'approved' \
             WHERE mock_id = ? AND version = ? RETURNING {VERSION_COLUMNS}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id.to_string())
            .bind(version)
            .fetch_one(&mut *tx)
            .await?;
        let approved = version_from_row(&row)?;
        tx.commit().await?;
        Ok(approved)
    }

    async fn create_import_job(&self, j: NewImportJob) -> Result<ImportJob, StoreError> {
        let progress = queued_progress();
        let progress_json = serde_json::to_string(&progress)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let key = sqlx::query("SELECT workspace, name FROM mocks WHERE id = ?")
            .bind(j.mock_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("mock {}", j.mock_id)))?;
        let at = now();
        let job = ImportJob {
            id: Uuid::new_v4(),
            workspace: key.try_get("workspace")?,
            mock_id: j.mock_id,
            mock: key.try_get("name")?,
            kind: j.kind,
            source_name: j.source_name,
            status: ImportJobStatus::Queued,
            progress,
            result_version: None,
            error: None,
            created_by: j.created_by,
            created_at: at,
            started_at: None,
            finished_at: None,
            updated_at: at,
        };
        sqlx::query(
            "INSERT INTO import_jobs (id, workspace, mock_id, mock, kind, source_name, input, status, \
             progress, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.to_string())
        .bind(&job.workspace)
        .bind(job.mock_id.to_string())
        .bind(&job.mock)
        .bind(&job.kind)
        .bind(&job.source_name)
        .bind(j.input)
        .bind(job.status.as_str())
        .bind(progress_json)
        .bind(&job.created_by)
        .bind(job.created_at)
        .bind(job.updated_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(job)
    }

    async fn get_import_job(&self, id: Uuid) -> Result<Option<ImportJob>, StoreError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM import_jobs WHERE id = ?");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(job_from_row)
            .transpose()
    }

    async fn import_job_input(&self, id: Uuid) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT input FROM import_jobs WHERE id = ? AND status IN ('queued', 'running')",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_import_jobs(
        &self,
        mock_id: Uuid,
        limit: u32,
    ) -> Result<Vec<ImportJob>, StoreError> {
        let sql = format!(
            "SELECT {JOB_COLUMNS} FROM import_jobs WHERE mock_id = ? \
             ORDER BY created_at DESC, id DESC LIMIT ?"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id.to_string())
            .bind(i64::from(limit.clamp(1, MAX_IMPORT_JOB_LIST)))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(job_from_row)
            .collect()
    }

    async fn claim_import_job(&self, now: DateTime<Utc>) -> Result<Option<ImportJob>, StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sql = format!(
            "UPDATE import_jobs SET status = 'running', started_at = ?, updated_at = ? \
             WHERE id = (SELECT id FROM import_jobs WHERE status = 'queued' \
             ORDER BY created_at, id LIMIT 1) RETURNING {JOB_COLUMNS}"
        );
        let job = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(now)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(job_from_row)
            .transpose()?;
        tx.commit().await?;
        Ok(job)
    }

    async fn update_import_progress(
        &self,
        id: Uuid,
        p: &ImportProgress,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE import_jobs SET progress = ?, updated_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(serde_json::to_string(p)?)
        .bind(now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    async fn finish_import_job(
        &self,
        id: Uuid,
        outcome: ImportOutcome,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let (status, version, error) = match outcome {
            ImportOutcome::Done { version } => (ImportJobStatus::Done, Some(version), None),
            ImportOutcome::Failed { error } => (ImportJobStatus::Failed, None, Some(error)),
        };
        Ok(sqlx::query(
            "UPDATE import_jobs SET status = ?, result_version = ?, error = ?, input = X'', \
             finished_at = ?, updated_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(status.as_str())
        .bind(version)
        .bind(error)
        .bind(now)
        .bind(now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    async fn complete_import_job(
        &self,
        id: Uuid,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Value,
        now: DateTime<Utc>,
    ) -> Result<Option<ContractVersion>, StoreError> {
        require_draft_status(&v)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let running: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM import_jobs WHERE id = ? AND mock_id = ? AND status = 'running'",
        )
        .bind(id.to_string())
        .bind(mock_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        if running.is_none() {
            return Ok(None);
        }
        let version = Self::insert_version_in(&mut tx, mock_id, v, Some(draft_meta)).await?;
        sqlx::query(
            "UPDATE import_jobs SET status = 'done', result_version = ?, error = NULL, \
             input = X'', finished_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(version.version)
        .bind(now)
        .bind(now)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(version))
    }

    async fn count_active_import_jobs(&self, mock_id: Uuid) -> Result<u64, StoreError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM import_jobs WHERE mock_id = ? AND status IN ('queued', 'running')",
        )
        .bind(mock_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn cancel_import_job(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE import_jobs SET status = 'canceled', input = X'', finished_at = ?, updated_at = ? \
             WHERE id = ? AND status IN ('queued', 'running')",
        )
        .bind(now)
        .bind(now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    async fn fail_stale_import_jobs(
        &self,
        updated_before: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "UPDATE import_jobs SET status = 'failed', error = ?, input = X'', finished_at = ?, \
             updated_at = ? WHERE status = 'running' AND updated_at < ?",
        )
        .bind(STALE_IMPORT_ERROR)
        .bind(now)
        .bind(now)
        .bind(updated_before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    async fn prune_import_jobs(&self, finished_before: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM import_jobs WHERE status IN ('done', 'failed', 'canceled') \
             AND finished_at < ?",
        )
        .bind(finished_before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    fn subscribe(&self) -> broadcast::Receiver<ControlChange> {
        self.changes.subscribe()
    }
}
