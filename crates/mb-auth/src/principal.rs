use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::role::{Permission, Role};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    User,
    ApiKey,
    Local,
}

/// An authenticated caller. Stored as JSON in `admin_sessions`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub subject: String,
    pub display_name: String,
    pub kind: PrincipalKind,
    pub sysadmin: bool,
    /// Workspace name → role.
    pub roles: BTreeMap<String, Role>,
}

impl Principal {
    /// The principal of every admin request in `auth.mode: none`.
    pub fn local() -> Self {
        Principal {
            subject: "local".to_string(),
            display_name: "Local user".to_string(),
            kind: PrincipalKind::Local,
            sysadmin: true,
            roles: BTreeMap::new(),
        }
    }

    /// The effective role in `ws`; a sysadmin is `Owner` everywhere.
    pub fn role_in(&self, ws: &str) -> Option<Role> {
        if self.sysadmin {
            Some(Role::Owner)
        } else {
            self.roles.get(ws).copied()
        }
    }

    pub fn can(&self, ws: &str, p: Permission) -> bool {
        self.role_in(ws).is_some_and(|r| r.allows(p))
    }
}
