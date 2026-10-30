use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::AuthError;
use crate::principal::{Principal, PrincipalKind};
use crate::role::Role;

/// How identity-provider claims become a [`Principal`].
///
/// Group strings `<prefix>:<workspace>:<role>` grant roles and
/// `<prefix>:sysadmin` grants the sysadmin flag; anything else is ignored.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct RoleMapping {
    /// The claim holding the group strings (default `groups`).
    pub roles_claim: String,
    /// The prefix of the group strings that grant roles (default `mb`).
    pub group_prefix: String,
    /// `sub` values that are always sysadmin.
    pub sysadmin_subjects: Vec<String>,
}

impl Default for RoleMapping {
    fn default() -> Self {
        RoleMapping {
            roles_claim: "groups".to_string(),
            group_prefix: "mb".to_string(),
            sysadmin_subjects: Vec::new(),
        }
    }
}

impl RoleMapping {
    /// The user principal `claims` describe: `sub` as its subject, `name`,
    /// `preferred_username` or `email` as its display name, and the roles
    /// and sysadmin flag its groups grant.
    ///
    /// # Errors
    ///
    /// [`AuthError::Unauthenticated`] when `claims` has no non-empty `sub`.
    pub fn principal(&self, claims: &Map<String, Value>) -> Result<Principal, AuthError> {
        let sub = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AuthError::Unauthenticated("token has no `sub` claim".to_string()))?;
        let display_name = ["name", "preferred_username", "email"]
            .iter()
            .filter_map(|c| claims.get(*c).and_then(Value::as_str))
            .find(|s| !s.is_empty())
            .unwrap_or(sub)
            .to_string();

        let mut sysadmin = self.sysadmin_subjects.iter().any(|s| s == sub);
        let mut roles: BTreeMap<String, Role> = BTreeMap::new();
        for group in self.groups(claims) {
            match self.parse_group(group) {
                Some(Grant::Sysadmin) => sysadmin = true,
                Some(Grant::Role(ws, role)) => {
                    let entry = roles.entry(ws.to_string()).or_insert(role);
                    *entry = (*entry).max(role);
                }
                None => {}
            }
        }

        Ok(Principal {
            subject: sub.to_string(),
            display_name,
            kind: PrincipalKind::User,
            sysadmin,
            roles,
        })
    }

    /// The groups in `claims` that carry this mapping's prefix but grant
    /// nothing (an unknown role, a missing segment or an invalid workspace
    /// name), in claim order; [`RoleMapping::principal`] skips them.
    pub fn ignored_groups<'a>(&self, claims: &'a Map<String, Value>) -> Vec<&'a str> {
        self.groups(claims)
            .into_iter()
            .filter(|g| self.prefixed(g).is_some() && self.parse_group(g).is_none())
            .collect()
    }

    fn groups<'a>(&self, claims: &'a Map<String, Value>) -> Vec<&'a str> {
        match claims.get(&self.roles_claim) {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => s.split_whitespace().collect(),
            _ => Vec::new(),
        }
    }

    /// `group` without `<prefix>:`, or `None` when it lacks the prefix.
    fn prefixed<'a>(&self, group: &'a str) -> Option<&'a str> {
        group
            .strip_prefix(self.group_prefix.as_str())
            .and_then(|r| r.strip_prefix(':'))
    }

    fn parse_group<'a>(&self, group: &'a str) -> Option<Grant<'a>> {
        let rest = self.prefixed(group)?;
        if rest == "sysadmin" {
            return Some(Grant::Sysadmin);
        }
        let (ws, role) = rest.split_once(':')?;
        let role = Role::parse(role)?;
        mb_store::validate_workspace_name(ws).ok()?;
        Some(Grant::Role(ws, role))
    }
}

/// What one prefixed group grants.
enum Grant<'a> {
    Sysadmin,
    Role(&'a str, Role),
}
