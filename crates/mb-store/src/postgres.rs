//! Postgres [`ControlStore`]. Uuids are `UUID`, timestamps `TIMESTAMPTZ` and
//! JSON is `json` (bound as text and cast, read back as text).
//!
//! Change notifications are in-process only, like SQLite; cross-node
//! `LISTEN/NOTIFY` arrives with HA.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use tokio::sync::broadcast;
use uuid::Uuid;

use mb_mcm::Bundle;
use serde_json::Value;

use crate::{
    ACQUIRE_TIMEOUT, AdminSession, ApiKey, AuditEvent, AuditFilter, CHANGE_CHANNEL_CAPACITY,
    ChannelSettings, ContractStatus, ContractVersion, ControlChange, ControlStore,
    DEFAULT_WORKSPACE, ImportJob, ImportJobStatus, ImportOutcome, ImportProgress, LlmProfile,
    MAX_IMPORT_JOB_LIST, Mock, MockEventSettings, NewAuditEvent, NewContractVersion, NewImportJob,
    NewMock, NewRequestLogEntry, PruneCounter, RequestKind, RequestLogEntry, RequestLogFilter,
    ResourceSource, STALE_IMPORT_ERROR, Scenario, ScenarioRules, StoreError, Workspace,
    lease_times, missing_workspace, not_a_draft, now, queued_progress, require_draft_status,
    sha256_hex, validate_header_name, validate_hosts, validate_mock_name, validate_mock_settings,
    validate_route_prefix, validate_workspace_name,
};

/// Migrations table name, distinct from other crates' migrators that may share
/// the same database.
const MIGRATIONS_TABLE: &str = "_mb_store_migrations";

const MOCK_COLUMNS: &str = "id, workspace, name, route_prefix, persona, llm_profile, header_overrides, source, \
     data_plane_auth, created_at, updated_at, chaos, limits, hosts";
/// [`MOCK_COLUMNS`] with JSON read back as text.
const MOCK_SELECT: &str = "id, workspace, name, route_prefix, persona, llm_profile, header_overrides, source, \
     data_plane_auth::text AS data_plane_auth, created_at, updated_at, chaos::text AS chaos, \
     limits::text AS limits, hosts::text AS hosts";
const WORKSPACE_COLUMNS: &str = "name, display_name, created_at";
const VERSION_COLUMNS: &str = "id, mock_id, version, source_digest, source, bundle, status, created_by, created_at, \
     draft_meta";
/// [`VERSION_COLUMNS`] with JSON read back as text.
const VERSION_SELECT: &str = "id, mock_id, version, source_digest, source, bundle::text AS bundle, status, \
     created_by, created_at, draft_meta::text AS draft_meta";
/// Every `import_jobs` column but `input`, with JSON read back as text.
const JOB_SELECT: &str = "id, workspace, mock_id, mock, kind, source_name, status, progress::text AS progress, \
     result_version, error, created_by, created_at, started_at, finished_at, updated_at";
const PROFILE_COLUMNS: &str = "workspace, name, base_url, model, secret_ref, json_schema, temperature, timeout_ms, \
     max_tokens, fallback_profile, source, headers, extra_body";
/// [`PROFILE_COLUMNS`] with JSON read back as text.
const PROFILE_SELECT: &str = "workspace, name, base_url, model, secret_ref, json_schema, temperature, timeout_ms, \
     max_tokens, fallback_profile, source, headers::text AS headers, extra_body::text AS extra_body";
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
     prompt_tokens, completion_tokens, request::text AS request, response::text AS response, at, kind, fallback_reason";
const CHANNEL_COLUMNS: &str = "mock_id, channel_id, enabled, triggers, source, updated_at";
/// [`CHANNEL_COLUMNS`] with JSON read back as text.
const CHANNEL_SELECT: &str =
    "mock_id, channel_id, enabled, triggers::text AS triggers, source, updated_at";
const EVENT_SETTINGS_COLUMNS: &str = "mock_id, broker, source, updated_at";

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
    let chaos: Option<String> = row.try_get("chaos")?;
    let limits: String = row.try_get("limits")?;
    let hosts: String = row.try_get("hosts")?;
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
        chaos: chaos.map(|c| serde_json::from_str(&c)).transpose()?,
        limits: serde_json::from_str(&limits)?,
        hosts: serde_json::from_str(&hosts)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn version_from_row(row: &PgRow) -> Result<ContractVersion, StoreError> {
    let bundle: String = row.try_get("bundle")?;
    let status: String = row.try_get("status")?;
    let draft_meta: Option<String> = row.try_get("draft_meta")?;
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
        draft_meta: draft_meta.map(|m| serde_json::from_str(&m)).transpose()?,
    })
}

/// Inserts `v` as the mock's next version inside `tx`, demoting the active
/// version when `v` is `Active`. The caller holds the mock's
/// [`LOCK_FOR_VERSIONS`] lock.
async fn insert_version(
    tx: &mut Transaction<'_, Postgres>,
    mock_id: Uuid,
    v: NewContractVersion,
    draft_meta: Option<Value>,
) -> Result<ContractVersion, StoreError> {
    let bundle = serde_json::to_string(&v.bundle)?;
    let meta = draft_meta.as_ref().map(serde_json::to_string).transpose()?;
    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM contract_versions WHERE mock_id = $1",
    )
    .bind(mock_id)
    .fetch_one(&mut **tx)
    .await?;
    if v.status == ContractStatus::Active {
        sqlx::query(
            "UPDATE contract_versions SET status = 'approved' WHERE mock_id = $1 AND status = 'active'",
        )
        .bind(mock_id)
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
        "INSERT INTO contract_versions ({VERSION_COLUMNS}) \
         VALUES ($1, $2, $3, $4, $5, $6::json, $7, $8, $9, $10::json)"
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
        .bind(meta)
        .execute(&mut **tx)
        .await?;
    Ok(version)
}

/// The version's status inside `tx`; `NotFound` if it does not exist.
async fn version_status(
    tx: &mut Transaction<'_, Postgres>,
    mock_id: Uuid,
    version: i64,
) -> Result<ContractStatus, StoreError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM contract_versions WHERE mock_id = $1 AND version = $2",
    )
    .bind(mock_id)
    .bind(version)
    .fetch_optional(&mut **tx)
    .await?;
    let status = status.ok_or_else(|| {
        StoreError::NotFound(format!("contract version {version} of mock {mock_id}"))
    })?;
    ContractStatus::parse(&status)
}

fn job_from_row(row: &PgRow) -> Result<ImportJob, StoreError> {
    let status: String = row.try_get("status")?;
    let progress: String = row.try_get("progress")?;
    Ok(ImportJob {
        id: row.try_get("id")?,
        workspace: row.try_get("workspace")?,
        mock_id: row.try_get("mock_id")?,
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
    let headers: String = row.try_get("headers")?;
    let extra_body: String = row.try_get("extra_body")?;
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
        headers: serde_json::from_str(&headers)?,
        extra_body: serde_json::from_str(&extra_body)?,
        source: ResourceSource::parse(&source)?,
    })
}

fn log_from_row(row: &PgRow) -> Result<RequestLogEntry, StoreError> {
    let request: String = row.try_get("request")?;
    let response: Option<String> = row.try_get("response")?;
    let status: i32 = row.try_get("status")?;
    let kind: String = row.try_get("kind")?;
    Ok(RequestLogEntry {
        id: row.try_get("id")?,
        mock_id: row.try_get("mock_id")?,
        operation_id: row.try_get("operation_id")?,
        session: row.try_get("session")?,
        method: row.try_get("method")?,
        path: row.try_get("path")?,
        status: from_i64(i64::from(status), "status")?,
        source: row.try_get("source")?,
        fallback_reason: row.try_get("fallback_reason")?,
        latency_ms: from_i64(row.try_get("latency_ms")?, "latency_ms")?,
        prompt_tokens: from_i64(row.try_get("prompt_tokens")?, "prompt_tokens")?,
        completion_tokens: from_i64(row.try_get("completion_tokens")?, "completion_tokens")?,
        request: serde_json::from_str(&request)?,
        response: response.map(|r| serde_json::from_str(&r)).transpose()?,
        at: row.try_get("at")?,
        kind: RequestKind::parse(&kind)
            .ok_or_else(|| StoreError::Db(format!("unknown request kind {kind:?}")))?,
    })
}

fn channel_settings_from_row(row: &PgRow) -> Result<ChannelSettings, StoreError> {
    let triggers: Option<String> = row.try_get("triggers")?;
    let source: String = row.try_get("source")?;
    Ok(ChannelSettings {
        mock_id: row.try_get("mock_id")?,
        channel_id: row.try_get("channel_id")?,
        enabled: row.try_get("enabled")?,
        triggers: triggers.map(|t| serde_json::from_str(&t)).transpose()?,
        source: ResourceSource::parse(&source)?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn event_settings_from_row(row: &PgRow) -> Result<MockEventSettings, StoreError> {
    let source: String = row.try_get("source")?;
    Ok(MockEventSettings {
        mock_id: row.try_get("mock_id")?,
        broker: row.try_get("broker")?,
        source: ResourceSource::parse(&source)?,
        updated_at: row.try_get("updated_at")?,
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
        validate_hosts(&m.hosts)?;
        validate_header_name(&m.data_plane_auth.header)?;
        validate_mock_settings(m.chaos.as_ref(), &m.limits)?;
        let data_plane_auth = serde_json::to_string(&m.data_plane_auth)?;
        let chaos = m.chaos.as_ref().map(serde_json::to_string).transpose()?;
        let limits = serde_json::to_string(&m.limits)?;
        let hosts = serde_json::to_string(&m.hosts)?;
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
            chaos: m.chaos,
            limits: m.limits,
            hosts: m.hosts,
            created_at: at,
            updated_at: at,
        };
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, ws).await?;
        let sql = format!(
            "INSERT INTO mocks ({MOCK_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::json, $10, $11, $12::jsonb, $13::jsonb, $14::jsonb)"
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
            .bind(chaos)
            .bind(limits)
            .bind(hosts)
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
        validate_hosts(&m.hosts)?;
        validate_header_name(&m.data_plane_auth.header)?;
        validate_mock_settings(m.chaos.as_ref(), &m.limits)?;
        let data_plane_auth = serde_json::to_string(&m.data_plane_auth)?;
        let chaos = m.chaos.as_ref().map(serde_json::to_string).transpose()?;
        let limits = serde_json::to_string(&m.limits)?;
        let hosts = serde_json::to_string(&m.hosts)?;
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
             header_overrides = $5, source = $6, data_plane_auth = $7::json, updated_at = $8, \
             chaos = $10::jsonb, limits = $11::jsonb, hosts = $12::jsonb WHERE id = $9",
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
        .bind(chaos)
        .bind(limits)
        .bind(hosts)
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
        let mut tx = self.pool.begin().await?;
        let (ws, name) = require_locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS).await?;
        let version = insert_version(&mut tx, mock_id, v, None).await?;
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
        if let Some(chaos) = &s.rules.chaos {
            chaos.validate().map_err(StoreError::Invalid)?;
        }
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
            "SELECT {PROFILE_SELECT} FROM llm_profiles WHERE workspace = $1 ORDER BY name COLLATE \"C\""
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
        let sql =
            format!("SELECT {PROFILE_SELECT} FROM llm_profiles WHERE workspace = $1 AND name = $2");
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb, $13::jsonb) \
             ON CONFLICT (workspace, name) DO UPDATE SET base_url = excluded.base_url, model = excluded.model, \
             secret_ref = excluded.secret_ref, json_schema = excluded.json_schema, \
             temperature = excluded.temperature, timeout_ms = excluded.timeout_ms, \
             max_tokens = excluded.max_tokens, fallback_profile = excluded.fallback_profile, \
             source = excluded.source, headers = excluded.headers, extra_body = excluded.extra_body"
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
            .bind(serde_json::to_string(&p.headers)?)
            .bind(serde_json::to_string(&p.extra_body)?)
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
             latency_ms, prompt_tokens, completion_tokens, request, response, at, kind, fallback_reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::json, $12::json, $13, $14, $15) RETURNING id",
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
        .bind(e.kind.as_str())
        .bind(&e.fallback_reason)
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
        if let Some(kind) = f.kind {
            qb.push(" AND kind = ").push_bind(kind.as_str());
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

    async fn get_request_log(&self, id: i64) -> Result<Option<RequestLogEntry>, StoreError> {
        let sql = format!("SELECT {LOG_SELECT} FROM request_log WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(log_from_row)
            .transpose()
    }

    async fn add_token_usage(
        &self,
        mock_id: Uuid,
        day: chrono::NaiveDate,
        tokens: u64,
    ) -> Result<u64, StoreError> {
        let total: i64 = sqlx::query_scalar(
            "INSERT INTO token_usage (mock_id, day, tokens) VALUES ($1, $2, $3) \
             ON CONFLICT (mock_id, day) DO UPDATE SET tokens = token_usage.tokens + excluded.tokens \
             RETURNING tokens",
        )
        .bind(mock_id)
        .bind(day)
        .bind(to_i64(tokens, "tokens")?)
        .fetch_one(&self.pool)
        .await?;
        from_i64(total, "tokens")
    }

    async fn token_usage(&self, mock_id: Uuid, day: chrono::NaiveDate) -> Result<u64, StoreError> {
        let total: Option<i64> =
            sqlx::query_scalar("SELECT tokens FROM token_usage WHERE mock_id = $1 AND day = $2")
                .bind(mock_id)
                .bind(day)
                .fetch_optional(&self.pool)
                .await?;
        total.map_or(Ok(0), |t| from_i64(t, "tokens"))
    }

    async fn prune_token_usage(&self, before: chrono::NaiveDate) -> Result<u64, StoreError> {
        Ok(sqlx::query("DELETE FROM token_usage WHERE day < $1")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    async fn list_channel_settings(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ChannelSettings>, StoreError> {
        let sql = format!(
            "SELECT {CHANNEL_SELECT} FROM channel_settings WHERE mock_id = $1 \
             ORDER BY channel_id COLLATE \"C\""
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(channel_settings_from_row)
            .collect()
    }

    async fn put_channel_settings(&self, s: &ChannelSettings) -> Result<(), StoreError> {
        let triggers = s.triggers.as_ref().map(serde_json::to_string).transpose()?;
        let mut tx = self.pool.begin().await?;
        let (ws, name) = require_locked_mock_key(&mut tx, s.mock_id, LOCK_KEEP).await?;
        let sql = format!(
            "INSERT INTO channel_settings ({CHANNEL_COLUMNS}) VALUES ($1, $2, $3, $4::json, $5, $6) \
             ON CONFLICT (mock_id, channel_id) DO UPDATE SET enabled = excluded.enabled, \
             triggers = excluded.triggers, source = excluded.source, updated_at = excluded.updated_at"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(s.mock_id)
            .bind(&s.channel_id)
            .bind(s.enabled)
            .bind(triggers)
            .bind(s.source.as_str())
            .bind(s.updated_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.publish_mock(&ws, &name);
        Ok(())
    }

    async fn delete_channel_settings(
        &self,
        mock_id: Uuid,
        channel_id: &str,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let key = locked_mock_key(&mut tx, mock_id, LOCK_KEEP).await?;
        let deleted =
            sqlx::query("DELETE FROM channel_settings WHERE mock_id = $1 AND channel_id = $2")
                .bind(mock_id)
                .bind(channel_id)
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

    async fn get_event_settings(
        &self,
        mock_id: Uuid,
    ) -> Result<Option<MockEventSettings>, StoreError> {
        let sql =
            format!("SELECT {EVENT_SETTINGS_COLUMNS} FROM mock_event_settings WHERE mock_id = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(event_settings_from_row)
            .transpose()
    }

    async fn put_event_settings(&self, s: &MockEventSettings) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let (ws, name) = require_locked_mock_key(&mut tx, s.mock_id, LOCK_KEEP).await?;
        let sql = format!(
            "INSERT INTO mock_event_settings ({EVENT_SETTINGS_COLUMNS}) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (mock_id) DO UPDATE SET broker = excluded.broker, \
             source = excluded.source, updated_at = excluded.updated_at"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(s.mock_id)
            .bind(&s.broker)
            .bind(s.source.as_str())
            .bind(s.updated_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.publish_mock(&ws, &name);
        Ok(())
    }

    async fn acquire_scheduler_lease(
        &self,
        mock_id: Uuid,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        let (expires, at) = lease_times(ttl)?;
        let row = sqlx::query(
            "INSERT INTO scheduler_leases (mock_id, holder, expires_at) VALUES ($1, $2, $3) \
             ON CONFLICT (mock_id) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at \
             WHERE scheduler_leases.holder = excluded.holder OR scheduler_leases.expires_at < $4 \
             RETURNING holder",
        )
        .bind(mock_id)
        .bind(holder)
        .bind(expires)
        .bind(at)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
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

    async fn add_draft_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError> {
        require_draft_status(&v)?;
        let mut tx = self.pool.begin().await?;
        require_locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS).await?;
        let version = insert_version(&mut tx, mock_id, v, Some(draft_meta)).await?;
        tx.commit().await?;
        Ok(version)
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
        let mut tx = self.pool.begin().await?;
        require_locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS).await?;
        if version_status(&mut tx, mock_id, version).await? != ContractStatus::Draft {
            return Err(not_a_draft(version));
        }
        let sql = format!(
            "UPDATE contract_versions SET source = $1, source_digest = $2, bundle = $3::json, \
             draft_meta = $4::json WHERE mock_id = $5 AND version = $6 RETURNING {VERSION_SELECT}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(source)
            .bind(digest)
            .bind(bundle)
            .bind(meta)
            .bind(mock_id)
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
        let mut tx = self.pool.begin().await?;
        require_locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS).await?;
        if version_status(&mut tx, mock_id, version).await? != ContractStatus::Draft {
            return Err(not_a_draft(version));
        }
        let sql = format!(
            "UPDATE contract_versions SET status = 'approved' \
             WHERE mock_id = $1 AND version = $2 RETURNING {VERSION_SELECT}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
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
        let mut tx = self.pool.begin().await?;
        let (workspace, mock) = require_locked_mock_key(&mut tx, j.mock_id, LOCK_KEEP).await?;
        let at = now();
        let job = ImportJob {
            id: Uuid::new_v4(),
            workspace,
            mock_id: j.mock_id,
            mock,
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
             progress, created_by, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::json, $10, $11, $12)",
        )
        .bind(job.id)
        .bind(&job.workspace)
        .bind(job.mock_id)
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
        let sql = format!("SELECT {JOB_SELECT} FROM import_jobs WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(job_from_row)
            .transpose()
    }

    async fn import_job_input(&self, id: Uuid) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT input FROM import_jobs WHERE id = $1 AND status IN ('queued', 'running')",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_import_jobs(
        &self,
        mock_id: Uuid,
        limit: u32,
    ) -> Result<Vec<ImportJob>, StoreError> {
        let sql = format!(
            "SELECT {JOB_SELECT} FROM import_jobs WHERE mock_id = $1 \
             ORDER BY created_at DESC, id DESC LIMIT $2"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock_id)
            .bind(i64::from(limit.clamp(1, MAX_IMPORT_JOB_LIST)))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(job_from_row)
            .collect()
    }

    async fn claim_import_job(&self, now: DateTime<Utc>) -> Result<Option<ImportJob>, StoreError> {
        // `SKIP LOCKED` lets concurrent claimers pass over a row another
        // transaction is claiming; the outer status check re-validates it.
        let sql = format!(
            "UPDATE import_jobs SET status = 'running', started_at = $1, updated_at = $1 \
             WHERE status = 'queued' AND id = (SELECT id FROM import_jobs WHERE status = 'queued' \
             ORDER BY created_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING {JOB_SELECT}"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(now)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(job_from_row)
            .transpose()
    }

    async fn update_import_progress(
        &self,
        id: Uuid,
        p: &ImportProgress,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE import_jobs SET progress = $1::json, updated_at = $2 \
             WHERE id = $3 AND status = 'running'",
        )
        .bind(serde_json::to_string(p)?)
        .bind(now)
        .bind(id)
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
            "UPDATE import_jobs SET status = $1, result_version = $2, error = $3, input = ''::bytea, \
             finished_at = $4, updated_at = $4 WHERE id = $5 AND status = 'running'",
        )
        .bind(status.as_str())
        .bind(version)
        .bind(error)
        .bind(now)
        .bind(id)
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
        let mut tx = self.pool.begin().await?;
        // The mock first, then the job: the order a mock delete locks them.
        if locked_mock_key(&mut tx, mock_id, LOCK_FOR_VERSIONS)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let running: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM import_jobs WHERE id = $1 AND mock_id = $2 AND status = 'running' \
             FOR UPDATE",
        )
        .bind(id)
        .bind(mock_id)
        .fetch_optional(&mut *tx)
        .await?;
        if running.is_none() {
            return Ok(None);
        }
        let version = insert_version(&mut tx, mock_id, v, Some(draft_meta)).await?;
        sqlx::query(
            "UPDATE import_jobs SET status = 'done', result_version = $1, error = NULL, \
             input = ''::bytea, finished_at = $2, updated_at = $2 WHERE id = $3",
        )
        .bind(version.version)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(version))
    }

    async fn count_active_import_jobs(&self, mock_id: Uuid) -> Result<u64, StoreError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM import_jobs WHERE mock_id = $1 AND status IN ('queued', 'running')",
        )
        .bind(mock_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn cancel_import_job(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE import_jobs SET status = 'canceled', input = ''::bytea, finished_at = $1, \
             updated_at = $1 WHERE id = $2 AND status IN ('queued', 'running')",
        )
        .bind(now)
        .bind(id)
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
            "UPDATE import_jobs SET status = 'failed', error = $1, input = ''::bytea, \
             finished_at = $2, updated_at = $2 WHERE status = 'running' AND updated_at < $3",
        )
        .bind(STALE_IMPORT_ERROR)
        .bind(now)
        .bind(updated_before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    async fn prune_import_jobs(&self, finished_before: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM import_jobs WHERE status IN ('done', 'failed', 'canceled') \
             AND finished_at < $1",
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
