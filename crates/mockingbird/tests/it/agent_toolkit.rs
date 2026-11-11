//! The agent toolkit: the plugin packaging stays consistent, and the
//! onboarding commands, `agent install` and the MCP server work as a binary
//! (the last against a background server started with `server start`).
#![cfg(test)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use crate::support::{free_port, logged_admin, on_free_ports, read, repo, spawn_serve};
use assert_cmd::cargo::CommandCargoExt;
use mockingbird::agents::SKILLS;
use serde_json::{Value, json};

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

// Unix: Claude Desktop's config lives under HOME there (APPDATA on Windows).
#[cfg(unix)]
#[test]
fn agent_install_passes_key_names_not_values() {
    let dir = tempdir();
    run_json(dir.path(), &["init", "--provider", "openrouter"]);
    let run = |dir: &Path, args: &[&str]| -> Value {
        let out = bin()
            .args(args)
            .current_dir(dir)
            .env("HOME", dir)
            .env("OPENROUTER_API_KEY", "sk-or-secret-value")
            .env("MB_NO_SHELL_ENV", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let out = run(dir.path(), &["agent", "install", "cursor", "--no-skills"]);
    assert_eq!(out["passthrough"], json!(["OPENROUTER_API_KEY"]), "{out}");
    let cursor: Value = serde_json::from_str(&read(&dir.path().join(".cursor/mcp.json"))).unwrap();
    assert_eq!(
        cursor["mcpServers"]["mockingbird"]["env"]["OPENROUTER_API_KEY"],
        "${env:OPENROUTER_API_KEY}"
    );
    run(dir.path(), &["agent", "install", "codex", "--no-skills"]);
    assert!(
        read(&dir.path().join(".codex/config.toml"))
            .contains("env_vars = [\"OPENROUTER_API_KEY\"]")
    );
    run(
        dir.path(),
        &[
            "agent",
            "install",
            "claude-code",
            "--no-skills",
            "--pass-env",
            "EXTRA_KEY",
        ],
    );
    let claude: Value = serde_json::from_str(&read(&dir.path().join(".mcp.json"))).unwrap();
    assert_eq!(
        claude["mcpServers"]["mockingbird"]["env"],
        json!({ "EXTRA_KEY": "${EXTRA_KEY:-}", "OPENROUTER_API_KEY": "${OPENROUTER_API_KEY:-}" })
    );
    let out = run(
        dir.path(),
        &["agent", "install", "claude-desktop", "--no-skills"],
    );
    assert!(
        out["passthrough_unsupported"]
            .as_str()
            .unwrap()
            .contains("keys setup"),
        "{out}"
    );
    // MOCKINGBIRD-STUB-11: Claude Code reads variables, but not through a
    // printed `claude mcp add` line; the message says so.
    let out = run(
        dir.path(),
        &[
            "agent",
            "install",
            "claude-code",
            "--scope",
            "user",
            "--no-skills",
        ],
    );
    let msg = out["passthrough_unsupported"].as_str().unwrap();
    assert!(
        msg.contains("--scope project") && msg.contains("keys setup"),
        "{out}"
    );
    assert!(!msg.contains("cannot read variables"), "{msg}");
    let bad = bin()
        .args([
            "agent",
            "install",
            "cursor",
            "--no-skills",
            "--pass-env",
            "not a name",
        ])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(!bad.status.success());
    for f in [".cursor/mcp.json", ".codex/config.toml", ".mcp.json"] {
        assert!(
            !read(&dir.path().join(f)).contains("sk-or-secret-value"),
            "{f}"
        );
    }
    let desktop = mockingbird_desktop_config(dir.path());
    assert!(!read(&desktop).contains("sk-or-secret-value"));
    assert!(!read(&desktop).contains("\"env\""));

    let bare = tempdir();
    let out = run(bare.path(), &["agent", "install", "cursor", "--no-skills"]);
    assert!(out.get("passthrough").is_none(), "{out}");
    let cursor: Value = serde_json::from_str(&read(&bare.path().join(".cursor/mcp.json"))).unwrap();
    assert!(cursor["mcpServers"]["mockingbird"].get("env").is_none());
}

/// Claude Desktop's config under `home`.
#[cfg(unix)]
fn mockingbird_desktop_config(home: &Path) -> PathBuf {
    match std::env::consts::OS {
        "macos" => home.join("Library/Application Support/Claude/claude_desktop_config.json"),
        _ => home.join(".config/Claude/claude_desktop_config.json"),
    }
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
        Self::start_with(dir, admin_url, mcp_env)
    }

    /// Starts the server after `setup` adjusts its command (environment).
    fn start_with(
        dir: &Path,
        admin_url: &str,
        setup: impl FnOnce(&mut std::process::Command),
    ) -> Self {
        Self::spawn(dir, &["--admin-url", admin_url, "mcp"], setup)
    }

    /// Starts `mockingbird <args>` in `dir` after `setup`.
    fn spawn(dir: &Path, args: &[&str], setup: impl FnOnce(&mut std::process::Command)) -> Self {
        let mut cmd = bin();
        setup(&mut cmd);
        let mut child = cmd
            .args(args)
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

    /// A tool call that must fail; returns its error text.
    fn tool_error(&mut self, name: &str, args: Value) -> String {
        let r = self.rpc("tools/call", json!({ "name": name, "arguments": args }));
        assert_eq!(r["result"]["isError"], true, "{name} succeeded: {r}");
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
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

/// The environment [`McpClient::start`] gives the MCP server.
fn mcp_env(cmd: &mut std::process::Command) {
    cmd.env("MB_NO_SHELL_ENV", "1");
}

/// Starts an MCP client in `cwd` (`setup` adjusts its command) and through
/// it a background server with `args` on fresh ports, again on other ports
/// while one was taken before the server bound it. `prepare` runs before
/// each attempt. Returns the client, its admin URL and the answer of
/// `start_server`.
fn start_server_on_free_ports(
    cwd: &Path,
    args: &Value,
    setup: impl Fn(&mut std::process::Command),
    mut prepare: impl FnMut(),
) -> (McpClient, String, Value) {
    on_free_ports(|| {
        prepare();
        let admin = free_port();
        let admin_url = format!("http://127.0.0.1:{admin}");
        let mut mcp = McpClient::start_with(cwd, &admin_url, &setup);
        let mut args = args.clone();
        args["port"] = json!(free_port());
        args["admin_port"] = json!(admin);
        args["grpc_port"] = json!(free_port());
        let r = mcp.rpc(
            "tools/call",
            json!({ "name": "start_server", "arguments": args }),
        );
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        if r["result"]["isError"] == true {
            return Err(text);
        }
        let started: Value = serde_json::from_str(&text).unwrap();
        // Ready on the admin port: confirm it was this server that answered.
        let log = cwd.join(started["log"].as_str().unwrap());
        logged_admin(&log, admin).map(|()| (mcp, admin_url, started))
    })
}

#[test]
fn mcp_drives_a_background_server() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    run_json(
        dir.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let (mut mcp, _, started) = start_server_on_free_ports(
        dir.path(),
        &json!({ "config": "mockingbird.yaml" }),
        mcp_env,
        || {},
    );
    assert_eq!(started["ready"], true, "{started}");

    let init = mcp.rpc(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "mockingbird");
    mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let tools = mcp.rpc("tools/list", json!({}));
    assert!(tools["result"]["tools"].as_array().unwrap().len() > 30);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mocks = mcp.tool("list_mocks", json!({}));
        assert_eq!(mocks[0]["name"], "orders");

        let created = mcp.tool(
            "call_mock",
            json!({ "method": "POST", "path": "/orders/orders", "body": { "customerId": "c1", "total": 10 }, "session": "t" }),
        );
        assert_eq!(created["status"], 201, "{created}");
        assert!(created.get("expr_failed_unknown").is_none(), "{created}");
        // The state-first executor creates the order, with no LLM call.
        assert_eq!(created["headers"]["x-mockingbird-source"], "state");
        let id = created["body"]["id"].as_str().unwrap().to_string();
        let got = mcp.tool(
            "call_mock",
            json!({ "path": format!("/orders/orders/{id}"), "session": "t" }),
        );
        assert_eq!(got["body"]["id"], id.as_str());

        // The request log is written by a background writer: wait for both.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let log = loop {
            let log = mcp.tool(
                "list_requests",
                json!({ "mock": "orders", "session": "t", "limit": 5 }),
            );
            if log.as_array().unwrap().len() >= 2 || std::time::Instant::now() >= deadline {
                break log;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(log.as_array().unwrap().len(), 2, "{log}");
        assert!(log[0].get("response").is_none(), "{log}");
        assert!(log[0]["response_bytes"].as_u64().unwrap() > 0, "{log}");
        let one = mcp.tool(
            "get_request",
            json!({ "mock": "orders", "id": log[0]["id"] }),
        );
        assert!(one["response"].is_object(), "{one}");
        let deliveries = mcp.tool(
            "list_webhook_deliveries",
            json!({ "mock": "orders", "limit": 5 }),
        );
        assert_eq!(deliveries, json!([]), "{deliveries}");

        // An expression that does not compile is refused at its pointer, quoting
        // where it fails; one that fails when it runs is named in call_mock's answer.
        let patched = mcp.tool(
            "patch_world",
            json!({ "mock": "orders", "author": "agent", "patch": { "operations": { "createOrder": { "set": {
                "status": "request.body.customerId +",
                "total": "request.body.customerId * 2"
            } } } } }),
        );
        let refused = patched["refused"].as_array().expect("refused");
        assert_eq!(refused.len(), 1, "{patched}");
        assert_eq!(
            refused[0]["path"], "/operations/createOrder/set/status",
            "{patched}"
        );
        assert!(
            refused[0]["message"]
                .as_str()
                .unwrap()
                .contains("line 1, column"),
            "{patched}"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let failed = loop {
            let answer = mcp.tool(
                "call_mock",
                json!({ "method": "POST", "path": "/orders/orders", "body": { "customerId": "c2", "total": 3 }, "session": "t" }),
            );
            if answer.get("expr_failed").is_some() || std::time::Instant::now() >= deadline {
                break answer;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(
            failed["expr_failed"][0]["pointer"], "/operations/createOrder/set/total",
            "{failed}"
        );
        let clean = mcp.tool(
            "call_mock",
            json!({ "path": "/orders/orders", "session": "t" }),
        );
        // The call's record was found and lists no failure.
        assert!(clean.get("expr_failed").is_none(), "{clean}");
        assert!(clean.get("expr_failed_unknown").is_none(), "{clean}");

        let imported = mcp.tool(
            "import_contract",
            json!({ "mock": "hello", "content": "type Query { hello: String }\n", "filename": "schema.graphql" }),
        );
        assert_eq!(imported["status"], "active", "{imported}");
        // An import that makes a version says what to do next: review the world.
        let steps = imported["next_steps"].as_array().expect("next_steps");
        assert!(
            steps[0].as_str().unwrap().contains("mockingbird-world"),
            "{imported}"
        );
        let gaps = imported["world_gaps"].as_array().expect("world_gaps");
        assert_eq!(
            imported["world_gaps_total"].as_u64(),
            Some(gaps.len() as u64),
            "{imported}"
        );
        // The import is stored at once; the data plane mounts the new mock
        // when its registry watcher has rebuilt it, so wait for that.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let answer = loop {
            let answer = mcp.tool(
                "call_mock",
                json!({ "method": "POST", "path": "/hello/graphql", "body": { "query": "{ hello }" } }),
            );
            let unmounted = answer["status"] == 404
                && answer["body"]["detail"]
                    .as_str()
                    .is_some_and(|d| d.starts_with("no mock is mounted"));
            if !unmounted || std::time::Instant::now() >= deadline {
                break answer;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(answer["body"]["data"]["hello"].is_string(), "{answer}");

        // The faker seed through create_mock and update_mock.
        let seeded = mcp.tool("create_mock", json!({ "name": "seeded", "faker_seed": 5 }));
        assert_eq!(seeded["faker_seed"], 5, "{seeded}");
        let error = mcp.tool_error(
            "create_mock",
            json!({ "name": "negative", "faker_seed": -5 }),
        );
        assert!(error.contains("faker_seed"), "{error}");
        let cleared = mcp.tool(
            "update_mock",
            json!({ "name": "seeded", "patch": { "faker_seed": null } }),
        );
        assert!(cleared.get("faker_seed").is_none(), "{cleared}");

        let doctor = mcp.tool("doctor", json!({}));
        assert_eq!(doctor["admin"]["reachable"], true);
    }));
    let stopping = std::time::Instant::now();
    let stopped = mcp.tool("stop_server", json!({}));
    assert_eq!(stopped["stopped"], true);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    // The MCP server started this server, so it reaps it: SIGTERM is enough,
    // and the exited server is not mistaken for a live one (a zombie) until
    // the 10 s SIGKILL fallback.
    assert_eq!(stopped["forced"], false, "{stopped}");
    assert!(
        stopping.elapsed() < std::time::Duration::from_secs(5),
        "stop_server took {:?}",
        stopping.elapsed()
    );
    let status = mcp.tool("server_status", json!({}));
    assert_eq!(status["running"], false, "{status}");
}

#[test]
fn server_tools_find_the_server_from_another_folder() {
    let project = tempdir();
    std::fs::copy(fixture(), project.path().join("orders.yaml")).unwrap();
    run_json(
        project.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let config = project.path().join("mockingbird.yaml");
    let config = config.to_str().unwrap();
    let other = tempdir();
    let (mut mcp, admin_url, started) =
        start_server_on_free_ports(other.path(), &json!({ "config": config }), mcp_env, || {});
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(started["ready"], true, "{started}");
        let logs = mcp.tool("server_logs", json!({}));
        let log = logs["log"].as_str().unwrap();
        assert!(!log.is_empty(), "{logs}");
        assert!(!log.contains('\x1b'), "{log}");
        drop(mcp);
        let mut mcp = McpClient::start(other.path(), &admin_url);
        let status = mcp.tool("server_status", json!({ "config": config }));
        assert_eq!(status["running"], true, "{status}");
        let stopped = mcp.tool("stop_server", json!({ "config": config }));
        assert_eq!(stopped["stopped"], true, "{stopped}");
    }));
    if result.is_err() {
        let mut cleanup = McpClient::start(other.path(), &admin_url);
        let _ = cleanup.rpc(
            "tools/call",
            json!({ "name": "stop_server", "arguments": { "config": config } }),
        );
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(project.path().join(".mockingbird/server.json").exists());
    assert!(!other.path().join(".mockingbird").exists());
}

#[test]
fn start_server_with_the_proxy_returns_its_variables() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    run_json(
        dir.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let ca = dir.path().join("ca");
    let config = read(&dir.path().join("mockingbird.yaml"));
    let mut proxy = 0;
    let (mut mcp, _, started) = start_server_on_free_ports(
        dir.path(),
        &json!({ "config": "mockingbird.yaml", "proxy": true }),
        |cmd| {
            cmd.env("MB_NO_SHELL_ENV", "1").env("MB_CA_DIR", &ca);
        },
        || {
            proxy = free_port();
            std::fs::write(
                dir.path().join("mockingbird.yaml"),
                format!("{config}\nproxy:\n  port: {proxy}\n"),
            )
            .unwrap();
        },
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(started["ready"], true, "{started}");
        assert_eq!(
            started["proxy"],
            format!("http://127.0.0.1:{proxy}"),
            "{started}"
        );
        assert_eq!(
            started["proxy_env"]["vars"]["HTTPS_PROXY"],
            format!("http://127.0.0.1:{proxy}")
        );
        assert!(ca.join("ca.pem").exists());
        let env = mcp.tool("proxy_env", json!({ "config": "mockingbird.yaml" }));
        assert_eq!(env["vars"], started["proxy_env"]["vars"]);
        // The proxy answers: a plain request for an unmocked host through it.
        std::net::TcpStream::connect(("127.0.0.1", proxy)).unwrap();
    }));
    let stopped = mcp.tool("stop_server", json!({}));
    assert_eq!(stopped["stopped"], true);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn skills_recommend_proxy_mode() {
    for name in ["mockingbird-mock-api", "mockingbird-testing"] {
        let skill = SKILLS.iter().find(|s| s.name == name).unwrap();
        assert!(skill.text.contains("proxy_env"), "{name}");
        assert!(skill.text.contains("mockingbird proxy env"), "{name}");
    }
    let entry = SKILLS.iter().find(|s| s.name == "mockingbird").unwrap();
    for tool in [
        "get_draft",
        "edit_draft",
        "approve_draft",
        "edit_contract",
        "get_request",
        "proxy_env",
    ] {
        assert!(entry.text.contains(tool), "{tool}");
    }
}

#[cfg(unix)]
#[test]
fn doctor_reports_the_key_store() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir();
    let home = dir.path().join("home");
    let store = home.join(".mockingbird/secrets");
    let key = mockingbird::keys::store::write_secret(&store, "openrouter", "sk-or-test").unwrap();
    let doctor = |dir: &Path| -> Value {
        let out = bin()
            .args(["doctor"])
            .current_dir(dir)
            .env("HOME", &home)
            .env("MB_ADMIN_URL", "http://127.0.0.1:9")
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let report = doctor(dir.path());
    assert_eq!(
        report["key_store"]["keys"][0]["name"], "openrouter",
        "{report}"
    );
    assert_eq!(report["key_store"]["keys"][0]["private"], true);
    assert!(!report.to_string().contains("sk-or-test"));
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let report = doctor(dir.path());
    assert!(
        report["problems"].to_string().contains("chmod 600"),
        "{report}"
    );
}

/// POSTs `key=<value>` to the key page at `url` (a plain TCP client: the
/// test needs no async runtime). Returns the status line.
fn post_key(url: &str, key: &str) -> String {
    use std::io::Read;
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let body = format!("key={key}");
    let mut conn = std::net::TcpStream::connect(host).unwrap();
    write!(
        conn,
        "POST /{path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).unwrap();
    assert!(!response.contains(key), "the page echoed the key");
    response.lines().next().unwrap_or_default().to_string()
}

#[cfg(unix)]
#[test]
fn open_key_setup_never_returns_the_key() {
    let dir = tempdir();
    run_json(dir.path(), &["init", "--provider", "openrouter"]);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut mcp = McpClient::start_with(dir.path(), "http://127.0.0.1:9", |cmd| {
        cmd.env("MB_NO_SHELL_ENV", "1")
            .env("HOME", &home)
            .env_remove("OPENROUTER_API_KEY");
    });
    let r = mcp.tool("open_key_setup", json!({ "open": false }));
    let url = r["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "{r}");
    assert!(r["saved"].is_null(), "{r}");
    assert_eq!(r["profile"], "openrouter", "{r}");
    assert!(
        r["config"].as_str().unwrap().ends_with("mockingbird.yaml"),
        "{r}"
    );
    let status = post_key(&url, "sk-or-v1-from-browser");
    assert!(status.contains("200"), "{status}");
    let mut doctor = Value::Null;
    for _ in 0..50 {
        doctor = mcp.tool("doctor", json!({}));
        if doctor["key_store"]["keys"][0]["name"] == "openrouter" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        doctor["key_store"]["keys"][0]["name"], "openrouter",
        "{doctor}"
    );
    assert_eq!(doctor["key_store"]["keys"][0]["private"], true);
    assert_eq!(
        doctor["config"]["profiles"][0]["secrets"][0]["set"], true,
        "{doctor}"
    );
    assert!(!doctor.to_string().contains("sk-or-v1-from-browser"));
    assert!(!r.to_string().contains("sk-or-v1-from-browser"));
    let config = read(&dir.path().join("mockingbird.yaml"));
    assert!(!config.contains("sk-or-v1-from-browser"));
    assert!(config.contains("file:"), "{config}");
    assert_eq!(
        read(&home.join(".mockingbird/secrets/openrouter")),
        "sk-or-v1-from-browser"
    );
}

#[test]
fn keys_setup_needs_a_name_without_a_config() {
    let dir = tempdir();
    let out = bin()
        .args(["keys", "setup", "--no-open", "-c", "none.yaml"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("mockingbird keys setup <name>"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn init_writes_server_ports() {
    let dir = tempdir();
    run_json(
        dir.path(),
        &[
            "init",
            "--offline",
            "--port",
            "18080",
            "--admin-port",
            "19090",
            "--grpc-port",
            "15051",
        ],
    );
    let cfg = mockingbird::file_config::load(&dir.path().join("mockingbird.yaml")).unwrap();
    assert_eq!(cfg.server.port, 18080);
    assert_eq!(cfg.server.admin_port, 19090);
    assert_eq!(cfg.server.grpc_port, 15051);
}

#[test]
fn llm_test_defaults_to_the_config_default_profile() {
    let dir = tempdir();
    run_json(
        dir.path(),
        &[
            "init",
            "--provider",
            "custom",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--no-api-key",
            "--timeout",
            "1s",
        ],
    );
    // No --provider, --config or --profile: ./mockingbird.yaml's default.
    let out = bin()
        .args(["llm", "test"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(report["base_url"], "http://127.0.0.1:9/v1");
    assert_eq!(report["ok"], false);
}

/// A login shell that exports a key, then runs the `-c` command it was given.
///
/// A child `sh` writes the script, not this process: a process that other
/// tests in this binary fork while the script is open for writing would
/// inherit the descriptor, and executing the script then fails with
/// ETXTBSY (the import then silently finds nothing).
#[cfg(unix)]
fn fake_login_shell(dir: &Path) -> PathBuf {
    let path = dir.join("fake-shell.sh");
    let script = "#!/bin/sh\nexport MB_TEST_SHELL_ONLY_API_KEY=from-the-shell\n\
         while [ \"$1\" != \"-c\" ] && [ $# -gt 0 ]; do shift; done\nshift\nexec /bin/sh -c \"$1\"\n";
    let status = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"",
            "sh",
        ])
        .arg(script)
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "writing {}", path.display());
    path
}

#[cfg(unix)]
#[test]
fn mcp_imports_missing_variables_from_the_login_shell() {
    let dir = tempdir();
    run_json(
        dir.path(),
        &[
            "init",
            "--provider",
            "custom",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--api-key-env",
            "MB_TEST_SHELL_ONLY_API_KEY",
        ],
    );
    let shell = fake_login_shell(dir.path());
    let mut mcp = McpClient::start_with(dir.path(), "http://127.0.0.1:9", |cmd| {
        cmd.env("SHELL", &shell)
            .env_remove("MB_TEST_SHELL_ONLY_API_KEY");
    });
    let doctor = mcp.tool("doctor", json!({}));
    let secrets = &doctor["config"]["profiles"][0]["secrets"][0];
    assert_eq!(secrets["ref"], "env:MB_TEST_SHELL_ONLY_API_KEY");
    assert_eq!(secrets["set"], true, "{doctor}");
    assert_eq!(
        doctor["shell_env"]["imported"],
        json!(["MB_TEST_SHELL_ONLY_API_KEY"])
    );
    assert!(
        !doctor.to_string().contains("from-the-shell"),
        "doctor never shows values"
    );
}

#[cfg(unix)]
#[test]
fn an_empty_passed_through_variable_is_imported_from_the_login_shell() {
    let dir = tempdir();
    run_json(
        dir.path(),
        &[
            "init",
            "--provider",
            "custom",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--api-key-env",
            "MB_TEST_SHELL_ONLY_API_KEY",
        ],
    );
    let shell = fake_login_shell(dir.path());
    // What an agent's `${NAME:-}` passthrough hands over when it lacks the key.
    let mut mcp = McpClient::start_with(dir.path(), "http://127.0.0.1:9", |cmd| {
        cmd.env("SHELL", &shell)
            .env("MB_TEST_SHELL_ONLY_API_KEY", "");
    });
    let doctor = mcp.tool("doctor", json!({}));
    assert_eq!(
        doctor["config"]["profiles"][0]["secrets"][0]["set"], true,
        "{doctor}"
    );
}

#[cfg(unix)]
#[test]
fn start_server_refuses_a_profile_whose_key_is_missing() {
    let dir = tempdir();
    run_json(
        dir.path(),
        &[
            "init",
            "--provider",
            "custom",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--api-key-env",
            "MB_TEST_NOWHERE_API_KEY",
        ],
    );
    let admin = free_port();
    let mut mcp = McpClient::start_with(dir.path(), &format!("http://127.0.0.1:{admin}"), |cmd| {
        cmd.env("MB_NO_SHELL_ENV", "1")
            .env_remove("MB_TEST_NOWHERE_API_KEY");
    });
    let doctor = mcp.tool("doctor", json!({}));
    let problems = doctor["problems"].to_string();
    assert!(problems.contains("MB_TEST_NOWHERE_API_KEY"), "{problems}");
    assert!(problems.contains("MCP server"), "{problems}");
    let err = mcp.tool_error(
        "start_server",
        json!({ "config": "mockingbird.yaml", "port": free_port(), "admin_port": admin, "grpc_port": free_port() }),
    );
    assert!(err.contains("MB_TEST_NOWHERE_API_KEY"), "{err}");
    assert!(err.contains("offline"), "says how to start anyway: {err}");
}

/// Kills a child process when dropped.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The MCP server an agent starts in a project (no `MB_ADMIN_URL`) acts on
/// that project's server only. Another project's server on the admin port
/// this project's config names is refused and named, `start_server` says
/// who holds that port, and once it started the project's own server on
/// other ports every tool uses that one.
#[test]
fn mcp_acts_only_on_its_projects_server() {
    let other = tempdir();
    std::fs::copy(fixture(), other.path().join("orders.yaml")).unwrap();
    run_json(
        other.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let (child, bound) = spawn_serve(
        bin()
            .current_dir(other.path())
            .env_remove("MB_LLM_API_KEY")
            .args(["serve", "-c", "mockingbird.yaml", "--offline"])
            .args(["--port", "0", "--admin-port", "0", "--grpc-port", "0"]),
    );
    let _other_server = KillOnDrop(child);
    let foreign_url = bound.admin_url();
    let foreign_port = bound.admin.unwrap().port().to_string();
    let other_config = std::fs::canonicalize(other.path().join("mockingbird.yaml")).unwrap();
    let other_config = other_config.to_str().unwrap();

    // This project's config names the same admin port, as two default
    // configs would.
    let project = tempdir();
    std::fs::copy(fixture(), project.path().join("orders.yaml")).unwrap();
    // Data port 0: only the admin port can be busy at the first start.
    run_json(
        project.path(),
        &[
            "init",
            "--offline",
            "--contract",
            "orders.yaml",
            "--port",
            "0",
            "--admin-port",
            &foreign_port,
        ],
    );
    let mut mcp = McpClient::spawn(project.path(), &["mcp"], |cmd| {
        mcp_env(cmd);
        cmd.env_remove("MB_ADMIN_URL").env_remove("MB_ADMIN_PORT");
    });
    for (tool, args) in [
        ("list_mocks", json!({})),
        (
            "import_contract",
            json!({ "mock": "hello", "content": "type Query { hello: String }\n", "filename": "schema.graphql" }),
        ),
    ] {
        let error = mcp.tool_error(tool, args);
        assert!(
            error.contains("belongs to another project") && error.contains(other_config),
            "{tool}: {error}"
        );
    }
    let error = mcp.tool_error("start_server", json!({ "config": "mockingbird.yaml" }));
    assert!(
        error.contains("another project holds it") && error.contains(other_config),
        "{error}"
    );

    let started = on_free_ports(|| {
        let admin = free_port();
        let r = mcp.rpc(
            "tools/call",
            json!({ "name": "start_server", "arguments": {
                "config": "mockingbird.yaml",
                "port": free_port(),
                "admin_port": admin,
                "grpc_port": free_port(),
            } }),
        );
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        if r["result"]["isError"] == true {
            return Err(text);
        }
        let started: Value = serde_json::from_str(&text).unwrap();
        let log = project.path().join(started["log"].as_str().unwrap());
        logged_admin(&log, admin).map(|()| started)
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(started["ready"], true, "{started}");
        let imported = mcp.tool(
            "import_contract",
            json!({ "mock": "hello", "content": "type Query { hello: String }\n", "filename": "schema.graphql" }),
        );
        assert_eq!(imported["status"], "active", "{imported}");
        let mocks = mcp.tool("list_mocks", json!({}));
        let names: Vec<&str> = mocks
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert!(names.contains(&"hello"), "{mocks}");
    }));
    let stopped = mcp.tool("stop_server", json!({}));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    // The other project's server never saw the import.
    let mut theirs = McpClient::start(other.path(), &foreign_url);
    let mocks = theirs.tool("list_mocks", json!({}));
    assert_eq!(mocks.as_array().unwrap().len(), 1, "{mocks}");
    assert_eq!(mocks[0]["name"], "orders", "{mocks}");
}

#[test]
fn cli_world_schema_prints_valid_json() {
    let dir = tempdir();
    let printed = run_json(dir.path(), &["world", "schema"]);
    assert_eq!(printed, json_file("crates/mb-mcm/schemas/world.v1.json"));
    assert!(printed["$schema"].is_string(), "{printed}");
    assert_eq!(printed["type"], "object", "{printed}");
}

/// A contract stricter than the real API (here `text` required) is loosened with
/// `edit_contract`, which activates the edit as a new version: the request it
/// refused is then served.
#[test]
fn mcp_edit_contract_loosens_the_active_contract() {
    let dir = tempdir();
    run_json(dir.path(), &["init", "--offline"]);
    let (mut mcp, _admin_url, started) = start_server_on_free_ports(
        dir.path(),
        &json!({ "config": "mockingbird.yaml" }),
        mcp_env,
        || {},
    );
    assert_eq!(started["ready"], true, "{started}");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let contract = json!({
            "openapi": "3.1.0",
            "info": {"title": "Hooks", "version": "1"},
            "paths": {"/hooks/{id}": {"post": {
                "operationId": "postHook",
                "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["text"],
                    "properties": {"text": {"type": "string"}}
                }}}},
                "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {
                    "type": "object", "properties": {"ok": {"type": "boolean"}}
                }}}}}
            }}}
        });
        let imported = mcp.tool(
            "import_contract",
            json!({"mock": "hooks", "content": contract.to_string(), "filename": "hooks.json"}),
        );
        assert_eq!(imported["version"], 1, "{imported}");
        let call = json!({"mock": "hooks", "method": "POST", "path": "/hooks/hooks/1",
                          "body": {"attachments": []}});
        // The data plane mounts a version when its registry watcher rebuilt the mock:
        // call until the answer has the status wanted, or 10 s passed.
        let call_until = |mcp: &mut McpClient, status: u64| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let answer = mcp.tool("call_mock", call.clone());
                if answer["status"] == status || std::time::Instant::now() >= deadline {
                    break answer;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let refused = call_until(&mut mcp, 400);
        assert_eq!(refused["status"], 400, "{refused}");

        let patch =
            json!({"requestBody": {"content": {"application/json": {"schema": {"required": []}}}}});
        let edited = mcp.tool(
            "edit_contract",
            json!({"mock": "hooks", "operation": "postHook", "patch": patch}),
        );
        assert_eq!(edited["version"], 2, "{edited}");
        assert_eq!(edited["status"], "active", "{edited}");
        let served = call_until(&mut mcp, 200);
        assert_eq!(served["status"], 200, "{served}");

        let error = mcp.tool_error(
            "edit_contract",
            json!({"mock": "hooks", "operation": "nope", "patch": {}}),
        );
        assert!(error.contains("nope"), "{error}");

        // A YAML source is edited as well; a source that is no OpenAPI is refused
        // with what to do instead.
        let yaml = "openapi: 3.1.0\ninfo: {title: Ys, version: '1'}\npaths:\n  /y:\n    get:\n      operationId: getY\n      responses: {'200': {description: ok}}\n";
        mcp.tool(
            "import_contract",
            json!({"mock": "y", "content": yaml, "filename": "y.yaml"}),
        );
        let edited = mcp.tool(
            "edit_contract",
            json!({"mock": "y", "operation": "getY", "patch": {"summary": "Get y"}}),
        );
        assert_eq!(edited["version"], 2, "{edited}");
        mcp.tool(
            "import_contract",
            json!({"mock": "gql", "content": "type Query { hello: String }\n", "filename": "schema.graphql"}),
        );
        let error = mcp.tool_error("edit_contract", json!({"mock": "gql", "patch": {}}));
        assert!(error.contains("not an OpenAPI document"), "{error}");
    }));
    let stopped = mcp.tool("stop_server", json!({}));
    assert_eq!(stopped["stopped"], true);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn mcp_get_world_matches_admin() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    run_json(
        dir.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let (mut mcp, admin_url, started) = start_server_on_free_ports(
        dir.path(),
        &json!({ "config": "mockingbird.yaml" }),
        mcp_env,
        || {},
    );
    assert_eq!(started["ready"], true, "{started}");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tools = mcp.rpc("tools/list", json!({}));
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "get_world")
            .cloned()
            .unwrap_or_else(|| panic!("no get_world tool: {tools}"));
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");

        // The format is served to MCP clients as `mockingbird world schema`
        // prints it.
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "get_world_schema")
            .cloned()
            .unwrap_or_else(|| panic!("no get_world_schema tool: {tools}"));
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        let schema = mcp.tool("get_world_schema", json!({}));
        assert_eq!(schema, json_file("crates/mb-mcm/schemas/world.v1.json"));
        assert_eq!(schema, run_json(dir.path(), &["world", "schema"]));

        let admin = run_json(
            dir.path(),
            &[
                "--admin-url",
                &admin_url,
                "api",
                "GET",
                "/workspaces/default/mocks/orders/contracts/active/world",
            ],
        );
        assert_eq!(admin["revision"], 0, "{admin}");
        assert!(admin["world"]["operations"].is_object(), "{admin}");
        let version = admin["version"].as_u64().unwrap();

        let world = mcp.tool("get_world", json!({ "mock": "orders" }));
        assert_eq!(world, admin);
        let world = mcp.tool("get_world", json!({ "mock": "orders", "version": version }));
        assert_eq!(world, admin);
        let error = mcp.tool_error("get_world", json!({ "mock": "orders", "version": 99 }));
        assert!(error.contains("99"), "{error}");

        // The CLI answers the same JSON, and a human summary with the gaps
        // first.
        let shown = run_json(
            dir.path(),
            &[
                "--admin-url",
                &admin_url,
                "world",
                "show",
                "orders",
                "--json",
            ],
        );
        assert_eq!(shown, admin);
        let shown = run_json(
            dir.path(),
            &[
                "--admin-url",
                &admin_url,
                "world",
                "show",
                "orders",
                "--version",
                &version.to_string(),
                "--json",
            ],
        );
        assert_eq!(shown, admin);
        let out = bin()
            .args(["--admin-url", &admin_url, "world", "show", "orders"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        let gaps = text.find("Gaps").unwrap_or_else(|| panic!("{text}"));
        let operations = text.find("Operations").unwrap_or_else(|| panic!("{text}"));
        assert!(gaps < operations, "{text}");
        assert!(text.contains("revision 0"), "{text}");

        // --operation narrows to one operation, as the MCP tool's
        // `operations` and the Admin API's `?operation=` do.
        let op = admin["world"]["operations"]
            .as_object()
            .and_then(|o| o.keys().next().cloned())
            .unwrap_or_else(|| panic!("{admin}"));
        let narrowed = run_json(
            dir.path(),
            &[
                "--admin-url",
                &admin_url,
                "world",
                "show",
                "orders",
                "--operation",
                &op,
                "--json",
            ],
        );
        let ops: Vec<&String> = narrowed["world"]["operations"]
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        assert_eq!(ops, [&op], "{narrowed}");
        assert!(
            narrowed["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["operation"] == op.as_str()),
            "{narrowed}"
        );
        let world = mcp.tool("get_world", json!({ "mock": "orders", "operations": [op] }));
        assert_eq!(world, narrowed);
        // An unknown name answers nothing, with a warning.
        let out = bin()
            .args([
                "--admin-url",
                &admin_url,
                "world",
                "show",
                "orders",
                "--operation",
                "noSuchOp",
                "--type",
                "NoSuchType",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("warning: no operation noSuchOp"), "{text}");
        assert!(text.contains("warning: no type NoSuchType"), "{text}");
        assert!(text.contains("Operations (0)"), "{text}");
    }));
    mcp.tool("stop_server", json!({}));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn mcp_and_cli_write_world_revisions() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    run_json(
        dir.path(),
        &["init", "--offline", "--contract", "orders.yaml"],
    );
    let (mut mcp, admin_url, started) = start_server_on_free_ports(
        dir.path(),
        &json!({ "config": "mockingbird.yaml" }),
        mcp_env,
        || {},
    );
    assert_eq!(started["ready"], true, "{started}");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tools = mcp.rpc("tools/list", json!({}));
        for (name, read_only) in [
            ("put_world", false),
            ("patch_world", false),
            ("list_world_revisions", true),
            ("get_world_revision", true),
            ("restore_world_revision", false),
            ("diff_world", true),
        ] {
            let tool = tools["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("no {name} tool: {tools}"));
            assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{tool}");
        }
        let world = mcp.tool("get_world", json!({ "mock": "orders" }));
        let (op, desc) = world["world"]["operations"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, d)| d["kind"].is_string())
            .map(|(op, d)| (op.clone(), d.clone()))
            .unwrap_or_else(|| panic!("{world}"));
        let kind = desc["kind"].clone();

        // A patch as the assistant: the valid entry applies, the unknown
        // key is refused.
        let patched = mcp.tool(
            "patch_world",
            json!({
                "mock": "orders",
                "patch": {"operations": {op.clone(): {"kind": kind.clone(), "bogus": 1}}},
                "author": "agent",
                "expect_revision": 0,
            }),
        );
        assert_eq!(patched["revision"], 1, "{patched}");
        assert_eq!(
            patched["applied"]["operations"][&op]["kind"], kind,
            "{patched}"
        );
        assert_eq!(patched["refused"][0]["code"], "unknown_key", "{patched}");
        let world = mcp.tool("get_world", json!({ "mock": "orders" }));
        assert_eq!(
            world["authors"][format!("/operations/{op}/kind")],
            "agent",
            "{world}"
        );
        // A stale expect_revision is a revision conflict.
        let error = mcp.tool_error(
            "put_world",
            json!({ "mock": "orders", "world": {}, "expect_revision": 0 }),
        );
        assert!(error.contains("revision_conflict"), "{error}");
        let put = mcp.tool("put_world", json!({ "mock": "orders", "world": {} }));
        assert_eq!(put["revision"], 2, "{put}");

        let revisions = mcp.tool("list_world_revisions", json!({ "mock": "orders" }));
        assert_eq!(revisions[0]["revision"], 2, "{revisions}");
        assert_eq!(revisions[1]["author"], "agent", "{revisions}");
        // One revision reads as the Admin API answers it, with its overlay.
        let revision = mcp.tool(
            "get_world_revision",
            json!({ "mock": "orders", "revision": 1 }),
        );
        assert_eq!(revision["revision"], 1, "{revision}");
        assert_eq!(revision["author"], "agent", "{revision}");
        assert_eq!(revision["document"], patched["applied"], "{revision}");
        let admin_revision = run_json(
            dir.path(),
            &[
                "--admin-url",
                &admin_url,
                "api",
                "GET",
                "/workspaces/default/mocks/orders/contracts/active/world/revisions/1",
            ],
        );
        assert_eq!(revision, admin_revision);
        let version = revision["version"].as_u64().unwrap();
        assert_eq!(
            mcp.tool(
                "get_world_revision",
                json!({ "mock": "orders", "version": version, "revision": 1 }),
            ),
            admin_revision
        );
        let error = mcp.tool_error(
            "get_world_revision",
            json!({ "mock": "orders", "revision": 99 }),
        );
        assert!(error.contains("99"), "{error}");
        let diff = mcp.tool("diff_world", json!({ "mock": "orders" }));
        assert_eq!(
            diff["removed"],
            json!([format!("/operations/{op}/kind")]),
            "{diff}"
        );
        let restored = mcp.tool(
            "restore_world_revision",
            json!({ "mock": "orders", "revision": 1 }),
        );
        assert_eq!(restored["revision"], 3, "{restored}");
        assert_eq!(restored["applied"], patched["applied"]);

        // The CLI writes the same way.
        let file = dir.path().join("world.json");
        std::fs::write(
            &file,
            json!({"operations": {op.clone(): {"kind": kind}}}).to_string(),
        )
        .unwrap();
        let admin = ["--admin-url", admin_url.as_str(), "world"];
        let put = run_json(
            dir.path(),
            &[
                &admin[..],
                &[
                    "put",
                    "orders",
                    "world.json",
                    "--author",
                    "user",
                    "--expect-revision",
                    "3",
                ],
            ]
            .concat(),
        );
        assert_eq!(put["revision"], 4, "{put}");
        std::fs::write(&file, json!({"operations": {op.clone(): null}}).to_string()).unwrap();
        let patch = run_json(
            dir.path(),
            &[&admin[..], &["patch", "orders", "world.json"]].concat(),
        );
        assert_eq!(patch["revision"], 5, "{patch}");
        let revisions = run_json(dir.path(), &[&admin[..], &["revisions", "orders"]].concat());
        assert_eq!(revisions.as_array().unwrap().len(), 5, "{revisions}");
        let revision = run_json(
            dir.path(),
            &[&admin[..], &["revision", "orders", "4"]].concat(),
        );
        assert_eq!(revision["revision"], 4, "{revision}");
        assert_eq!(revision["author"], revisions[1]["author"], "{revision}");
        assert!(
            revision["document"]["operations"][&op].is_object(),
            "{revision}"
        );
        let revision = run_json(
            dir.path(),
            &[
                &admin[..],
                &["revision", "orders", "5", "--version", &version.to_string()],
            ]
            .concat(),
        );
        assert_eq!(revision["revision"], 5, "{revision}");
        assert!(
            revision["document"]["operations"][&op].is_null(),
            "{revision}"
        );
        let diff = run_json(
            dir.path(),
            &[&admin[..], &["diff", "orders", "--from", "4", "--to", "5"]].concat(),
        );
        assert_eq!(
            diff["removed"],
            json!([format!("/operations/{op}/kind")]),
            "{diff}"
        );
        let restored = run_json(
            dir.path(),
            &[&admin[..], &["restore", "orders", "4"]].concat(),
        );
        assert_eq!(restored["revision"], 6, "{restored}");
        let out = bin()
            .args(
                [
                    &admin[..],
                    &["put", "orders", "world.json", "--expect-revision", "1"],
                ]
                .concat(),
            )
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("revision_conflict"), "{stderr}");
    }));
    mcp.tool("stop_server", json!({}));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Started from a config, `start_server` answers each mock's world gaps as an
/// import does, and names mocks that answer for one host without a common
/// `shares` group.
#[test]
fn start_server_reviews_the_worlds_and_flags_same_host_mocks_without_shares() {
    let dir = tempdir();
    std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
    std::fs::write(
        dir.path().join("mockingbird.yaml"),
        "mocks:\n  rest:\n    contract: orders.yaml\n    hosts: [api.example.com]\n  other:\n    contract: orders.yaml\n    route_prefix: /other\n    hosts: [api.example.com]\n",
    )
    .unwrap();
    let (mut mcp, _, started) = start_server_on_free_ports(
        dir.path(),
        &json!({ "config": "mockingbird.yaml", "offline": true }),
        mcp_env,
        || {},
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(started["ready"], true, "{started}");
        let steps = started["next_steps"].as_array().expect("next_steps");
        assert_eq!(steps.len(), 1, "{started}");
        let step = steps[0].as_str().unwrap();
        for part in [
            "\"other\", \"rest\"",
            "\"api.example.com\"",
            "shares",
            "same_as",
        ] {
            assert!(step.contains(part), "{part}: {step}");
        }
        // The orders contract's tier-0 world has gaps, so the review is not empty.
        let review = started["world_review"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(!review.is_empty(), "{started}");
        for mock in ["other", "rest"] {
            let world = mcp.tool("get_world", json!({ "mock": mock }));
            let gaps = world["gaps"].as_array().unwrap();
            let entry = review.iter().find(|e| e["mock"] == mock);
            if gaps.is_empty() {
                assert!(entry.is_none(), "{mock}: {started}");
                continue;
            }
            let entry = entry.unwrap_or_else(|| panic!("{mock}: {started}"));
            assert_eq!(entry["version"], world["version"], "{entry}");
            assert_eq!(entry["world_gaps_total"], gaps.len(), "{entry}");
            assert!(
                entry["next_steps"][0]
                    .as_str()
                    .is_some_and(|s| s.contains("mockingbird-world")),
                "{entry}"
            );
        }
    }));
    let stopped = mcp.tool("stop_server", json!({}));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
