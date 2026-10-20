#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The agent toolkit: the plugin packaging stays consistent, and the
//! onboarding commands, `agent install` and the MCP server work as a binary
//! (the last against a background server started with `server start`).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use mockingbird::agents::SKILLS;
use serde_json::{Value, json};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json_file(rel: &str) -> Value {
    serde_json::from_str(&read(&repo().join(rel))).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn fixture() -> PathBuf {
    repo().join("fixtures/openapi/orders-3.0.yaml")
}

fn bin() -> std::process::Command {
    std::process::Command::cargo_bin("mockingbird").unwrap()
}

fn run_json(dir: &Path, args: &[&str]) -> Value {
    let out = bin().args(args).current_dir(dir).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{args:?}: not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn plugin_manifests_agree() {
    let version = env!("CARGO_PKG_VERSION");
    for rel in [
        "plugins/mockingbird/.claude-plugin/plugin.json",
        "plugins/mockingbird/plugin.json",
        "plugins/mockingbird/.cursor-plugin/plugin.json",
        "gemini-extension.json",
    ] {
        let doc = json_file(rel);
        assert_eq!(doc["name"], "mockingbird", "{rel}");
        assert_eq!(doc["version"], version, "{rel}");
    }
    for rel in [
        "plugins/mockingbird/.mcp.json",
        "plugins/mockingbird/mcp.json",
        "powers/mockingbird/mcp.json",
    ] {
        let doc = json_file(rel);
        assert!(
            doc["mcpServers"]["mockingbird"]["command"].is_string(),
            "{rel}"
        );
    }
    // Every marketplace points at the plugin directory.
    let claude = json_file(".claude-plugin/marketplace.json");
    assert_eq!(claude["plugins"][0]["source"], "./plugins/mockingbird");
    let codex = json_file(".agents/plugins/marketplace.json");
    assert_eq!(
        codex["plugins"][0]["source"]["path"],
        "./plugins/mockingbird"
    );
    let cursor = json_file(".cursor-plugin/marketplace.json");
    let root = cursor["metadata"]["pluginRoot"].as_str().unwrap();
    let source = cursor["plugins"][0]["source"].as_str().unwrap();
    assert!(
        repo()
            .join(root)
            .join(source)
            .join(".cursor-plugin/plugin.json")
            .exists()
    );
    let gemini = json_file("gemini-extension.json");
    assert!(
        repo()
            .join(gemini["contextFileName"].as_str().unwrap())
            .exists()
    );
    // The Claude Code launcher and its bundled installer.
    let launcher = repo().join("plugins/mockingbird/bin/mockingbird-mcp");
    assert!(read(&launcher).contains("mcp"));
    assert_eq!(
        read(&repo().join("install.sh")),
        read(&repo().join("plugins/mockingbird/bin/install.sh")),
        "plugins/mockingbird/bin/install.sh must be a copy of install.sh"
    );
    #[cfg(unix)]
    for script in [
        "install.sh",
        "plugins/mockingbird/bin/mockingbird-mcp",
        "plugins/mockingbird/bin/install.sh",
    ] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(repo().join(script))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{script} is not executable");
    }
    let power = read(&repo().join("powers/mockingbird/POWER.md"));
    assert!(power.starts_with("---\nname: \"mockingbird\""));
}

#[test]
fn every_skill_directory_is_embedded() {
    let dir = repo().join("plugins/mockingbird/skills");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut embedded: Vec<String> = SKILLS.iter().map(|s| s.name.to_string()).collect();
    embedded.sort();
    assert_eq!(on_disk, embedded);
}

#[cfg(unix)]
#[test]
fn install_script_is_valid_sh() {
    let out = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo().join("install.sh"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn init_writes_a_loadable_config() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    let out = run_json(
        dir.path(),
        &[
            "init",
            "--provider",
            "cloudflare-ai-gateway",
            "--account-id",
            "acc",
            "--contract",
            "orders.yaml",
        ],
    );
    assert_eq!(out["profile"], "cloudflare-ai-gateway");
    assert_eq!(out["secrets"][0]["ref"], "env:CF_AIG_API_KEY");
    let cfg = mockingbird::file_config::load(&dir.path().join("mockingbird.yaml")).unwrap();
    let p = &cfg.llm.profiles["cloudflare-ai-gateway"];
    assert_eq!(
        p.base_url,
        "https://gateway.ai.cloudflare.com/v1/acc/default/compat"
    );
    assert_eq!(
        p.headers["cf-aig-authorization"],
        "Bearer ${env:CF_AIG_API_KEY}"
    );
    assert!(cfg.mocks.contains_key("orders"));
    assert_eq!(
        cfg.llm.default_profile.as_deref(),
        Some("cloudflare-ai-gateway")
    );

    // A second profile, made the default.
    let out = run_json(
        dir.path(),
        &["llm", "configure", "--provider", "openrouter", "--default"],
    );
    assert_eq!(out["default_profile"], true);
    let cfg = mockingbird::file_config::load(&dir.path().join("mockingbird.yaml")).unwrap();
    assert_eq!(cfg.llm.default_profile.as_deref(), Some("openrouter"));
    assert_eq!(cfg.llm.profiles.len(), 2);
    // The leading comment block survives the rewrite.
    assert!(read(&dir.path().join("mockingbird.yaml")).starts_with("# Mockingbird configuration"));

    // init refuses to overwrite.
    let out = bin()
        .args(["init", "--offline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn providers_lists_every_preset() {
    let out = run_json(Path::new("."), &["llm", "providers"]);
    let ids: Vec<&str> = out["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    for id in [
        "openrouter",
        "google-ai-studio",
        "cloudflare-ai-gateway",
        "custom",
    ] {
        assert!(ids.contains(&id), "{id}");
    }
}

#[test]
fn agent_install_writes_mcp_config_and_skills() {
    let dir = tempdir();
    std::fs::create_dir_all(dir.path().join(".cursor")).unwrap();
    std::fs::write(
        dir.path().join(".cursor/mcp.json"),
        r#"{"mcpServers":{"other":{"command":"x"}}}"#,
    )
    .unwrap();
    run_json(dir.path(), &["agent", "install", "cursor"]);
    let mcp: Value = serde_json::from_str(&read(&dir.path().join(".cursor/mcp.json"))).unwrap();
    assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
    assert_eq!(mcp["mcpServers"]["mockingbird"]["args"], json!(["mcp"]));
    for skill in SKILLS {
        let path = dir
            .path()
            .join(".cursor/skills")
            .join(skill.name)
            .join("SKILL.md");
        assert_eq!(read(&path), skill.text);
    }

    run_json(dir.path(), &["agent", "install", "codex", "--no-skills"]);
    let toml = read(&dir.path().join(".codex/config.toml"));
    assert!(toml.contains("[mcp_servers.mockingbird]") && toml.contains(r#"args = ["mcp"]"#));
    // Idempotent: a second install does not add a second table.
    run_json(dir.path(), &["agent", "install", "codex", "--no-skills"]);
    assert_eq!(
        read(&dir.path().join(".codex/config.toml"))
            .matches("[mcp_servers.mockingbird]")
            .count(),
        1
    );

    run_json(dir.path(), &["agent", "install", "vscode", "--no-skills"]);
    let vscode: Value = serde_json::from_str(&read(&dir.path().join(".vscode/mcp.json"))).unwrap();
    assert_eq!(vscode["servers"]["mockingbird"]["type"], "stdio");

    run_json(dir.path(), &["agent", "install", "kiro", "--no-mcp"]);
    let steering = read(&dir.path().join(".kiro/steering/mockingbird-setup.md"));
    assert!(steering.starts_with("---\ninclusion: manual\n---"));
}

/// A `mockingbird mcp` child speaking newline-delimited JSON-RPC.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl McpClient {
    fn start(dir: &Path, admin_url: &str) -> Self {
        let mut child = bin()
            .args(["--admin-url", admin_url, "mcp"])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["id"], id);
        resp
    }

    fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.rpc("tools/call", json!({ "name": name, "arguments": args }));
        let result = r["result"].clone();
        assert_eq!(
            result["isError"], false,
            "{name}: {}",
            result["content"][0]["text"]
        );
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn mcp_drives_a_background_server() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    run_json(
        dir.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let (port, admin, grpc) = (free_port(), free_port(), free_port());
    let admin_url = format!("http://127.0.0.1:{admin}");
    let mut mcp = McpClient::start(dir.path(), &admin_url);

    let init = mcp.rpc(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "mockingbird");
    mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let tools = mcp.rpc("tools/list", json!({}));
    assert!(tools["result"]["tools"].as_array().unwrap().len() > 30);

    let started = mcp.tool(
        "start_server",
        json!({ "config": "mockingbird.yaml", "port": port, "admin_port": admin, "grpc_port": grpc }),
    );
    assert_eq!(started["ready"], true, "{started}");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mocks = mcp.tool("list_mocks", json!({}));
        assert_eq!(mocks[0]["name"], "orders");

        let created = mcp.tool(
            "call_mock",
            json!({ "method": "POST", "path": "/orders/orders", "body": { "customerId": "c1", "total": 10 }, "session": "t" }),
        );
        assert_eq!(created["status"], 201, "{created}");
        assert_eq!(created["headers"]["x-mockingbird-source"], "fallback");
        let id = created["body"]["id"].as_str().unwrap().to_string();
        let got = mcp.tool(
            "call_mock",
            json!({ "path": format!("/orders/orders/{id}"), "session": "t" }),
        );
        assert_eq!(got["body"]["id"], id.as_str());

        let log = mcp.tool(
            "list_requests",
            json!({ "mock": "orders", "session": "t", "limit": 5 }),
        );
        assert_eq!(log.as_array().unwrap().len(), 2);

        let imported = mcp.tool(
            "import_contract",
            json!({ "mock": "hello", "content": "type Query { hello: String }\n", "filename": "schema.graphql" }),
        );
        assert_eq!(imported["status"], "active", "{imported}");
        let answer = mcp.tool(
            "call_mock",
            json!({ "method": "POST", "path": "/hello/graphql", "body": { "query": "{ hello }" } }),
        );
        assert!(answer["body"]["data"]["hello"].is_string(), "{answer}");

        let doctor = mcp.tool("doctor", json!({}));
        assert_eq!(doctor["admin"]["reachable"], true);
    }));
    let stopped = mcp.tool("stop_server", json!({}));
    assert_eq!(stopped["stopped"], true);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
