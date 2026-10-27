//! `mockingbird agent install|print|skills`: registers the MCP server
//! (`mockingbird mcp`) with a coding agent and copies the Mockingbird
//! skills where that agent reads them.
//!
//! The skills are embedded from `plugins/mockingbird/skills`, the same
//! files the Claude Code, Codex and Cursor plugins ship, so every channel
//! gives an agent the same instructions.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

/// One embedded skill: its directory name and `SKILL.md` text.
pub struct Skill {
    pub name: &'static str,
    pub text: &'static str,
}

macro_rules! skill {
    ($name:literal) => {
        Skill {
            name: $name,
            text: include_str!(concat!(
                "../../../plugins/mockingbird/skills/",
                $name,
                "/SKILL.md"
            )),
        }
    };
}

/// Every skill, the entry point first.
pub const SKILLS: &[Skill] = &[
    skill!("mockingbird"),
    skill!("mockingbird-setup"),
    skill!("mockingbird-mock-api"),
    skill!("mockingbird-deploy"),
    skill!("mockingbird-testing"),
];

impl Skill {
    /// The `description:` of the front matter.
    pub fn description(&self) -> &'static str {
        self.front_matter("description").unwrap_or_default()
    }

    fn front_matter(&self, key: &str) -> Option<&'static str> {
        let body = self.text.strip_prefix("---\n")?;
        let end = body.find("\n---")?;
        body[..end].lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|r| r.strip_prefix(':'))
                .map(str::trim)
        })
    }

    /// The text after the front matter.
    pub fn body(&self) -> &'static str {
        self.text
            .strip_prefix("---\n")
            .and_then(|b| b.find("\n---\n").map(|i| &b[i + 5..]))
            .unwrap_or(self.text)
            .trim_start()
    }
}

/// The agents `agent install` knows.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    Cursor,
    Gemini,
    Kiro,
    Vscode,
    Windsurf,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scope {
    /// This project (the current directory).
    #[default]
    Project,
    /// The user's home directory: every project.
    User,
}

#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Register the MCP server with an agent and install the skills.
    Install(InstallArgs),
    /// Print the MCP server entry for an agent without writing anything.
    Print {
        #[arg(value_enum)]
        client: Client,
    },
    /// Write the skills (SKILL.md files) into a directory.
    Skills {
        /// Target directory; each skill goes to `<dir>/<name>/SKILL.md`.
        dir: PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[arg(value_enum)]
    pub client: Client,
    #[arg(long, value_enum, default_value_t = Scope::Project)]
    pub scope: Scope,
    /// Register the MCP server only.
    #[arg(long)]
    pub no_skills: bool,
    /// Install the skills only.
    #[arg(long)]
    pub no_mcp: bool,
    /// Show what would be written.
    #[arg(long)]
    pub dry_run: bool,
    /// Command the agent runs [default: this binary's absolute path].
    #[arg(long)]
    pub command: Option<String>,
    /// Config whose default LLM profile names the key variables to pass
    /// through to the MCP server.
    #[arg(short = 'c', long, default_value = "mockingbird.yaml")]
    pub config: PathBuf,
    /// Another environment variable to pass through (its name; repeatable).
    #[arg(long = "pass-env", value_name = "NAME")]
    pub pass_env: Vec<String>,
}

/// How an agent's MCP config refers to a variable of the agent's own
/// environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvSyntax {
    /// `"env": {"NAME": "${NAME}"}`.
    Braced,
    /// `"env": {"NAME": "${env:NAME}"}`.
    EnvPrefixed,
    /// `env_vars = ["NAME"]` in Codex's TOML table.
    CodexEnvVars,
    /// The agent expands nothing in its MCP config.
    Unsupported,
}

/// The passthrough syntax of `client`'s MCP config for `scope` (each from
/// the agent's MCP configuration docs; see docs/agents.md).
pub fn env_syntax(client: Client, scope: Scope) -> EnvSyntax {
    match (client, scope) {
        (Client::ClaudeCode, Scope::Project) => EnvSyntax::Braced,
        // A printed `claude mcp add` line: a shell would expand the value
        // into ~/.claude.json.
        (Client::ClaudeCode, Scope::User) => EnvSyntax::Unsupported,
        (Client::ClaudeDesktop, _) => EnvSyntax::Unsupported,
        (Client::Codex, _) => EnvSyntax::CodexEnvVars,
        (Client::Cursor | Client::Vscode | Client::Windsurf, _) => EnvSyntax::EnvPrefixed,
        // Kiro expands only the names approved in its settings.
        (Client::Gemini | Client::Kiro, _) => EnvSyntax::Braced,
    }
}

/// Whether `name` can be an environment variable name.
fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot find the home directory (HOME)"))
}

/// The command the agent should run.
fn command(args_command: Option<&str>) -> String {
    args_command
        .map(String::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| "mockingbird".into())
}

/// The `mcpServers` entry most agents take.
fn server_entry(command: &str) -> Value {
    json!({ "command": command, "args": ["mcp"] })
}

/// Where `client` keeps its MCP config for `scope`, and under which key
/// its server map lives.
fn mcp_target(client: Client, scope: Scope) -> anyhow::Result<Option<(PathBuf, &'static str)>> {
    let base = match scope {
        Scope::Project => PathBuf::from("."),
        Scope::User => home()?,
    };
    Ok(Some(match (client, scope) {
        (Client::ClaudeCode, Scope::Project) => (base.join(".mcp.json"), "mcpServers"),
        // `claude mcp add --scope user` owns ~/.claude.json.
        (Client::ClaudeCode, Scope::User) => return Ok(None),
        (Client::ClaudeDesktop, _) => (claude_desktop_config()?, "mcpServers"),
        // TOML: handled separately.
        (Client::Codex, _) => return Ok(None),
        (Client::Cursor, _) => (base.join(".cursor/mcp.json"), "mcpServers"),
        (Client::Gemini, _) => (base.join(".gemini/settings.json"), "mcpServers"),
        (Client::Kiro, _) => (base.join(".kiro/settings/mcp.json"), "mcpServers"),
        (Client::Vscode, Scope::Project) => (base.join(".vscode/mcp.json"), "servers"),
        (Client::Vscode, Scope::User) => return Ok(None),
        (Client::Windsurf, _) => (
            home()?.join(".codeium/windsurf/mcp_config.json"),
            "mcpServers",
        ),
    }))
}

fn claude_desktop_config() -> anyhow::Result<PathBuf> {
    let home = home()?;
    Ok(match std::env::consts::OS {
        "macos" => home.join("Library/Application Support/Claude/claude_desktop_config.json"),
        "windows" => std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Claude/claude_desktop_config.json"),
        _ => home.join(".config/Claude/claude_desktop_config.json"),
    })
}

/// Where `client` reads skills for `scope`, if it does.
fn skills_dir(client: Client, scope: Scope) -> anyhow::Result<Option<PathBuf>> {
    let base = match scope {
        Scope::Project => PathBuf::from("."),
        Scope::User => home()?,
    };
    Ok(match (client, scope) {
        (Client::ClaudeCode, _) => Some(base.join(".claude/skills")),
        (Client::Codex, Scope::Project) => Some(base.join(".agents/skills")),
        (Client::Codex, Scope::User) => Some(
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| base.join(".codex"))
                .join("skills"),
        ),
        (Client::Cursor, _) => Some(base.join(".cursor/skills")),
        (Client::Gemini, _) => Some(base.join(".gemini/skills")),
        (Client::Vscode, Scope::Project) => Some(base.join(".github/skills")),
        // Kiro reads steering files, written separately.
        (Client::Kiro, _) => Some(base.join(".kiro/steering")),
        _ => None,
    })
}

/// The server entry as `client` wants it, passing `env` through (names
/// only) where its config can refer to them.
fn entry_for(client: Client, command: &str, env: &[String], scope: Scope) -> Value {
    let mut entry = match client {
        Client::Vscode => json!({ "type": "stdio", "command": command, "args": ["mcp"] }),
        _ => server_entry(command),
    };
    let reference = match env_syntax(client, scope) {
        EnvSyntax::Braced => |n: &str| format!("${{{n}}}"),
        EnvSyntax::EnvPrefixed => |n: &str| format!("${{env:{n}}}"),
        EnvSyntax::CodexEnvVars | EnvSyntax::Unsupported => return entry,
    };
    if !env.is_empty() {
        let vars: Map<String, Value> = env
            .iter()
            .map(|n| (n.clone(), json!(reference(n))))
            .collect();
        entry["env"] = Value::Object(vars);
    }
    entry
}

fn codex_toml(command: &str, env: &[String]) -> String {
    let mut table = format!(
        "[mcp_servers.mockingbird]\ncommand = {}\nargs = [\"mcp\"]\n",
        toml_string(command)
    );
    if !env.is_empty() {
        let names: Vec<String> = env.iter().map(|n| toml_string(n)).collect();
        table.push_str(&format!("env_vars = [{}]\n", names.join(", ")));
    }
    table
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `agent print`.
pub fn print(client: Client) -> Value {
    let cmd = command(None);
    match client {
        Client::Codex => json!({ "file": "~/.codex/config.toml", "toml": codex_toml(&cmd, &[]) }),
        Client::ClaudeCode => json!({
            "command": format!("claude mcp add --scope user mockingbird -- {cmd} mcp"),
            "json": { "mcpServers": { "mockingbird": server_entry(&cmd) } },
        }),
        Client::Vscode => {
            json!({ "servers": { "mockingbird": entry_for(client, &cmd, &[], Scope::User) } })
        }
        _ => json!({ "mcpServers": { "mockingbird": entry_for(client, &cmd, &[], Scope::User) } }),
    }
}

/// Sets `<key>.mockingbird` in the JSON file at `path`, keeping every other
/// entry. An existing file that is not a JSON object is left alone.
fn merge_json(path: &Path, key: &str, entry: Value, dry_run: bool) -> anyhow::Result<Value> {
    let mut doc = if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(&text).with_context(|| {
                format!(
                    "{} is not plain JSON (comments?); add the entry by hand: {}",
                    path.display(),
                    json!({ key: { "mockingbird": entry } })
                )
            })?
        }
    } else {
        json!({})
    };
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let servers = obj
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{key} in {} is not an object", path.display()))?;
    let replaced = servers.insert("mockingbird".into(), entry).is_some();
    if !dry_run {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&doc)? + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(json!({ "file": path, "key": format!("{key}.mockingbird"), "replaced": replaced }))
}

fn install_codex(scope: Scope, cmd: &str, env: &[String], dry_run: bool) -> anyhow::Result<Value> {
    let path = match scope {
        Scope::Project => PathBuf::from(".codex/config.toml"),
        Scope::User => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".codex"))
            .join("config.toml"),
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.contains("[mcp_servers.mockingbird]") {
        return Ok(json!({ "file": path, "unchanged": true,
            "detail": "a [mcp_servers.mockingbird] table exists; edit it by hand to change it" }));
    }
    if !dry_run {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let sep = if text.is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        std::fs::write(&path, format!("{text}{sep}{}", codex_toml(cmd, env)))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(json!({ "file": path, "added": "[mcp_servers.mockingbird]" }))
}

/// Writes every skill under `dir`; Kiro gets steering files instead.
fn write_skills(dir: &Path, kiro: bool, dry_run: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for skill in SKILLS {
        let (path, text) = if kiro {
            (
                dir.join(format!("{}.md", skill.name)),
                format!(
                    "---\ninclusion: manual\n---\n\n<!-- {} -->\n\n{}",
                    skill.description(),
                    skill.body()
                ),
            )
        } else {
            (
                dir.join(skill.name).join("SKILL.md"),
                skill.text.to_string(),
            )
        };
        if !dry_run {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        written.push(path);
    }
    Ok(written)
}

/// `agent install`.
pub fn install(args: &InstallArgs) -> anyhow::Result<Value> {
    if args.no_mcp && args.no_skills {
        bail!("--no-mcp and --no-skills leave nothing to do");
    }
    if let Some(bad) = args.pass_env.iter().find(|n| !env_name(n)) {
        bail!("--pass-env {bad:?} is not a variable name");
    }
    let mut names = crate::daemon::default_profile_env_names(&args.config);
    names.extend(args.pass_env.iter().cloned());
    names.sort();
    names.dedup();
    let cmd = command(args.command.as_deref());
    let mut out = Map::new();
    let mut next = Vec::new();
    if !args.no_mcp {
        let syntax = env_syntax(args.client, args.scope);
        let mcp = match args.client {
            Client::Codex => install_codex(args.scope, &cmd, &names, args.dry_run)?,
            client => match mcp_target(client, args.scope)? {
                Some((path, key)) => merge_json(
                    &path,
                    key,
                    entry_for(client, &cmd, &names, args.scope),
                    args.dry_run,
                )?,
                None if client == Client::ClaudeCode => {
                    let line = format!("claude mcp add --scope user mockingbird -- {cmd} mcp");
                    next.push(format!("run: {line}"));
                    json!({ "command": line })
                }
                None => {
                    next.push(
                        "add the server in the editor's user settings (MCP: Add Server), or use --scope project".into(),
                    );
                    print(client)
                }
            },
        };
        out.insert("mcp".into(), mcp);
        let written = !names.is_empty()
            && syntax != EnvSyntax::Unsupported
            && !(args.client == Client::Codex && out["mcp"].get("unchanged").is_some());
        if written {
            out.insert("passthrough".into(), json!(names));
            if args.client == Client::Kiro {
                next.push(format!(
                    "approve {} in Kiro's settings (Mcp Approved Env Vars) so Kiro expands them",
                    names.join(", ")
                ));
            }
        } else if !names.is_empty() && syntax == EnvSyntax::Unsupported {
            out.insert(
                "passthrough_unsupported".into(),
                json!(format!(
                    "{} cannot read variables in its MCP config; run `mockingbird keys setup` to store the key in a file instead",
                    args.client.to_possible_value().map(|v| v.get_name().to_string()).unwrap_or_default()
                )),
            );
        }
    }
    if !args.no_skills {
        match skills_dir(args.client, args.scope)? {
            Some(dir) => {
                let files = write_skills(&dir, args.client == Client::Kiro, args.dry_run)?;
                out.insert("skills".into(), json!(files));
            }
            None => {
                out.insert(
                    "skills".into(),
                    json!("this agent has no skills directory; the MCP server's prompts and resources carry the same guidance"),
                );
            }
        }
    }
    next.push("restart the agent (or reload its MCP servers) to pick Mockingbird up".into());
    next.push("then ask it to run the `doctor` tool".into());
    out.insert("dry_run".into(), json!(args.dry_run));
    out.insert("next_steps".into(), json!(next));
    Ok(Value::Object(out))
}

/// `agent skills <dir>`.
pub fn skills(dir: &Path) -> anyhow::Result<Value> {
    Ok(json!({ "written": write_skills(dir, false, false)? }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_uses_each_agents_syntax() {
        let env = vec!["OPENROUTER_API_KEY".to_string()];
        let var = |client, scope| entry_for(client, "/mb", &env, scope)["env"]["OPENROUTER_API_KEY"].clone();
        assert_eq!(var(Client::Cursor, Scope::Project), "${env:OPENROUTER_API_KEY}");
        assert_eq!(var(Client::Windsurf, Scope::User), "${env:OPENROUTER_API_KEY}");
        assert_eq!(var(Client::Vscode, Scope::Project), "${env:OPENROUTER_API_KEY}");
        assert_eq!(var(Client::ClaudeCode, Scope::Project), "${OPENROUTER_API_KEY}");
        assert_eq!(var(Client::Gemini, Scope::Project), "${OPENROUTER_API_KEY}");
        assert_eq!(var(Client::Kiro, Scope::Project), "${OPENROUTER_API_KEY}");
        assert!(
            entry_for(Client::ClaudeDesktop, "/mb", &env, Scope::User)
                .get("env")
                .is_none()
        );
        assert!(
            entry_for(Client::Cursor, "/mb", &[], Scope::Project)
                .get("env")
                .is_none()
        );
        assert_eq!(env_syntax(Client::ClaudeCode, Scope::User), EnvSyntax::Unsupported);
        assert_eq!(env_syntax(Client::Codex, Scope::User), EnvSyntax::CodexEnvVars);
        assert!(codex_toml("/mb", &env).contains("env_vars = [\"OPENROUTER_API_KEY\"]"));
        assert!(!codex_toml("/mb", &[]).contains("env_vars"));
    }

    #[test]
    fn skills_have_front_matter() {
        for s in SKILLS {
            assert_eq!(s.front_matter("name"), Some(s.name), "{}", s.name);
            assert!(s.description().len() > 40, "{}", s.name);
            assert!(s.body().starts_with('#'), "{}", s.name);
        }
    }

    #[test]
    fn merge_keeps_other_servers() {
        let dir = std::env::temp_dir().join(format!("mb-agents-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap_or_default();
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#,
        )
        .unwrap_or_default();
        let r = merge_json(&path, "mcpServers", server_entry("/bin/mb"), false);
        assert!(r.is_ok(), "{r:?}");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
            .unwrap_or_default();
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
        assert_eq!(doc["mcpServers"]["mockingbird"]["args"][0], "mcp");
        assert_eq!(doc["theme"], "dark");
        std::fs::remove_dir_all(&dir).ok();
    }
}
