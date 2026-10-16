//! Trigger scheduling for publish channels, behind a scheduler lease so
//! only one replica fires each mock's triggers.

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mb_mcm::{Channel, ChannelBinding, Direction, StateChangeKind, Trigger};
use mb_state::{ChangeKind, StateChanged, StoredResource};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};

use super::MAX_HOPS;
use super::runtime::{ChannelInner, Origin, WORKSPACE};

/// Grants the scheduler lease of a mock to one holder at a time.
#[async_trait]
pub trait LeaseProvider: Send + Sync {
    /// Take or renew the lease `key` for `holder` for `ttl`. `Ok(true)`
    /// when `holder` now holds it.
    async fn try_acquire(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, String>;
}

/// A lease provider for a single replica: every holder always leads.
pub struct AlwaysLeader;

#[async_trait]
impl LeaseProvider for AlwaysLeader {
    async fn try_acquire(&self, _key: &str, _holder: &str, _ttl: Duration) -> Result<bool, String> {
        Ok(true)
    }
}

/// How long a granted scheduler lease lasts.
pub const LEASE_TTL: Duration = Duration::from_secs(30);
/// How often the scheduler lease is renewed.
pub const LEASE_RENEW: Duration = Duration::from_secs(10);

/// Most related resources of the changed type, besides the changed one.
const MAX_RELATED: usize = 5;

/// The triggers of one enabled, active publish channel.
struct ChannelTriggers {
    channel: Channel,
    triggers: Vec<Trigger>,
}

/// Start the trigger tasks of every enabled, active publish channel, the
/// state-change listener when some trigger needs it, and the leader loop
/// when some broker channel's trigger is a schedule or cron.
pub(crate) fn spawn_triggers(inner: &Arc<ChannelInner>, tasks: &mut JoinSet<()>) {
    let active: Vec<ChannelTriggers> = inner
        .rt
        .bundle
        .channels
        .iter()
        .filter(|c| c.direction == Direction::Publish)
        .filter(|c| inner.inactive_reason(&c.id).is_none())
        .filter_map(|c| {
            let setup = inner.setup.channel(c);
            setup.enabled.then(|| ChannelTriggers {
                channel: c.clone(),
                triggers: setup.triggers,
            })
        })
        .collect();

    let mut timed = false;
    let mut on_change: Vec<(Channel, String, StateChangeKind)> = Vec::new();
    for ct in &active {
        for trigger in &ct.triggers {
            match trigger {
                Trigger::Schedule { every } => {
                    timed |= leader_gated(&ct.channel);
                    tasks.spawn(run_schedule(Arc::clone(inner), ct.channel.clone(), *every));
                }
                Trigger::Cron { expr } => {
                    timed |= leader_gated(&ct.channel);
                    tasks.spawn(run_cron(
                        Arc::clone(inner),
                        ct.channel.clone(),
                        expr.clone(),
                    ));
                }
                Trigger::OnStateChange {
                    resource_type,
                    kind,
                } => on_change.push((ct.channel.clone(), resource_type.clone(), *kind)),
                Trigger::Manual => {}
            }
        }
    }
    if !on_change.is_empty() {
        // Subscribe now so no change after `start` returns is missed.
        let changes = inner.rt.state.subscribe();
        inner.listen_state_changes();
        tasks.spawn(run_state_changes(Arc::clone(inner), changes, on_change));
    }
    if timed {
        tasks.spawn(run_leader(Arc::clone(inner)));
    }
}

/// Whether `channel`'s schedule and cron triggers fire on the lease holder
/// only. A broker channel's message reaches every consumer from one
/// replica; a WebSocket channel's reaches only the clients of the replica
/// that sends it, so every replica fires it.
fn leader_gated(channel: &Channel) -> bool {
    !matches!(channel.binding, ChannelBinding::WebSocket { .. })
}

/// Produce on `channel` every `every` while this replica leads (on every
/// replica for a WebSocket channel).
async fn run_schedule(inner: Arc<ChannelInner>, channel: Channel, every: Duration) {
    let stopped = inner.rt.shutdown.stopped();
    tokio::pin!(stopped);
    let gated = leader_gated(&channel);
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let origin = json!({
        "kind": "schedule",
        "every_ms": u64::try_from(every.as_millis()).unwrap_or(u64::MAX),
    });
    loop {
        tokio::select! {
            () = &mut stopped => return,
            _ = ticks.tick() => {}
        }
        if !gated || inner.is_leader() {
            fire(
                &inner,
                &channel,
                Origin::Trigger {
                    trigger: origin.clone(),
                    hop: 0,
                },
                Vec::new(),
                &mut stopped,
            )
            .await;
        }
    }
}

/// Produce on `channel` at each time `expr` (UTC) names while this
/// replica leads (on every replica for a WebSocket channel).
async fn run_cron(inner: Arc<ChannelInner>, channel: Channel, expr: String) {
    let schedule = match cron::Schedule::from_str(&Trigger::normalized_cron(&expr)) {
        Ok(schedule) => schedule,
        Err(e) => {
            tracing::error!(mock = %inner.rt.name, channel = %channel.id, %expr, error = %e,
                "invalid cron expression; trigger disabled");
            return;
        }
    };
    let stopped = inner.rt.shutdown.stopped();
    tokio::pin!(stopped);
    let gated = leader_gated(&channel);
    let origin = json!({"kind": "cron", "expr": expr});
    // The last time fired, so a timer waking a little early against the
    // wall clock never fires the same time twice.
    let mut last: Option<DateTime<Utc>> = None;
    loop {
        let now = Utc::now();
        let from = last.map_or(now, |last| last.max(now));
        let Some(next) = schedule.after(&from).next() else {
            return;
        };
        last = Some(next);
        let wait = (next - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        tokio::select! {
            () = &mut stopped => return,
            () = tokio::time::sleep(wait) => {}
        }
        if !gated || inner.is_leader() {
            fire(
                &inner,
                &channel,
                Origin::Trigger {
                    trigger: origin.clone(),
                    hop: 0,
                },
                Vec::new(),
                &mut stopped,
            )
            .await;
        }
    }
}

/// Produce once, unless shutdown begins first. Errors are recorded by
/// `produce` and never stop the trigger.
async fn fire(
    inner: &Arc<ChannelInner>,
    channel: &Channel,
    origin: Origin,
    related: Vec<StoredResource>,
    stopped: &mut std::pin::Pin<&mut impl Future<Output = ()>>,
) {
    tokio::select! {
        () = stopped.as_mut() => {}
        result = inner.produce(channel, origin, related) => {
            if let Err(e) = result {
                tracing::debug!(mock = %inner.rt.name, channel = %channel.id, error = %e, "trigger publish failed");
            }
        }
    }
}

fn kind_matches(want: StateChangeKind, got: ChangeKind) -> bool {
    matches!(
        (want, got),
        (StateChangeKind::Any, _)
            | (StateChangeKind::Upserted, ChangeKind::Upserted)
            | (StateChangeKind::Deleted, ChangeKind::Deleted)
    )
}

/// Fan the mock's state changes out to the matching triggers. Runs on
/// every replica, leader or not.
async fn run_state_changes(
    inner: Arc<ChannelInner>,
    mut changes: broadcast::Receiver<StateChanged>,
    triggers: Vec<(Channel, String, StateChangeKind)>,
) {
    let stopped = inner.rt.shutdown.stopped();
    tokio::pin!(stopped);
    loop {
        let change = tokio::select! {
            () = &mut stopped => return,
            change = changes.recv() => change,
        };
        let change = match change {
            Ok(change) => change,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(mock = %inner.rt.name, skipped = n, "state-change triggers lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if change.session.workspace != WORKSPACE || change.session.mock != inner.rt.name {
            continue;
        }
        // A change made by a consumed message continues its hop count, so
        // a channel consuming what this trigger publishes stops at the
        // hop limit instead of looping.
        let hop = match inner.take_state_hop(&(
            change.session.session.clone(),
            change.resource_type.clone(),
            change.id.clone(),
        )) {
            Some(received) if received >= MAX_HOPS => {
                tracing::debug!(mock = %inner.rt.name, resource_type = %change.resource_type,
                    id = %change.id, "state-change trigger skipped: hop limit");
                continue;
            }
            Some(received) => received + 1,
            None => 0,
        };
        let matching: Vec<&Channel> = triggers
            .iter()
            .filter(|(_, ty, kind)| *ty == change.resource_type && kind_matches(*kind, change.kind))
            .map(|(channel, _, _)| channel)
            .collect();
        if matching.is_empty() {
            continue;
        }
        let related = match change.kind {
            ChangeKind::Upserted => match related(&inner, &change).await {
                Some(related) => related,
                None => continue,
            },
            ChangeKind::Deleted => Vec::new(),
        };
        let origin = json!({
            "kind": "on_state_change",
            "resource_type": change.resource_type,
            "id": change.id,
            "change": match change.kind {
                ChangeKind::Upserted => "upserted",
                ChangeKind::Deleted => "deleted",
            },
            "session": change.session.session,
        });
        for channel in matching {
            fire(
                &inner,
                channel,
                Origin::Trigger {
                    trigger: origin.clone(),
                    hop,
                },
                related.clone(),
                &mut stopped,
            )
            .await;
        }
    }
}

/// The upserted resource, then up to five more of its type; `None` when
/// it is gone or cannot be read.
async fn related(inner: &ChannelInner, change: &StateChanged) -> Option<Vec<StoredResource>> {
    let state = &inner.rt.state;
    let resource = match state
        .get(&change.session, &change.resource_type, &change.id)
        .await
    {
        Ok(Some(resource)) => resource,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(mock = %inner.rt.name, error = %e, "state-change trigger: reading the resource failed");
            return None;
        }
    };
    let others = state
        .list(&change.session, &change.resource_type, MAX_RELATED + 1)
        .await
        .unwrap_or_default();
    let mut out = vec![resource];
    out.extend(
        others
            .into_iter()
            .filter(|r| r.id != change.id)
            .take(MAX_RELATED),
    );
    Some(out)
}

/// Take and renew the mock's scheduler lease. A failed attempt keeps the
/// current leadership until `LEASE_TTL` after the last grant. Each attempt
/// is bounded by `LEASE_RENEW` and by the time left on the current grant,
/// so a hung lease store never keeps this replica leading past it.
async fn run_leader(inner: Arc<ChannelInner>) {
    let stopped = inner.rt.shutdown.stopped();
    tokio::pin!(stopped);
    let mut ticks = tokio::time::interval(LEASE_RENEW);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_grant: Option<Instant> = None;
    loop {
        tokio::select! {
            () = &mut stopped => return,
            _ = ticks.tick() => {}
        }
        let started = Instant::now();
        let limit = match last_grant {
            Some(at) if inner.is_leader() => {
                LEASE_RENEW.min((at + LEASE_TTL).saturating_duration_since(started))
            }
            _ => LEASE_RENEW,
        };
        let acquire = tokio::time::timeout(
            limit,
            inner
                .deps
                .lease
                .try_acquire(&inner.setup.lease_key, &inner.deps.holder, LEASE_TTL),
        );
        let attempt = tokio::select! {
            () = &mut stopped => return,
            attempt = acquire => attempt.unwrap_or_else(|_| {
                Err(format!("timed out after {} ms", limit.as_millis()))
            }),
        };
        match attempt {
            Ok(leader) => {
                // The grant runs from when it was asked for.
                if leader {
                    last_grant = Some(started);
                }
                inner.set_leader(leader);
            }
            Err(e) => {
                tracing::warn!(mock = %inner.rt.name, error = %e, "scheduler lease renewal failed");
                if last_grant.is_none_or(|at| at.elapsed() >= LEASE_TTL) {
                    inner.set_leader(false);
                }
            }
        }
    }
}
