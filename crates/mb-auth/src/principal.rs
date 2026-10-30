use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::role::{Permission, Role};

/// How a [`Principal`] authenticated. `Display` prints the wire name
/// (`user`, `api_key`, `local`), as serde does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A person signed in through the identity provider (a session cookie
    /// or a bearer JWT).
    User,
    /// An API key.
    ApiKey,
    /// The single local user of `auth.mode: none`.
    Local,
}

impl PrincipalKind {
    /// The wire name (`user`, `api_key`, `local`).
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::User => "user",
            PrincipalKind::ApiKey => "api_key",
            PrincipalKind::Local => "local",
        }
    }
}

impl fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An authenticated caller. Stored as JSON in `admin_sessions`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// A stable identifier: the identity provider's `sub`, `api_key:<id>`
    /// or `local`.
    pub subject: String,
    /// A human-readable name for audit logs and the console.
    pub display_name: String,
    /// How the caller authenticated.
    pub kind: PrincipalKind,
    /// A sysadmin is `Owner` in every workspace and may create workspaces.
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

    /// Whether this principal's role in `ws` grants `p`.
    pub fn can(&self, ws: &str, p: Permission) -> bool {
        self.role_in(ws).is_some_and(|r| r.allows(p))
    }
}
