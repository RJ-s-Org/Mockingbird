//! Applies a [`FileConfig`] to the control store: file-declared profiles,
//! mocks, contracts and scenarios are created, updated or deleted to match
//! the file. API-owned resources are never touched.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, anyhow, bail};
use mb_mcm::Bundle;
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, LlmProfile, Mock, NewContractVersion, NewMock,
    ResourceSource, Scenario, ScenarioRules, validate_mock_name, validate_route_prefix,
};
use sha2::{Digest, Sha256};

use crate::file_config::{FileConfig, FileMock, FileProfile};

/// What a [`reconcile`] changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    /// `(mock, version)` of every contract version added and activated.
    pub new_versions: Vec<(String, i64)>,
    /// Problems the change may cause but that did not stop it (also logged
    /// at warn level), e.g. API mocks left using a deleted file profile.
    pub warnings: Vec<String>,
}

impl ReconcileReport {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.updated.is_empty()
            && self.deleted.is_empty()
            && self.new_versions.is_empty()
    }
}

/// A file mock with its contract read and compiled.
struct Prepared<'a> {
    name: &'a str,
    mock: &'a FileMock,
    prefix: String,
    source: Vec<u8>,
    digest: String,
    bundle: Bundle,
}

/// Makes the store match `cfg`.
///
/// The whole config is validated first (names, prefixes, profile
/// references, secret references, contracts and scenario operations, and
/// clashes with API-owned resources, route prefixes included); any error
/// returns `Err` with nothing applied.
pub async fn reconcile(
    store: &dyn ControlStore,
    cfg: &FileConfig,
) -> anyhow::Result<ReconcileReport> {
    let ws = DEFAULT_WORKSPACE;
    let existing_mocks: HashMap<String, Mock> = store
        .list_mocks(ws)
        .await
        .context("listing mocks")?
        .into_iter()
        .map(|m| (m.name.clone(), m))
        .collect();
    let existing_profiles: HashMap<String, LlmProfile> = store
        .list_profiles(ws)
        .await
        .context("listing LLM profiles")?
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();

    let prepared = validate(store, cfg, &existing_mocks, &existing_profiles).await?;

    let mut report = ReconcileReport::default();

    // Profiles first, so mocks never reference a missing one.
    for (name, fp) in &cfg.llm.profiles {
        let profile = to_profile(name, fp)?;
        if existing_profiles.get(name) != Some(&profile) {
            store
                .put_profile(&profile)
                .await
                .with_context(|| format!("saving LLM profile {name:?}"))?;
        }
    }

    for p in &prepared {
        apply_mock(store, p, existing_mocks.get(p.name), &mut report).await?;
    }

    for (name, mock) in &existing_mocks {
        if mock.source == ResourceSource::File && !cfg.mocks.contains_key(name) {
            store
                .delete_mock(ws, name)
                .await
                .with_context(|| format!("deleting mock {name:?}"))?;
            report.deleted.push(name.clone());
        }
    }
    report.deleted.sort();

    let removed: Vec<&String> = existing_profiles
        .iter()
        .filter(|(name, p)| {
            p.source == ResourceSource::File && !cfg.llm.profiles.contains_key(*name)
        })
        .map(|(name, _)| name)
        .collect();
    if !removed.is_empty() {
        // Mocks the API owns are not in the file, so nothing above stopped
        // them from using a profile the file no longer declares.
        let api_mocks: Vec<&Mock> = existing_mocks
            .values()
            .filter(|m| m.source == ResourceSource::Api)
            .collect();
        for name in removed {
            store
                .delete_profile(ws, name)
                .await
                .with_context(|| format!("deleting LLM profile {name:?}"))?;
            let mut users: Vec<&str> = api_mocks
                .iter()
                .filter(|m| uses_profile(m, name, &existing_profiles))
                .map(|m| m.name.as_str())
                .collect();
            users.sort_unstable();
            if !users.is_empty() {
                let warning = format!(
                    "LLM profile {name:?} was removed from the config file but API mocks still use it: {}",
                    users.join(", ")
                );
                tracing::warn!("{warning}");
                report.warnings.push(warning);
            }
        }
    }
    Ok(report)
}

/// Whether `mock`'s profile is `profile` or falls back to it.
fn uses_profile(mock: &Mock, profile: &str, profiles: &HashMap<String, LlmProfile>) -> bool {
    let mut seen = HashSet::new();
    let mut next = mock.llm_profile.as_deref();
    while let Some(name) = next {
        if name == profile {
            return true;
        }
        if !seen.insert(name) {
            return false;
        }
        next = profiles
            .get(name)
            .and_then(|p| p.fallback_profile.as_deref());
    }
    false
}

async fn validate<'a>(
    store: &dyn ControlStore,
    cfg: &'a FileConfig,
    existing_mocks: &HashMap<String, Mock>,
    existing_profiles: &HashMap<String, LlmProfile>,
) -> anyhow::Result<Vec<Prepared<'a>>> {
    // Profiles a file mock may use: the file's own and API-owned ones.
    let known_profile = |name: &str| {
        cfg.llm.profiles.contains_key(name)
            || existing_profiles
                .get(name)
                .is_some_and(|p| p.source == ResourceSource::Api)
    };

    for (name, p) in &cfg.llm.profiles {
        if !valid_identifier(name) {
            bail!("LLM profile name {name:?} must be 1-128 characters of [A-Za-z0-9._-]");
        }
        if existing_profiles
            .get(name)
            .is_some_and(|e| e.source == ResourceSource::Api)
        {
            bail!("LLM profile {name:?} is managed through the API; rename it in the config file");
        }
        if let Some(reference) = &p.api_key
            && !valid_secret_ref(reference)
        {
            bail!(
                "LLM profile {name:?}: api_key must be a secret reference (env:NAME or file:/path), not a key"
            );
        }
        if let Some(fallback) = &p.fallback
            && !known_profile(fallback)
        {
            bail!("LLM profile {name:?}: fallback profile {fallback:?} does not exist");
        }
        to_profile(name, p)?;
    }
    if let Some(name) = &cfg.llm.default_profile
        && !known_profile(name)
    {
        bail!("default LLM profile {name:?} does not exist");
    }

    let mut prefixes: BTreeMap<String, &str> = BTreeMap::new();
    let mut prepared = Vec::with_capacity(cfg.mocks.len());
    for (name, mock) in &cfg.mocks {
        validate_mock_name(name).map_err(|e| anyhow!("{e}"))?;
        let existing = existing_mocks.get(name);
        if existing.is_some_and(|m| m.source == ResourceSource::Api) {
            bail!("mock {name:?} is managed through the API; rename it in the config file");
        }
        let prefix = mock.effective_prefix(name);
        validate_route_prefix(&prefix).map_err(|e| anyhow!("mock {name:?}: {e}"))?;
        if let Some(other) = prefixes.insert(prefix.clone(), name) {
            bail!("mocks {other:?} and {name:?} share the route prefix {prefix:?}");
        }
        if let Some(profile) = &mock.llm_profile
            && !known_profile(profile)
        {
            bail!("mock {name:?}: LLM profile {profile:?} does not exist");
        }

        // File reads and compilation block: keep them off the runtime.
        let path = mock.contract.clone();
        let (source, bundle) = tokio::task::spawn_blocking(move || {
            let source = std::fs::read(&path)
                .with_context(|| format!("cannot read contract {}", path.display()))?;
            let bundle = mb_import::compile(&source)
                .with_context(|| format!("cannot compile contract {}", path.display()))?;
            anyhow::Ok((source, bundle))
        })
        .await
        .context("reading the contract did not finish")?
        .with_context(|| format!("mock {name:?}"))?;
        let operations: HashSet<&str> = bundle.operations.iter().map(|o| o.id.as_str()).collect();

        let api_scenarios: HashSet<String> = match existing {
            Some(m) => store
                .list_scenarios(m.id)
                .await
                .with_context(|| format!("listing the scenarios of mock {name:?}"))?
                .into_iter()
                .filter(|s| s.source == ResourceSource::Api)
                .map(|s| s.name)
                .collect(),
            None => HashSet::new(),
        };
        for (sname, s) in &mock.scenarios {
            if !valid_identifier(sname) {
                bail!(
                    "mock {name:?}: scenario name {sname:?} must be 1-128 characters of [A-Za-z0-9._-]"
                );
            }
            if api_scenarios.contains(sname) {
                bail!("mock {name:?}: scenario {sname:?} is managed through the API");
            }
            if let Some(status) = s.status
                && !(100..=599).contains(&status)
            {
                bail!("mock {name:?}, scenario {sname:?}: status {status} is not 100-599");
            }
            if let Some(set) = &s.set
                && !set.is_object()
            {
                bail!("mock {name:?}, scenario {sname:?}: set must be a mapping");
            }
            if let Some(op) = s
                .operations
                .iter()
                .find(|op| !operations.contains(op.as_str()))
            {
                bail!("mock {name:?}, scenario {sname:?}: operation {op:?} is not in the contract");
            }
            latency_ms(name, sname, s.latency)?;
        }

        prepared.push(Prepared {
            name,
            mock,
            prefix,
            digest: sha256_hex(&source),
            source,
            bundle,
        });
    }
    // A file mock may not take the prefix of a mock the API owns.
    for api_mock in existing_mocks
        .values()
        .filter(|m| m.source == ResourceSource::Api)
    {
        if let Some(name) = prefixes.get(&api_mock.route_prefix) {
            bail!(
                "mock {name:?}: route prefix {:?} is already used by mock {:?}, which is managed through the API",
                api_mock.route_prefix,
                api_mock.name
            );
        }
    }
    Ok(prepared)
}

async fn apply_mock(
    store: &dyn ControlStore,
    p: &Prepared<'_>,
    existing: Option<&Mock>,
    report: &mut ReconcileReport,
) -> anyhow::Result<()> {
    let name = p.name;
    let mut changed = false;
    let mock = match existing {
        None => {
            let mock = store
                .create_mock(
                    DEFAULT_WORKSPACE,
                    NewMock {
                        name: name.to_string(),
                        route_prefix: Some(p.prefix.clone()),
                        persona: p.mock.persona.clone(),
                        llm_profile: p.mock.llm_profile.clone(),
                        header_overrides: p.mock.header_overrides,
                        source: ResourceSource::File,
                    },
                )
                .await
                .with_context(|| format!("creating mock {name:?}"))?;
            report.created.push(name.to_string());
            mock
        }
        Some(current) => {
            let wanted = Mock {
                route_prefix: p.prefix.clone(),
                persona: p.mock.persona.clone(),
                llm_profile: p.mock.llm_profile.clone(),
                header_overrides: p.mock.header_overrides,
                ..current.clone()
            };
            if wanted != *current {
                changed = true;
                store
                    .update_mock(&wanted)
                    .await
                    .with_context(|| format!("updating mock {name:?}"))?
            } else {
                current.clone()
            }
        }
    };

    let active = store
        .active_contract(mock.id)
        .await
        .with_context(|| format!("loading the active contract of mock {name:?}"))?;
    if active.map(|v| v.source_digest) != Some(p.digest.clone()) {
        let version = store
            .add_contract_version(
                mock.id,
                NewContractVersion {
                    source: p.source.clone(),
                    bundle: p.bundle.clone(),
                    status: ContractStatus::Approved,
                    created_by: Some("file".into()),
                },
            )
            .await
            .with_context(|| format!("adding a contract version to mock {name:?}"))?;
        store
            .activate_contract_version(mock.id, version.version)
            .await
            .with_context(|| {
                format!(
                    "activating contract version {} of mock {name:?}",
                    version.version
                )
            })?;
        report
            .new_versions
            .push((name.to_string(), version.version));
        changed = true;
    }

    let current: HashMap<String, Scenario> = store
        .list_scenarios(mock.id)
        .await
        .with_context(|| format!("listing the scenarios of mock {name:?}"))?
        .into_iter()
        .filter(|s| s.source == ResourceSource::File)
        .map(|s| (s.name.clone(), s))
        .collect();
    for (sname, s) in &p.mock.scenarios {
        let scenario = Scenario {
            mock_id: mock.id,
            name: sname.clone(),
            instructions: s.instructions.clone(),
            rules: ScenarioRules {
                status: s.status,
                latency_ms: latency_ms(name, sname, s.latency)?,
                set: s.set.clone(),
                operations: s.operations.clone(),
            },
            source: ResourceSource::File,
        };
        if current.get(sname) != Some(&scenario) {
            store
                .put_scenario(&scenario)
                .await
                .with_context(|| format!("saving scenario {sname:?} of mock {name:?}"))?;
            changed = true;
        }
    }
    for sname in current.keys() {
        if !p.mock.scenarios.contains_key(sname) {
            store
                .delete_scenario(mock.id, sname)
                .await
                .with_context(|| format!("deleting scenario {sname:?} of mock {name:?}"))?;
            changed = true;
        }
    }

    if changed && existing.is_some() {
        report.updated.push(name.to_string());
    }
    Ok(())
}

fn to_profile(name: &str, p: &FileProfile) -> anyhow::Result<LlmProfile> {
    Ok(LlmProfile {
        workspace: DEFAULT_WORKSPACE.to_string(),
        name: name.to_string(),
        base_url: p.base_url.clone(),
        model: p.model.clone(),
        secret_ref: p.api_key.clone(),
        json_schema: p.json_schema,
        temperature: p.temperature,
        timeout_ms: u64::try_from(p.timeout.as_millis())
            .map_err(|_| anyhow!("LLM profile {name:?}: timeout is too long"))?,
        max_tokens: p.max_tokens,
        fallback_profile: p.fallback.clone(),
        source: ResourceSource::File,
    })
}

fn latency_ms(
    mock: &str,
    scenario: &str,
    latency: Option<std::time::Duration>,
) -> anyhow::Result<Option<u64>> {
    latency
        .map(|d| {
            u64::try_from(d.as_millis())
                .map_err(|_| anyhow!("mock {mock:?}, scenario {scenario:?}: latency is too long"))
        })
        .transpose()
}

/// Scenario and profile names: 1–128 chars of `[A-Za-z0-9._-]`.
fn valid_identifier(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `env:NAME` or `file:/path` with a non-empty name or path.
fn valid_secret_ref(reference: &str) -> bool {
    reference
        .strip_prefix("env:")
        .or_else(|| reference.strip_prefix("file:"))
        .is_some_and(|rest| !rest.is_empty())
}

/// Hex sha256, as the store computes `source_digest`.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
