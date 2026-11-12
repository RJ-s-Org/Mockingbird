//! The `shares` group a mock keeps its sessions under (`mocks.<name>.shares`):
//! the session endpoints reach a mock's sessions under it, and `get_world`
//! names it. The server works it out of the workspace's mocks the way its
//! running mocks do ([`AdminState::shares_group`]).

use mb_store::Mock;
use serde::Serialize;
use utoipa::ToSchema;

use crate::{AdminState, ApiResult, Problem};

/// The `shares` value that keeps a mock out of every group.
pub const SHARES_NONE: &str = "none";

/// The group a mock keeps its sessions under.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct SharesGroup {
    /// The group's name: the one the mock's `shares` gives, or the registrable
    /// domain (`github.com`) of its default group.
    #[schema(example = "github.com")]
    pub group: String,
    /// The mock sets no `shares`: it joined the default group of the mocks of
    /// its workspace whose hosts share a registrable domain with its own.
    pub default: bool,
}

/// Works out the group of mock `name` among `mocks` (its workspace's, itself
/// included); `None` when it is in none.
pub type SharesGroupOf = fn(&str, &[Mock]) -> Option<SharesGroup>;

/// The group a mock names in `shares` ([`SHARES_NONE`] names none), without
/// default groups: [`AdminState::new`]'s [`SharesGroupOf`].
#[must_use]
pub fn named_group(name: &str, mocks: &[Mock]) -> Option<SharesGroup> {
    let shares = mocks
        .iter()
        .find(|m| m.name == name)?
        .behavior
        .shares
        .as_deref()?;
    (shares != SHARES_NONE).then(|| SharesGroup {
        group: shares.to_owned(),
        default: false,
    })
}

impl AdminState {
    /// The group mock `name` of `ws` is in (see [`Self::shares_group`]).
    pub(crate) async fn group_of(&self, ws: &str, name: &str) -> ApiResult<Option<SharesGroup>> {
        let mocks = self.store.list_mocks(ws).await?;
        Ok((self.shares_group)(name, &mocks))
    }

    /// The name mock `name` of `ws` keeps its sessions under: its group's, or
    /// its own. 404 for an unknown mock.
    pub(crate) async fn session_holder(&self, ws: &str, name: &str) -> ApiResult<String> {
        let mocks = self.store.list_mocks(ws).await?;
        if !mocks.iter().any(|m| m.name == name) {
            return Err(Problem::not_found(format!("mock {name:?} does not exist")));
        }
        Ok((self.shares_group)(name, &mocks).map_or_else(|| name.to_owned(), |g| g.group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock(name: &str, shares: Option<&str>) -> Mock {
        let mut m: Mock = serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000000",
            "workspace": "default",
            "name": name,
            "route_prefix": "/",
            "persona": null,
            "llm_profile": null,
            "header_overrides": false,
            "source": "api",
            "hosts": ["api.example.test"],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        m.behavior.shares = shares.map(str::to_owned);
        m
    }

    #[test]
    fn named_group_takes_the_mock_s_own_name_and_none_names_none() {
        let mocks = [
            mock("rest", Some("gh")),
            mock("gql", Some(SHARES_NONE)),
            mock("plain", None),
        ];
        assert_eq!(
            named_group("rest", &mocks),
            Some(SharesGroup {
                group: "gh".into(),
                default: false
            })
        );
        assert_eq!(named_group("gql", &mocks), None);
        // No default groups: same-host mocks that set nothing are in none.
        assert_eq!(named_group("plain", &mocks), None);
        assert_eq!(named_group("missing", &mocks), None);
    }
}
