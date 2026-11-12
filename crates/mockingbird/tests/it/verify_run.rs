//! `mockingbird verify --run "<cmd>"`: runs the app's command against the
//! mocks as `mockingbird run` does and reports, from the requests it made,
//! what points at the mock.
#![cfg(test)]

use crate::support::{fixture, free_port, logged_admin, on_free_ports, port_was_busy};
use assert_cmd::cargo::CommandCargoExt as _;
use serde_json::Value;
use std::path::Path;

/// A config serving `orders` for `api.orders.test` on free ports (unmapped
/// hosts blocked); returns the data-plane and admin ports.
fn write_config(dir: &Path) -> (u16, u16) {
    let (data, admin) = (free_port(), free_port());
    std::fs::write(
        dir.join("mockingbird.yaml"),
        format!(
            "storage:\n  url: \"sqlite::memory:\"\nserver:\n  port: {data}\n  admin_port: {admin}\n  grpc_port: {}\nproxy:\n  port: {}\n  unmapped: block\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [api.orders.test]\n",
            free_port(),
            free_port(),
            fixture("openapi/orders-3.0.yaml").display()
        ),
    )
    .unwrap();
    (data, admin)
}

fn mockingbird(dir: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("mockingbird").unwrap();
    cmd.current_dir(dir)
        .env("MB_CA_DIR", dir.join("ca"))
        .env_remove("MB_ADMIN_URL")
        .env_remove("MB_STATE_DIR")
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("https_proxy")
        .env_remove("http_proxy");
    cmd
}

/// `verify --run <command> --format json` (plus `extra`), starting the
/// server unless one answers.
fn verify_run(dir: &Path, command: &str, extra: &[&str]) -> std::process::Output {
    mockingbird(dir)
        .args(["verify", "--offline", "--format", "json", "--state-dir"])
        .arg(dir.join("state"))
        .args(extra)
        .args(["--run", command])
        .timeout(std::time::Duration::from_secs(120))
        .output()
        .unwrap()
}

/// [`verify_run`] on a fresh config, again on other ports while one was
/// taken before the server bound it. Returns the exit code and the JSON.
fn fresh(dir: &Path, command: &str, extra: &[&str]) -> (Option<i32>, Value) {
    on_free_ports(|| {
        let (_, admin) = write_config(dir);
        let out = verify_run(dir, command, extra);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if port_was_busy(&stderr) {
            return Err(stderr.into_owned());
        }
        logged_admin(&dir.join("state/server.log"), admin)?;
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}\nstdout: {}\nstderr: {stderr}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        Ok((out.status.code(), json))
    })
}

fn kinds(json: &Value) -> Vec<&str> {
    json["failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn verify_run_reports_an_unmatched_route_and_a_rejected_request_with_next_steps() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "curl -s -o /dev/null https://api.orders.test/nope && curl -s -o /dev/null 'https://api.orders.test/orders?status=bogus'",
        &[],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["ok"], false);
    assert_eq!(json["command"]["exit_code"], 0, "{json:#}");
    assert_eq!(json["window"]["requests"], 2, "{json:#}");
    assert_eq!(json["window"]["server_started"], true);
    assert_eq!(
        kinds(&json),
        ["unmatched_route", "error_status"],
        "{json:#}"
    );
    let unmatched = &json["failures"][0];
    assert_eq!(unmatched["mock"], "orders");
    assert!(
        unmatched["detail"]
            .as_str()
            .unwrap()
            .contains("GET /nope matched no operation"),
        "{unmatched}"
    );
    assert!(
        unmatched["next_step"]
            .as_str()
            .unwrap()
            .contains("edit_contract"),
        "{unmatched}"
    );
    let error = &json["failures"][1];
    assert_eq!(error["operation"], "listOrders");
    assert_eq!(error["status"], 400);
    // A request validation rejects fails even though the command passed.
    assert_eq!(error["source"], "rejected");
    // The problem's detail, read from the recorded response.
    assert!(
        error["detail"].as_str().unwrap().contains("query/status"),
        "{error}"
    );
    let step = error["next_step"].as_str().unwrap();
    assert!(
        step.contains("listOrders") && step.contains("get_world") && step.contains("get_request"),
        "{step}"
    );
}

/// A 4xx the app's test asked for (here a status forced by a header, as a
/// scenario would) is expected while the command passes: a note, exit 0.
#[test]
fn verify_run_takes_a_4xx_as_expected_when_the_command_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "test \"$(curl -s -o /dev/null -w '%{http_code}' -H 'x-mockingbird-status: 404' https://api.orders.test/orders/missing)\" = 404",
        &[],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(json["ok"], true);
    assert_eq!(json["failures"], serde_json::json!([]));
    let notes = json["notes"].to_string();
    assert!(
        notes.contains("getOrder answered 404") && notes.contains("expected"),
        "{json:#}"
    );
}

/// What the command leaves running in its process group is stopped with
/// it, after a timeout and after a normal exit alike.
#[test]
fn verify_run_stops_what_the_command_left_running() {
    let dir = tempfile::tempdir().unwrap();
    let late = dir.path().join("late");
    let started = std::time::Instant::now();
    let (code, json) = fresh(
        dir.path(),
        &format!("(sleep 4; touch {}) & sleep 60", late.display()),
        &["--timeout", "1s"],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["command"]["timed_out"], true);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(45),
        "a grandchild holding the output must not hold up the report: {:?}",
        started.elapsed()
    );
    let after_exit = dir.path().join("after-exit");
    let (code, json) = fresh(
        dir.path(),
        &format!("(sleep 4; touch {}) & exit 0", after_exit.display()),
        &[],
    );
    assert_eq!(code, Some(0), "{json:#}");
    std::thread::sleep(std::time::Duration::from_secs(6));
    assert!(
        !late.exists(),
        "the backgrounded process outlived the timeout"
    );
    assert!(
        !after_exit.exists(),
        "the backgrounded process outlived the command"
    );
}

#[test]
fn verify_run_passes_a_clean_command() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "curl -sSf -o /dev/null https://api.orders.test/orders",
        &[],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(json["ok"], true);
    assert_eq!(json["command"]["exit_code"], 0);
    assert_eq!(json["window"]["requests"], 1);
    assert_eq!(json["failures"], serde_json::json!([]));
}

#[test]
fn verify_run_reports_a_failing_command_with_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "curl -sSf -o /dev/null https://api.orders.test/orders; echo 'FAIL orders spec'; exit 3",
        &[],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["ok"], false);
    assert_eq!(json["command"]["exit_code"], 3);
    assert_eq!(json["failures"], serde_json::json!([]));
    assert!(
        json["command"]["output_tail"]
            .as_str()
            .unwrap()
            .contains("FAIL orders spec"),
        "{json:#}"
    );
}

#[test]
fn verify_run_stops_a_command_at_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (code, json) = fresh(dir.path(), "sleep 60", &["--timeout", "1s"]);
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["command"]["timed_out"], true);
    assert_eq!(json["command"]["exit_code"], Value::Null);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(45),
        "{:?}",
        started.elapsed()
    );
}

/// Requests logged before the command started do not count: against a
/// running server, only the command's own requests are examined.
#[test]
fn verify_run_only_counts_requests_inside_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let data = on_free_ports(|| {
        let (data, admin) = write_config(dir.path());
        let out = mockingbird(dir.path())
            .args([
                "server",
                "start",
                "-c",
                "mockingbird.yaml",
                "--offline",
                "--proxy",
                "--no-watch",
                "--state-dir",
            ])
            .arg(&state)
            .timeout(std::time::Duration::from_secs(90))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let _ = mockingbird(dir.path())
                .args(["server", "stop", "--state-dir"])
                .arg(&state)
                .output();
            return Err(stderr.into_owned());
        }
        logged_admin(&state.join("server.log"), admin)?;
        Ok(data)
    });
    // An unmatched route, before the window.
    let before = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-H", "Host: api.orders.test"])
        .arg(format!("http://127.0.0.1:{data}/nope"))
        .status()
        .unwrap();
    let out = verify_run(
        dir.path(),
        "curl -sSf -o /dev/null https://api.orders.test/orders",
        &[],
    );
    let _ = mockingbird(dir.path())
        .args(["server", "stop", "--state-dir"])
        .arg(&state)
        .output();
    assert!(before.success());
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
    });
    assert_eq!(out.status.code(), Some(0), "{json:#}");
    assert_eq!(json["window"]["requests"], 1, "{json:#}");
    assert_eq!(json["window"]["server_started"], false);
    assert_eq!(json["failures"], serde_json::json!([]));
}

#[test]
fn verify_run_text_names_each_failure_with_its_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let out = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        let out = mockingbird(dir.path())
            .args(["verify", "--offline", "--state-dir"])
            .arg(dir.path().join("state"))
            .args(["--run", "curl -s -o /dev/null https://api.orders.test/nope"])
            .timeout(std::time::Duration::from_secs(120))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if port_was_busy(&stderr) {
            return Err(stderr.into_owned());
        }
        logged_admin(&dir.path().join("state/server.log"), admin)?;
        Ok(out)
    });
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(
            "verify --run failed: `curl -s -o /dev/null https://api.orders.test/nope` exited 0"
        ),
        "{text}"
    );
    assert!(
        text.contains("  unmatched_route: orders: GET /nope"),
        "{text}"
    );
    assert!(
        text.contains("    Next: Mock `orders` has no operation"),
        "{text}"
    );
}

/// A line-delimited JSON-RPC client of `mockingbird mcp` over stdio.
struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(dir: &Path) -> Self {
        let mut child = std::process::Command::cargo_bin("mockingbird")
            .unwrap()
            .current_dir(dir)
            .env("MB_NO_SHELL_ENV", "1")
            .env("MB_CA_DIR", dir.join("ca"))
            .env_remove("MB_ADMIN_URL")
            .env_remove("MB_STATE_DIR")
            .env_remove("HTTPS_PROXY")
            .env_remove("HTTP_PROXY")
            .env_remove("https_proxy")
            .env_remove("http_proxy")
            .arg("mcp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next: 0,
        }
    }

    /// One request; every line on stdout must be a JSON-RPC message, and
    /// the next one answers this request (nothing else is written there).
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        use std::io::{BufRead as _, Write as _};
        self.next += 1;
        let id = self.next;
        let msg =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout carried a non-JSON-RPC line ({e}): {line:?}"));
        assert_eq!(resp["id"], id, "{resp}");
        resp
    }

    /// `tools/call`: whether it failed, and its text.
    fn tool(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.rpc(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (r["result"]["isError"] == true, text)
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The MCP tool: the config resolves to ./mockingbird.yaml, a missing one
/// is an error, `timeout_secs` stops the command, and the command's output
/// never reaches the protocol's stdout.
#[test]
fn mcp_verify_run_runs_a_command_and_keeps_stdio_clean() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::start(dir.path());
    mcp.rpc(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    let (failed, text) = mcp.tool("verify_run", serde_json::json!({"command": "true"}));
    assert!(failed, "{text}");
    assert!(text.contains("give config"), "{text}");

    let outcome = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        let (failed, text) = mcp.tool(
            "verify_run",
            serde_json::json!({
                "command": "echo to-stdout; echo to-stderr >&2; curl -sSf -o /dev/null https://api.orders.test/orders",
                "offline": true,
            }),
        );
        if failed {
            return Err(text);
        }
        let log = dir.path().join(".mockingbird/server.log");
        if log.exists() {
            logged_admin(&log, admin)?;
        }
        Ok(serde_json::from_str::<Value>(&text).unwrap())
    });
    assert_eq!(outcome["ok"], true, "{outcome:#}");
    assert_eq!(outcome["window"]["requests"], 1, "{outcome:#}");
    let tail = outcome["command"]["output_tail"].as_str().unwrap();
    assert!(
        tail.contains("to-stdout") && tail.contains("to-stderr"),
        "{tail}"
    );

    let (failed, text) = mcp.tool(
        "verify_run",
        serde_json::json!({"command": "sleep 60", "config": "mockingbird.yaml", "offline": true, "timeout_secs": 1}),
    );
    assert!(!failed, "{text}");
    let outcome: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(outcome["command"]["timed_out"], true, "{outcome:#}");
    assert_eq!(outcome["ok"], false);
    // The stream is still in step after both runs.
    let tools = mcp.rpc("tools/list", serde_json::json!({}));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty())
    );
}
