//! Validation of channel triggers, shared by the importers, the Admin API
//! and reconcile.

use std::str::FromStr;
use std::time::Duration;

use mb_mcm::{Direction, MIN_SCHEDULE_INTERVAL, Trigger};

/// Why [`check_triggers`] rejected a channel's triggers. The `Display` text
/// is the message the Admin API and the config loader report.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum TriggerError {
    /// A channel the mock subscribes to declares triggers.
    #[error("triggers apply only to channels this mock publishes")]
    NotPublish,
    /// A schedule fires more often than [`MIN_SCHEDULE_INTERVAL`].
    #[error("schedule interval must be at least {}ms", .min.as_millis())]
    ScheduleTooFrequent {
        /// The shortest accepted interval.
        min: Duration,
    },
    /// A cron expression does not parse.
    #[error("invalid cron expression \"{expr}\": {reason}")]
    InvalidCron {
        /// The expression as written.
        expr: String,
        /// The parser's message.
        reason: String,
    },
    /// An `on_state_change` trigger names no resource type.
    #[error("on_state_change needs a resource_type")]
    MissingResourceType,
}

/// Check `triggers` for a channel of `direction`:
/// - only [`Direction::Publish`] channels may declare triggers;
/// - a schedule fires at most every [`MIN_SCHEDULE_INTERVAL`];
/// - a cron expression parses (after [`Trigger::normalized_cron`]);
/// - an `on_state_change` names its resource type.
///
/// # Errors
///
/// The first rule broken, as a [`TriggerError`].
pub fn check_triggers(triggers: &[Trigger], direction: Direction) -> Result<(), TriggerError> {
    if direction == Direction::Subscribe && !triggers.is_empty() {
        return Err(TriggerError::NotPublish);
    }
    for trigger in triggers {
        match trigger {
            Trigger::Schedule { every } if *every < MIN_SCHEDULE_INTERVAL => {
                return Err(TriggerError::ScheduleTooFrequent {
                    min: MIN_SCHEDULE_INTERVAL,
                });
            }
            Trigger::Cron { expr } => {
                if let Err(e) = cron::Schedule::from_str(&Trigger::normalized_cron(expr)) {
                    return Err(TriggerError::InvalidCron {
                        expr: expr.clone(),
                        reason: e.to_string(),
                    });
                }
            }
            Trigger::OnStateChange { resource_type, .. } if resource_type.is_empty() => {
                return Err(TriggerError::MissingResourceType);
            }
            _ => {}
        }
    }
    Ok(())
}
