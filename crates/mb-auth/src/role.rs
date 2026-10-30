use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A workspace role. Ordered `Viewer < Editor < Admin < Owner`.
///
/// `Display` and `FromStr` use the wire names (`viewer`, `editor`, `admin`,
/// `owner`), as serde does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reads everything in the workspace except API keys.
    Viewer,
    /// Also edits mocks, contracts, scenarios and sessions.
    Editor,
    /// Also manages LLM profiles and API keys.
    Admin,
    /// Also changes and deletes the workspace.
    Owner,
}

impl Role {
    /// The wire name (`viewer`, `editor`, `admin`, `owner`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    /// The role whose wire name is exactly `s`, or `None`. [`str::parse`]
    /// does the same with a [`ParseRoleError`].
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "editor" => Some(Role::Editor),
            "admin" => Some(Role::Admin),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }

    /// Whether this role grants `p`.
    pub fn allows(self, p: Permission) -> bool {
        self >= p.min_role()
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = ParseRoleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Role::parse(s).ok_or_else(|| ParseRoleError(s.to_string()))
    }
}

/// A string that is not a [`Role`] wire name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseRoleError(String);

impl ParseRoleError {
    /// The string that was refused.
    pub fn input(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ParseRoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown role {:?} (expected viewer, editor, admin or owner)",
            self.0
        )
    }
}

impl std::error::Error for ParseRoleError {}

/// A workspace-scoped permission.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Read everything in the workspace except API keys.
    Read,
    /// Mocks, contracts, scenarios, session seed/reset.
    Edit,
    /// LLM profiles.
    ManageProfiles,
    /// API keys, including listing them.
    ManageApiKeys,
    /// Workspace PATCH/DELETE.
    ManageWorkspace,
}

impl Permission {
    /// The least role that grants this permission.
    pub fn min_role(self) -> Role {
        match self {
            Permission::Read => Role::Viewer,
            Permission::Edit => Role::Editor,
            Permission::ManageProfiles | Permission::ManageApiKeys => Role::Admin,
            Permission::ManageWorkspace => Role::Owner,
        }
    }
}
