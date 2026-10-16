//! Validation of channel triggers, shared by the importers, the Admin API
//! and reconcile.

use std::str::FromStr;

use mb_mcm::{Direction, MIN_SCHEDULE_INTERVAL, Trigger};

/// Check `triggers` for a channel of `direction`:
/// - only [`Direction::Publish`] channels may declare triggers;
/// - a schedule fires at most every [`MIN_SCHEDULE_INTERVAL`];
/// - a cron expression parses (after [`Trigger::normalized_cron`]);
/// - an `on_state_change` names its resource type.
pub fn validate_triggers(triggers: &[Trigger], direction: Direction) -> Result<(), String> {
    if direction == Direction::Subscribe && !triggers.is_empty() {
        return Err("triggers apply only to channels this mock publishes".into());
    }
    for trigger in triggers {
        match trigger {
            Trigger::Schedule { every } if *every < MIN_SCHEDULE_INTERVAL => {
                return Err("schedule interval must be at least 100ms".into());
            }
            Trigger::Cron { expr } => {
                if let Err(e) = cron::Schedule::from_str(&Trigger::normalized_cron(expr)) {
                    return Err(format!("invalid cron expression \"{expr}\": {e}"));
                }
            }
            Trigger::OnStateChange { resource_type, .. } if resource_type.is_empty() => {
                return Err("on_state_change needs a resource_type".into());
            }
            _ => {}
        }
    }
    Ok(())
}
