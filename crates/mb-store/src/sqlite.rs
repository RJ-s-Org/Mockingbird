//! SQLite [`ControlStore`]. Uuids, timestamps and JSON are stored as TEXT.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    CHANGE_CHANNEL_CAPACITY, ContractStatus, ContractVersion, ControlChange, ControlStore,
    LlmProfile, Mock, NewContractVersion, NewMock, NewRequestLogEntry, RETENTION_PRUNE_INTERVAL,
    RequestLogEntry, RequestLogFilter, ResourceSource, Scenario, ScenarioRules, StoreError, now,
    sha256_hex, validate_mock_name, validate_route_prefix,
};

/// Migrations table name, distinct from other crates' migrators that may share
/// the same database.
const MIGRATIONS_TABLE: &str = "_mb_store_migrations";

const MOCK_COLUMNS: &str = "id, workspace, name, route_prefix, persona, llm_profile, header_overrides, source, created_at, updated_at";
const VERSION_COLUMNS: &str =
    "id, mock_id, version, source_digest, source, bundle, status, created_by, created_at";
const PROFILE_COLUMNS: &str = "workspace, name, base_url, model, secret_ref, json_schema, temperature, timeout_ms, \
     max_tokens, fallback_profile, source";
const LOG_COLUMNS: &str = "id, mock_id, operation_id, session, method, path, status, source, latency_ms, \
     prompt_tokens, completion_tokens, request, response, at";

pub struct SqliteControlStore {
    pool: SqlitePool,
    changes: broadcast::Sender<ControlChange>,
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
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(options)
                .await?
        } else {
            SqlitePoolOptions::new()
                .connect_with(options.journal_mode(SqliteJournalMode::Wal))
                .await?
        };
        let mut migrator = sqlx::migrate!("./migrations/sqlite");
        migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
        migrator.run(&pool).await?;
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Ok(Self { pool, changes })
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
    Ok(Mock {
        id: parse_uuid(&id)?,
        workspace: row.try_get("workspace")?,
        name: row.try_get("name")?,
        route_prefix: row.try_get("route_prefix")?,
        persona: row.try_get("persona")?,
        llm_profile: row.try_get("llm_profile")?,
        header_overrides: row.try_get("header_overrides")?,
        source: ResourceSource::parse(&source)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn version_from_row(row: &SqliteRow) -> Result<ContractVersion, StoreError> {
    let id: String = row.try_get("id")?;
    let mock_id: String = row.try_get("mock_id")?;
    let bundle: String = row.try_get("bundle")?;
    let status: String = row.try_get("status")?;
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
            created_at: at,
            updated_at: at,
        };
        let sql =
            format!("INSERT INTO mocks ({MOCK_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(mock.id.to_string())
            .bind(&mock.workspace)
            .bind(&mock.name)
            .bind(&mock.route_prefix)
            .bind(&mock.persona)
            .bind(&mock.llm_profile)
            .bind(mock.header_overrides)
            .bind(mock.source.as_str())
            .bind(mock.created_at)
            .bind(mock.updated_at)
            .execute(&self.pool)
            .await
            .map_err(|e| match StoreError::from(e) {
                StoreError::Conflict(_) => {
                    StoreError::Conflict(format!("mock {:?} already exists", mock.name))
                }
                other => other,
            })?;
        self.publish_mock(&mock.workspace, &mock.name);
        Ok(mock)
    }

    async fn update_mock(&self, m: &Mock) -> Result<Mock, StoreError> {
        validate_mock_name(&m.name)?;
        validate_route_prefix(&m.route_prefix)?;
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
             source = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&updated.name)
        .bind(&updated.route_prefix)
        .bind(&updated.persona)
        .bind(&updated.llm_profile)
        .bind(updated.header_overrides)
        .bind(updated.source.as_str())
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
        let bundle = serde_json::to_string(&v.bundle)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM contract_versions WHERE mock_id = ?",
        )
        .bind(mock_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if v.status == ContractStatus::Active {
            sqlx::query("UPDATE contract_versions SET status = 'approved' WHERE mock_id = ? AND status = 'active'")
                .bind(mock_id.to_string())
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
            "INSERT INTO contract_versions ({VERSION_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
            .execute(&self.pool)
            .await?;
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
        if id % RETENTION_PRUNE_INTERVAL == 0 {
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

    fn subscribe(&self) -> broadcast::Receiver<ControlChange> {
        self.changes.subscribe()
    }
}
