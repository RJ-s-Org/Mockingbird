use serde::{Deserialize, Serialize};

/// A workspace role. Ordered `Viewer < Editor < Admin < Owner`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

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
    pub fn min_role(self) -> Role {
        match self {
            Permission::Read => Role::Viewer,
            Permission::Edit => Role::Editor,
            Permission::ManageProfiles | Permission::ManageApiKeys => Role::Admin,
            Permission::ManageWorkspace => Role::Owner,
        }
    }
}
