#![cfg(test)]
//! The provider policy runs per call with the call's tenancy (D1): its
//! choice routes the call to that profile's engine, its rejection is the
//! call's problem, and the self-hosted `ConfiguredProfile` never builds an
//! engine besides the mock's own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig, Source};
use mb_ext::{CallInfo, ProviderPolicy, Rejection, Tenancy, TenantResolver};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::{MockRuntime, ProfileEngines, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

fn orders() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

/// An engine whose LLM answers every order with `customer` as its
/// `customerId`.
fn engine_answering(customer: &str) -> Arc<Engine> {
    let answers = (0..8)
        .map(|_| {
            Ok(
                json!({"id": "x", "customerId": customer, "status": "pending", "total": 10})
                    .to_string(),
            )
        })
        .collect();
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(answers));
    Arc::new(Engine::new(Some(provider), EngineConfig::default()))
}

/// The tenant named by the `x-tenant` header, else `default`.
struct HeaderTenant;

#[async_trait]
impl TenantResolver for HeaderTenant {
    async fn resolve(&self, call: &CallInfo<'_>) -> Result<Tenancy, Rejection> {
        let tenant = call
            .headers
            .get("x-tenant")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("default");
        Ok(Tenancy::new(tenant, "default"))
    }
}

/// Tenant `acme` gets profile `gold`; every other tenant the configured one.
struct GoldForAcme;

impl ProviderPolicy for GoldForAcme {
    fn choose(
        &self,
        tenancy: &Tenancy,
        _mock: &str,
        configured: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        Ok(match tenancy.tenant.as_str() {
            "acme" => Some("gold".into()),
            _ => configured.map(str::to_string),
        })
    }
}

/// Refuses every call with 402 and `Retry-After: 30`.
struct NoPlan;

impl ProviderPolicy for NoPlan {
    fn choose(
        &self,
        _tenancy: &Tenancy,
        _mock: &str,
        _configured: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        Err(
            Rejection::new(402, "Payment Required", "the plan has no LLM calls left")
                .with_retry_after(Duration::from_secs(30)),
        )
    }
}

/// Builds an engine answering with the profile's name, counting the builds
/// and recording the profiles asked for.
#[derive(Default)]
struct Counting {
    built: AtomicUsize,
    asked: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl ProfileEngines for Counting {
    async fn engine(&self, profile: Option<&str>) -> Result<Arc<Engine>, String> {
        self.built.fetch_add(1, Ordering::SeqCst);
        self.asked.lock().unwrap().push(profile.map(str::to_string));
        Ok(engine_answering(profile.unwrap_or("none")))
    }
}

/// An `orders` runtime configured with profile `base`, whose engine answers
/// with customer `base`, other profiles' engines built by `engines`.
fn runtime(policy: Arc<dyn ProviderPolicy>, engines: Arc<Counting>) -> MockRuntime {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new("orders", orders(), engine_answering("base"), state);
    rt.configured_profile = Some("base".into());
    rt.profile_engines = Some(engines);
    rt.ext.tenants = Arc::new(HeaderTenant);
    rt.ext.providers = policy;
    rt
}

async fn get(router: &Router, uri: &str, tenant: Option<&str>) -> Response {
    let mut req = Request::get(uri);
    if let Some(t) = tenant {
        req = req.header("x-tenant", t);
    }
    router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn customer(router: &Router, uri: &str, tenant: Option<&str>) -> Value {
    let resp = get(router, uri, tenant).await;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    // The LLM's answer (its `id` patched to the path's).
    assert_eq!(
        resp.headers()["x-mockingbird-source"],
        Source::Patched.as_str(),
        "{uri}"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["customerId"].clone()
}

#[tokio::test]
async fn each_tenant_is_routed_to_the_profile_its_policy_picks() {
    let engines = Arc::new(Counting::default());
    let rt = runtime(Arc::new(GoldForAcme), engines.clone());
    let router = http_rest::router(Arc::new(rt));

    assert_eq!(customer(&router, "/orders/o1", Some("acme")).await, "gold");
    assert_eq!(customer(&router, "/orders/o2", None).await, "base");
    assert_eq!(customer(&router, "/orders/o3", Some("beta")).await, "base");
    assert_eq!(customer(&router, "/orders/o4", Some("acme")).await, "gold");

    // Built once, on first use, and only for the profile that differs.
    assert_eq!(engines.built.load(Ordering::SeqCst), 1);
    assert_eq!(
        engines.asked.lock().unwrap().clone(),
        vec![Some("gold".to_string())]
    );
}

#[tokio::test]
async fn a_rejecting_policy_is_answered_as_a_problem_with_retry_after() {
    let engines = Arc::new(Counting::default());
    let rt = runtime(Arc::new(NoPlan), engines.clone());
    let router = http_rest::router(Arc::new(rt));

    let resp = get(&router, "/orders/o1", Some("acme")).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(resp.headers()["retry-after"], "30");
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["title"], "Payment Required");
    assert_eq!(body["detail"], "the plan has no LLM calls left");
    assert_eq!(engines.built.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_configured_profile_policy_never_builds_another_engine() {
    let engines = Arc::new(Counting::default());
    let rt = runtime(Arc::new(mb_ext::ConfiguredProfile), engines.clone());
    let router = http_rest::router(Arc::new(rt));

    for (i, tenant) in [Some("acme"), None, Some("beta"), Some("acme")]
        .into_iter()
        .enumerate()
    {
        let uri = format!("/orders/o{i}");
        assert_eq!(customer(&router, &uri, tenant).await, "base");
    }
    assert_eq!(engines.built.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unbuildable_profile_is_a_503_problem() {
    struct Failing;

    #[async_trait]
    impl ProfileEngines for Failing {
        async fn engine(&self, _profile: Option<&str>) -> Result<Arc<Engine>, String> {
            Err("LLM profile \"gold\" not found".into())
        }
    }

    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new("orders", orders(), engine_answering("base"), state);
    rt.configured_profile = Some("base".into());
    rt.profile_engines = Some(Arc::new(Failing));
    rt.ext.tenants = Arc::new(HeaderTenant);
    rt.ext.providers = Arc::new(GoldForAcme);
    let router = http_rest::router(Arc::new(rt));

    let resp = get(&router, "/orders/o1", Some("acme")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
    // Other tenants keep being served.
    assert_eq!(customer(&router, "/orders/o2", None).await, "base");
}
