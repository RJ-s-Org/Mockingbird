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
    pub roles_claim: String,
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

        let groups: Vec<&str> = match claims.get(&self.roles_claim) {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => s.split_whitespace().collect(),
            _ => Vec::new(),
        };

        let mut sysadmin = self.sysadmin_subjects.iter().any(|s| s == sub);
        let mut roles: BTreeMap<String, Role> = BTreeMap::new();
        for group in groups {
            let Some(rest) = group
                .strip_prefix(self.group_prefix.as_str())
                .and_then(|r| r.strip_prefix(':'))
            else {
                continue;
            };
            if rest == "sysadmin" {
                sysadmin = true;
                continue;
            }
            let Some((ws, role)) = rest.split_once(':') else {
                continue;
            };
            let Some(role) = Role::parse(role) else {
                continue;
            };
            if mb_store::validate_workspace_name(ws).is_err() {
                continue;
            }
            let entry = roles.entry(ws.to_string()).or_insert(role);
            *entry = (*entry).max(role);
        }

        Ok(Principal {
            subject: sub.to_string(),
            display_name,
            kind: PrincipalKind::User,
            sysadmin,
            roles,
        })
    }
}
