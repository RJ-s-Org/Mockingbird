//! Per-call LLM profile choice: the mock's [`ProviderPolicy`] runs for every
//! admitted call with the call's tenancy, and a call whose policy picks a
//! profile other than the configured one generates with that profile's
//! engine, built on first use and kept for the life of the runtime.
//!
//! [`ProviderPolicy`]: mb_ext::ProviderPolicy

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use mb_engine::Engine;
use mb_ext::{Rejection, Tenancy};

use crate::MockRuntime;

/// Builds a mock's engine for an LLM profile its provider policy picks
/// instead of the configured one ([`MockRuntime::configured_profile`]).
/// The binary implements it on top of its profile chain; the engine has the
/// mock's persona, stream settings and guards, like the configured one.
#[async_trait]
pub trait ProfileEngines: Send + Sync {
    /// The engine generating with `profile`, or without an LLM for `None`.
    ///
    /// # Errors
    ///
    /// A message (never a secret) when the profile cannot be loaded or its
    /// provider chain cannot be built; the call is then answered `503`.
    async fn engine(&self, profile: Option<&str>) -> Result<Arc<Engine>, String>;
}

/// The engines [`ProfileEngines`] built for one runtime, by profile. Bounded
/// by the profiles the policy picks that exist (a failed build is not kept),
/// and dropped with the runtime, which is rebuilt whenever the mock or a
/// profile changes.
#[derive(Default)]
pub(crate) struct EngineCache(tokio::sync::Mutex<HashMap<Option<String>, Arc<Engine>>>);

impl std::fmt::Debug for EngineCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let built = self.0.try_lock().map(|m| m.len()).ok();
        f.debug_struct("EngineCache")
            .field("built", &built)
            .finish()
    }
}

/// Title of the problem answering a call whose chosen profile cannot be built.
const UNAVAILABLE: &str = "LLM profile unavailable";

impl MockRuntime {
    /// The engine a call admitted as `tenancy` generates with: the provider
    /// policy's choice for [`MockRuntime::configured_profile`]. The
    /// configured profile itself (always so under the self-hosted
    /// `ConfiguredProfile` policy) uses [`MockRuntime::engine`]; another
    /// profile uses the engine [`MockRuntime::profile_engines`] builds for
    /// it, once per runtime. Without `profile_engines` every call uses
    /// [`MockRuntime::engine`].
    ///
    /// # Errors
    ///
    /// The policy's [`Rejection`], or a `503` when the chosen profile's
    /// engine cannot be built.
    pub async fn engine_for(&self, tenancy: &Tenancy) -> Result<Arc<Engine>, Rejection> {
        let configured = self.configured_profile.as_deref();
        let chosen = self.ext.providers.choose(tenancy, &self.name, configured)?;
        if chosen.as_deref() == configured {
            return Ok(Arc::clone(&self.engine));
        }
        let Some(builder) = &self.profile_engines else {
            return Ok(Arc::clone(&self.engine));
        };
        let mut built = self.profile_cache.0.lock().await;
        if let Some(engine) = built.get(&chosen) {
            return Ok(Arc::clone(engine));
        }
        match builder.engine(chosen.as_deref()).await {
            Ok(engine) => {
                built.insert(chosen, Arc::clone(&engine));
                Ok(engine)
            }
            Err(error) => {
                tracing::warn!(
                    mock = %self.name,
                    profile = chosen.as_deref().unwrap_or(""),
                    %error,
                    "cannot build the LLM profile the provider policy chose"
                );
                Err(Rejection::new(
                    503,
                    UNAVAILABLE,
                    format!(
                        "the LLM profile {:?} chosen for this call is unavailable",
                        chosen.as_deref().unwrap_or("")
                    ),
                ))
            }
        }
    }
}
