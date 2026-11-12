//! Whether a provider can be called at all: one whose key the provider
//! refuses for good (a bad key, no credit) is marked unusable for the rest of
//! the process, and one that rate limits is backed off for a while. Calls to
//! a provider in either state fall back at once, without contacting it, and
//! are counted.
//!
//! Each [`OpenAiCompatible`](crate::OpenAiCompatible) built for a profile
//! shares that profile's [`ProviderHealth`], taken from the server's
//! [`HealthRegistry`] by profile and by what decides whether the key works
//! (its base URL and key):
//! every mock of the profile sees the mark, and a reload that changes the key
//! or the base URL builds clients with a fresh, usable state.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::LlmError;

/// How long a rate-limited provider is backed off when its `429` names no
/// `Retry-After`.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

/// The longest backoff a `Retry-After` may ask for.
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// How the reason of an answer that fell back for a provider marked unusable
/// starts.
pub const UNUSABLE_REASON: &str = "the LLM provider is unusable for this run";

/// How the reason of an answer that fell back for a provider backing off
/// starts.
pub const BACKING_OFF_REASON: &str = "the LLM provider is rate limited";

/// Phrases of a `403` (or a quota `429`) body that say the key itself can no
/// longer be used, lowercased: OpenRouter's key limit and credit messages,
/// OpenAI's quota ones.
const KEY_UNUSABLE_PHRASES: [&str; 6] = [
    "key limit exceeded",
    "insufficient credit",
    "requires more credits",
    "insufficient_quota",
    "exceeded your current quota",
    "out of credits",
];

/// What a provider's error status says about its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The key can no longer be used: stop calling for the rest of the run.
    Unusable(String),
    /// Rate limited: back off this long.
    BackOff(Duration),
    /// An ordinary error.
    Ordinary,
}

/// What `status` with `body` (and its `Retry-After`, in seconds) says about
/// the provider's key: `401` and `402`, and a `403` or `429` whose body says
/// the key is out of credit or past its limit, make it unusable; any other
/// `429` backs it off for `Retry-After` (else [`DEFAULT_BACKOFF`], at most
/// [`MAX_BACKOFF`]). Other statuses, other `403`s included, are ordinary.
#[must_use]
pub fn classify(status: u16, body: &str, retry_after: Option<Duration>) -> Verdict {
    let lower = body.to_ascii_lowercase();
    let key_spent = KEY_UNUSABLE_PHRASES.iter().any(|p| lower.contains(p));
    match status {
        401 => Verdict::Unusable("the provider rejected the API key".into()),
        402 => Verdict::Unusable("the provider asks for payment: the key is out of credit".into()),
        403 | 429 if key_spent => {
            Verdict::Unusable("the key is past its limit or out of credit".into())
        }
        429 => Verdict::BackOff(retry_after.unwrap_or(DEFAULT_BACKOFF).min(MAX_BACKOFF)),
        _ => Verdict::Ordinary,
    }
}

/// A `Retry-After` header value in seconds; an HTTP date or anything else
/// is `None`.
#[must_use]
pub fn retry_after_secs(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[derive(Clone, Debug)]
enum State {
    Ok,
    Unusable {
        status: u16,
        reason: String,
    },
    BackingOff {
        until: Instant,
        until_wall: SystemTime,
        status: u16,
    },
}

/// The health of one profile's provider (see the [module docs](self)).
#[derive(Debug)]
pub struct ProviderHealth {
    profile: String,
    state: Mutex<State>,
    fallbacks: AtomicU64,
}

/// What [`ProviderHealth::snapshot`] reports.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HealthSnapshot {
    /// The profile, as `workspace/name`.
    pub profile: String,
    /// `ok`, `unusable` or `backing_off`.
    pub state: &'static str,
    /// The status that set the state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Why it is unusable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the backoff ends, as seconds since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_unix: Option<u64>,
    /// Calls that fell back without contacting the provider because of its
    /// state.
    pub fallbacks: u64,
}

impl ProviderHealth {
    /// A usable provider for `profile` (`workspace/name`), not shared: one
    /// shared by every client of the profile comes from a [`HealthRegistry`].
    #[must_use]
    pub fn new(profile: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            profile: profile.into(),
            state: Mutex::new(State::Ok),
            fallbacks: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Why a call now must not contact the provider, or `None` when it may;
    /// a backoff that has ended is cleared. Counts nothing.
    #[must_use]
    pub fn blocked(&self) -> Option<String> {
        let mut state = self.lock();
        match &*state {
            State::Ok => None,
            State::Unusable { status, reason } => Some(format!(
                "{UNUSABLE_REASON}: profile {} answered HTTP {status} ({reason})",
                self.profile
            )),
            State::BackingOff { until, status, .. } => {
                let now = Instant::now();
                if *until <= now {
                    *state = State::Ok;
                    return None;
                }
                Some(format!(
                    "{BACKING_OFF_REASON}: profile {} answered HTTP {status}; \
                     calls resume when its Retry-After has passed",
                    self.profile
                ))
            }
        }
    }

    /// [`Self::blocked`], counting the call as fallen back when it is.
    ///
    /// # Errors
    ///
    /// [`LlmError::Unavailable`] while the provider is unusable or backing
    /// off.
    pub fn admit(&self) -> Result<(), LlmError> {
        match self.blocked() {
            None => Ok(()),
            Some(reason) => {
                self.fallbacks.fetch_add(1, Ordering::Relaxed);
                Err(LlmError::Unavailable(reason))
            }
        }
    }

    /// Records an error status of the provider: one that says the key is
    /// unusable marks it so for good, a rate limit backs it off (each logged
    /// once, as a warning). Returns the [`LlmError::Unavailable`] the call
    /// answers with when the status changed the state, else `None` (an
    /// ordinary error).
    pub fn observe(
        &self,
        status: u16,
        body: &str,
        retry_after: Option<Duration>,
    ) -> Option<LlmError> {
        let verdict = classify(status, body, retry_after);
        let mut state = self.lock();
        match verdict {
            Verdict::Ordinary => return None,
            Verdict::Unusable(reason) => {
                if !matches!(*state, State::Unusable { .. }) {
                    tracing::warn!(
                        profile = %self.profile,
                        status,
                        reason = %reason,
                        body = %crate::openai::excerpt(body),
                        "LLM provider marked unusable for the rest of this run; \
                         its calls now fall back to local fill and the faker without calling it \
                         (change the profile's key or base_url to try it again)"
                    );
                    *state = State::Unusable { status, reason };
                }
            }
            Verdict::BackOff(wait) => {
                let until = Instant::now() + wait;
                match &mut *state {
                    State::Unusable { .. } => {}
                    State::BackingOff { until: held, .. } if *held >= until => {}
                    held => {
                        let fresh = matches!(held, State::Ok);
                        *held = State::BackingOff {
                            until,
                            until_wall: SystemTime::now() + wait,
                            status,
                        };
                        if fresh {
                            tracing::warn!(
                                profile = %self.profile,
                                status,
                                backoff_s = wait.as_secs(),
                                "LLM provider rate limited; its calls fall back to local fill \
                                 and the faker without calling it until the backoff ends"
                            );
                        }
                    }
                }
            }
        }
        drop(state);
        self.blocked().map(LlmError::Unavailable)
    }

    /// The provider's state and how many calls fell back because of it.
    #[must_use]
    pub fn snapshot(&self) -> HealthSnapshot {
        // Clears an ended backoff first.
        let _ = self.blocked();
        let fallbacks = self.fallbacks.load(Ordering::Relaxed);
        let mut snapshot = HealthSnapshot {
            profile: self.profile.clone(),
            state: "ok",
            status: None,
            reason: None,
            until_unix: None,
            fallbacks,
        };
        match &*self.lock() {
            State::Ok => {}
            State::Unusable { status, reason } => {
                snapshot.state = "unusable";
                snapshot.status = Some(*status);
                snapshot.reason = Some(reason.clone());
            }
            State::BackingOff {
                until_wall, status, ..
            } => {
                snapshot.state = "backing_off";
                snapshot.status = Some(*status);
                snapshot.until_unix = until_wall
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs());
            }
        }
        snapshot
    }
}

/// The [`ProviderHealth`] of each profile's provider, by profile and a
/// fingerprint of its base URL and key: one per server, for the rest of its
/// process.
#[derive(Debug, Default)]
pub struct HealthRegistry {
    entries: Mutex<HashMap<(String, u64), Weak<ProviderHealth>>>,
}

impl HealthRegistry {
    /// The health shared by the clients of `profile` (`workspace/name`)
    /// calling `base_url` with `api_key`: the one already held by a live
    /// client of the same profile, base URL and key, else a new, usable one.
    #[must_use]
    pub fn for_profile(
        &self,
        profile: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Arc<ProviderHealth> {
        let mut hasher = DefaultHasher::new();
        base_url.trim_end_matches('/').hash(&mut hasher);
        api_key.hash(&mut hasher);
        let key = (profile.to_string(), hasher.finish());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, health| health.strong_count() > 0);
        if let Some(health) = entries.get(&key).and_then(Weak::upgrade) {
            return health;
        }
        let health = ProviderHealth::new(profile);
        entries.insert(key, Arc::downgrade(&health));
        health
    }

    /// The health of every profile a live client calls, sorted by profile.
    #[must_use]
    pub fn snapshot(&self) -> Vec<HealthSnapshot> {
        let live: Vec<Arc<ProviderHealth>> = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        let mut all: Vec<HealthSnapshot> = live.iter().map(|h| h.snapshot()).collect();
        all.sort_by(|a, b| a.profile.cmp(&b.profile));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_that_say_the_key_is_spent_are_unusable() {
        assert!(matches!(classify(401, "", None), Verdict::Unusable(_)));
        assert!(matches!(classify(402, "", None), Verdict::Unusable(_)));
        let openrouter = r#"{"error":{"message":"Key limit exceeded (total limit). Manage it using https://openrouter.ai/settings/keys","code":403}}"#;
        assert!(matches!(
            classify(403, openrouter, None),
            Verdict::Unusable(_)
        ));
        let openai = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        assert!(matches!(classify(429, openai, None), Verdict::Unusable(_)));
        let region = r#"{"error":{"code":"unsupported_country_region_territory","message":"Country, region, or territory not supported"}}"#;
        assert_eq!(classify(403, region, None), Verdict::Ordinary);
        assert_eq!(classify(500, "key limit exceeded", None), Verdict::Ordinary);
    }

    #[test]
    fn a_rate_limit_backs_off_for_its_retry_after() {
        assert_eq!(
            classify(429, "slow down", retry_after_secs("2")),
            Verdict::BackOff(Duration::from_secs(2))
        );
        assert_eq!(classify(429, "", None), Verdict::BackOff(DEFAULT_BACKOFF));
        assert_eq!(
            classify(429, "", retry_after_secs("Wed, 21 Oct 2015 07:28:00 GMT")),
            Verdict::BackOff(DEFAULT_BACKOFF)
        );
        assert_eq!(
            classify(429, "", Some(Duration::from_secs(86_400))),
            Verdict::BackOff(MAX_BACKOFF)
        );
    }

    #[test]
    fn a_marked_provider_counts_the_calls_it_turns_away() {
        let health = ProviderHealth::new("default/p");
        assert!(health.admit().is_ok());
        assert!(health.observe(500, "", None).is_none());
        assert!(health.observe(402, "no credit", None).is_some());
        assert!(matches!(health.admit(), Err(LlmError::Unavailable(_))));
        assert!(matches!(health.admit(), Err(LlmError::Unavailable(_))));
        let s = health.snapshot();
        assert_eq!((s.state, s.status, s.fallbacks), ("unusable", Some(402), 2));
        // A later rate limit does not lift the mark.
        health.observe(429, "", Some(Duration::from_secs(0)));
        assert_eq!(health.snapshot().state, "unusable");
    }

    #[test]
    fn a_new_key_or_base_url_gets_a_fresh_state() {
        let registry = HealthRegistry::default();
        let a = registry.for_profile("default/p", "http://x/v1", Some("k1"));
        a.observe(401, "", None);
        let same = registry.for_profile("default/p", "http://x/v1/", Some("k1"));
        assert!(Arc::ptr_eq(&a, &same));
        let new_key = registry.for_profile("default/p", "http://x/v1", Some("k2"));
        assert!(new_key.admit().is_ok());
        let new_url = registry.for_profile("default/p", "http://y/v1", Some("k1"));
        assert!(new_url.admit().is_ok());
        drop((a, same));
        let states: Vec<&str> = registry.snapshot().iter().map(|s| s.state).collect();
        assert_eq!(states, ["ok", "ok"]);
    }
}
