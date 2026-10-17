//! A mock's daily token budget, shared by every replica through the
//! control store's `token_usage` table (spec §11.3).
//!
//! Spending is counted locally and flushed every
//! [`BUDGET_FLUSH_INTERVAL`]; each flush learns the total every replica
//! has flushed for the UTC day. The budget is exhausted once that total
//! plus the local, unflushed spend reaches the limit; it resets at UTC
//! midnight.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use mb_store::{ControlStore, Mock};
use uuid::Uuid;

use crate::registry::mock_label;

/// How often local spend is written to the store.
pub const BUDGET_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Counter of LLM calls refused by an exhausted budget, labelled by `mock`.
pub const BUDGET_EXHAUSTED_METRIC: &str = "mb_token_budget_exhausted_total";
/// Gauge of the tokens spent today, across replicas, labelled by `mock`;
/// set on each flush.
pub const BUDGET_USED_METRIC: &str = "mb_token_budget_used";

/// A [`mb_engine::TokenBudget`] backed by the control store.
pub struct StoreTokenBudget {
    store: Arc<dyn ControlStore>,
    mock_id: Uuid,
    /// The mock as named in metrics and `/readyz` (see [`mock_label`]).
    label: String,
    /// Tokens per UTC day.
    limit: u64,
    /// The UTC day `known_total` counts.
    day: Mutex<NaiveDate>,
    /// Spent here and not yet written to the store.
    unflushed: AtomicU64,
    /// The day's total across replicas, as of the last flush.
    known_total: AtomicU64,
}

impl StoreTokenBudget {
    /// A budget of `limit` tokens per UTC day for `mock`, with a task that
    /// flushes it every [`BUDGET_FLUSH_INTERVAL`] (the first time at once,
    /// to learn the day's total so far). The task holds only a [`Weak`]
    /// reference and ends once the budget is dropped with its runtime.
    ///
    /// Must be called inside a Tokio runtime.
    pub fn spawn(store: Arc<dyn ControlStore>, mock: &Mock, limit: u64) -> Arc<Self> {
        let budget = Arc::new(Self {
            store,
            mock_id: mock.id,
            label: mock_label(&mock.workspace, &mock.name),
            limit,
            day: Mutex::new(today()),
            unflushed: AtomicU64::new(0),
            known_total: AtomicU64::new(0),
        });
        tokio::spawn(flush_loop(Arc::downgrade(&budget)));
        budget
    }

    /// The mock's label, as in metrics and `/readyz`.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Tokens spent today as far as this replica knows: the total at the
    /// last flush plus the local, unflushed spend.
    pub fn used(&self) -> u64 {
        self.roll(today());
        self.known_total
            .load(Ordering::SeqCst)
            .saturating_add(self.unflushed.load(Ordering::SeqCst))
    }

    /// Whether today's spend has reached the limit. Unlike
    /// [`mb_engine::TokenBudget::exhausted`], records no metric (for
    /// `/readyz`).
    pub fn is_exhausted(&self) -> bool {
        self.used() >= self.limit
    }

    /// Writes the unflushed spend to the store and learns the day's total
    /// across replicas (sets [`BUDGET_USED_METRIC`]). On a store error the
    /// spend stays unflushed for the next attempt.
    pub async fn flush(&self) {
        let day = today();
        self.roll(day);
        let tokens = self.unflushed.swap(0, Ordering::SeqCst);
        // Counted as known while in flight, so the budget never looks
        // emptier than it is.
        self.known_total.fetch_add(tokens, Ordering::SeqCst);
        let result = if tokens == 0 {
            self.store.token_usage(self.mock_id, day).await
        } else {
            self.store.add_token_usage(self.mock_id, day, tokens).await
        };
        match result {
            Ok(total) => {
                if self.current_day() == day {
                    self.known_total.store(total, Ordering::SeqCst);
                }
                metrics::gauge!(BUDGET_USED_METRIC, "mock" => self.label.clone()).set(total as f64);
            }
            Err(error) => {
                tracing::warn!(mock = %self.label, %error, "cannot record token usage; retrying at the next flush");
                saturating_sub(&self.known_total, tokens);
                self.unflushed.fetch_add(tokens, Ordering::SeqCst);
            }
        }
    }

    fn current_day(&self) -> NaiveDate {
        *self.day.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Starts a new day's count once the UTC date changes.
    fn roll(&self, today: NaiveDate) {
        let mut day = self.day.lock().unwrap_or_else(|p| p.into_inner());
        if *day != today {
            *day = today;
            self.known_total.store(0, Ordering::SeqCst);
        }
    }
}

impl mb_engine::TokenBudget for StoreTokenBudget {
    fn exhausted(&self) -> bool {
        let exhausted = self.is_exhausted();
        if exhausted {
            metrics::counter!(BUDGET_EXHAUSTED_METRIC, "mock" => self.label.clone()).increment(1);
        }
        exhausted
    }

    fn spend(&self, tokens: u64) {
        self.unflushed.fetch_add(tokens, Ordering::SeqCst);
    }
}

impl Drop for StoreTokenBudget {
    /// Writes the last unflushed spend in the background, when a runtime
    /// is still running.
    fn drop(&mut self) {
        let tokens = self.unflushed.swap(0, Ordering::SeqCst);
        if tokens == 0 {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let (mock_id, day, label) = (self.mock_id, self.current_day(), self.label.clone());
        handle.spawn(async move {
            if let Err(error) = store.add_token_usage(mock_id, day, tokens).await {
                tracing::warn!(mock = %label, %error, "cannot record the last token usage");
            }
        });
    }
}

async fn flush_loop(budget: Weak<StoreTokenBudget>) {
    let mut ticks = tokio::time::interval(BUDGET_FLUSH_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let Some(budget) = budget.upgrade() else {
            break;
        };
        budget.flush().await;
    }
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

fn saturating_sub(n: &AtomicU64, by: u64) {
    let _ = n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
        Some(v.saturating_sub(by))
    });
}
