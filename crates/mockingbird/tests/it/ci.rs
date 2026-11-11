//! The CI commands end to end through the binary: `server start --record`
//! against a stub LLM, `snapshot export`, `server start --replay` with no
//! LLM at all, `report` and `verify`, and `server start` failing on a busy
//! port.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::support::{free_port, logged_admin, on_free_ports, spawn_serve};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml")
}

struct Ports {
    data: u16,
    admin: u16,
    grpc: u16,
}

impl Ports {
    fn new() -> Self {
        Self {
            data: free_port(),
            admin: free_port(),
            grpc: free_port(),
        }
    }

    fn admin_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.admin)
    }

    fn data_url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.data)
    }
}

fn mockingbird(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("mockingbird"));
    for k in [
        "MB_LLM_API_KEY",
        "MB_LLM_BASE_URL",
        "MB_ADMIN_URL",
        "MB_STATE_DIR",
    ] {
        cmd.env_remove(k);
    }
    cmd.args(args).envs(env.iter().copied()).output().unwrap()
}

fn start(state: &Path, ports: &Ports, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let (data, admin, grpc) = (
        ports.data.to_string(),
        ports.admin.to_string(),
        ports.grpc.to_string(),
    );
    let mut args = vec![
        "server",
        "start",
        "--state-dir",
        state.to_str().unwrap(),
        "--port",
        &data,
        "--admin-port",
        &admin,
        "--grpc-port",
        &grpc,
    ];
    args.extend_from_slice(extra);
    let contract = fixture();
    args.push(contract.to_str().unwrap());
    mockingbird(&args, env)
}

/// [`start`] on fresh ports, which it returns once the server runs; again on
/// other ports while one was taken before the server bound it.
fn start_on_free_ports(state: &Path, extra: &[&str], env: &[(&str, &str)]) -> Ports {
    on_free_ports(|| {
        let ports = Ports::new();
        let out = start(state, &ports, extra, env);
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        // Ready on the admin port: confirm it was this server that answered.
        logged_admin(&state.join("server.log"), ports.admin)
            .map(|()| ports)
            .inspect_err(|_| drop(Server(state.to_path_buf())))
    })
}

/// Stops the background server of `state` when dropped.
struct Server(PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        mockingbird(
            &["server", "stop", "--state-dir", self.0.to_str().unwrap()],
            &[],
        );
    }
}

fn json_out(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout {} stderr {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn order(id: &str) -> Value {
    json!({"id": id, "customerId": "c1", "status": "pending", "total": 10})
}

#[tokio::test]
async fn record_then_replay_answers_without_an_llm() {
    let tmp = tempfile::tempdir().unwrap();
    let snap = tmp.path().join("snapshot");
    let snap_s = snap.to_str().unwrap();
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": order("o1").to_string()}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&llm)
        .await;
    let http = reqwest::Client::new();

    // Record.
    let state = tmp.path().join("record");
    let uri = llm.uri();
    let env = [
        ("MB_LLM_BASE_URL", uri.as_str()),
        ("MB_LLM_API_KEY", "k"),
        ("MB_LLM_MODEL", "m"),
    ];
    let ports = start_on_free_ports(&state, &["--record", snap_s], &env);
    let server = Server(state);
    let created = http
        .post(ports.data_url("/orders"))
        .json(&json!({"customerId": "c1", "total": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let recorded: Value = http
        .get(ports.data_url("/orders/o2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let out = mockingbird(
        &[
            "--admin-url",
            &ports.admin_url(),
            "snapshot",
            "export",
            "--dir",
            snap_s,
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let exported = json_out(&out);
    assert_eq!(exported["mocks"][0]["sessions"], 1, "{exported}");
    drop(server);
    let llm_calls = llm.received_requests().await.unwrap().len();
    assert!(llm_calls >= 2);
    let answers = std::fs::read_to_string(snap.join("answers.jsonl")).unwrap();
    assert!(answers.lines().count() >= 2, "{answers}");
    assert!(snap.join("contracts/orders-3-0.yaml").exists());

    // Replay: no key, and the stub LLM is never called again.
    let state = tmp.path().join("replay");
    let ports = start_on_free_ports(&state, &["--replay", snap_s], &[]);
    let _server = Server(state);
    let stored = http.get(ports.data_url("/orders/o1")).send().await.unwrap();
    assert_eq!(stored.status(), 200, "the seeded session holds o1");
    let replayed: Value = http
        .get(ports.data_url("/orders/o2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed, recorded);
    // listOrders was never recorded: the faker answers. (A plain list is the session's
    // records, from state; a parameter the description does not name generates it.)
    let listed = http
        .get(ports.data_url("/orders?trace=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), 200);
    assert_eq!(llm.received_requests().await.unwrap().len(), llm_calls);

    let admin = ports.admin_url();
    let mut report = Value::Null;
    // The request log is written asynchronously.
    for _ in 0..50 {
        report = json_out(&mockingbird(&["--admin-url", &admin, "report"], &[]));
        if report["requests"].as_u64() >= Some(3) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let ops = &report["mocks"][0]["operations"];
    assert_eq!(ops["listOrders"]["fallbacks"], 1, "{report}");
    assert!(report["snapshot"]["miss"].as_u64() >= Some(1), "{report}");
    let reasons = &report["mocks"][0]["fallback_reasons"];
    assert!(
        reasons
            .to_string()
            .contains("no answer recorded in the snapshot"),
        "{reasons}"
    );

    let ok = mockingbird(
        &[
            "--admin-url",
            &admin,
            "verify",
            "--called",
            "getOrder=2",
            "--no-errors",
            "--format",
            "json",
        ],
        &[],
    );
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(json_out(&ok)["ok"], true);
    let failed = mockingbird(
        &[
            "--admin-url",
            &admin,
            "verify",
            "--no-fallbacks",
            "--called",
            "createOrder",
            "--format",
            "markdown",
        ],
        &[],
    );
    assert_eq!(failed.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&failed.stdout);
    assert!(stdout.contains("Checks failed"), "{stdout}");
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("no-fallbacks") && stderr.contains("createOrder: called 0"),
        "{stderr}"
    );
    // A route the contract lacks: verify's default text names it briefly.
    let missing = http
        .get(ports.data_url("/refunds/r1"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let mut unmatched = mockingbird(&["--admin-url", &admin, "verify", "--no-unmatched"], &[]);
    for _ in 0..50 {
        if unmatched.status.code() == Some(1) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        unmatched = mockingbird(&["--admin-url", &admin, "verify", "--no-unmatched"], &[]);
    }
    assert_eq!(unmatched.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&unmatched.stdout);
    assert!(
        stdout.starts_with("verify failed: 1 check")
            && stdout.contains(
                "no-unmatched: orders-3-0: 1 requests matched no operation (1× GET /refunds/r1)"
            ),
        "{stdout}"
    );
    assert!(
        unmatched.stderr.is_empty(),
        "text on stdout is not repeated"
    );
    let junit = mockingbird(&["--admin-url", &admin, "report", "--format", "junit"], &[]);
    assert!(
        String::from_utf8_lossy(&junit.stdout).contains(
            "<testcase classname=\"mockingbird.orders-3-0\" name=\"listOrders\"><failure"
        )
    );
}

/// `server start` on a port another process holds fails with the bind
/// error and spawns no server, whatever answers on that port.
fn assert_start_refused(ports: &Ports, listener: &str, port: u16) {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let out = start(&state, ports, &["--offline", "--wait", "5s"], &[]);
    let _server = Server(state.clone());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let body = json_out(&out);
    assert_eq!(body["ready"], false, "{body}");
    assert_eq!(body["listener"], listener, "{body}");
    assert_eq!(body["address"], format!("127.0.0.1:{port}"), "{body}");
    assert!(!body["os_error"].as_str().unwrap().is_empty(), "{body}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!("127.0.0.1:{port}")),
        "{out:?}"
    );
    assert!(!state.join("server.pid").exists(), "no server was spawned");
}

#[tokio::test]
async fn server_start_fails_on_a_busy_admin_port_that_answers_ready() {
    let stub = MockServer::start().await;
    Mock::given(path("/readyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ready"})))
        .mount(&stub)
        .await;
    let mut ports = Ports::new();
    ports.admin = stub.address().port();
    assert_start_refused(&ports, "admin", ports.admin);
}

#[test]
fn server_start_fails_on_a_busy_data_port() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut ports = Ports::new();
    ports.data = held.local_addr().unwrap().port();
    assert_start_refused(&ports, "data_plane", ports.data);
}

#[test]
fn replay_and_record_must_share_a_directory() {
    let out = mockingbird(
        &[
            "serve",
            "--port",
            "0",
            "--admin-port",
            "0",
            "--grpc-port",
            "0",
            "--replay",
            "a",
            "--record",
            "b",
            "--offline",
            fixture().to_str().unwrap(),
        ],
        &[],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("same directory"));
}

/// MOCKINGBIRD-STUB-6: the stop file `server start` names in
/// `MB_STOP_FILE` shuts `serve` down gracefully (the path `server stop`
/// takes on Windows, where a detached server has no console signal).
#[tokio::test]
async fn the_stop_file_shuts_the_server_down_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let stop_file = tmp.path().join("server.stop");
    let contract = fixture();
    let (mut child, bound) = spawn_serve(
        std::process::Command::new(assert_cmd::cargo::cargo_bin("mockingbird"))
            .args([
                "serve",
                "--offline",
                "--port",
                "0",
                "--admin-port",
                "0",
                "--grpc-port",
                "0",
                contract.to_str().unwrap(),
            ])
            .env("MB_STOP_FILE", &stop_file)
            .env_remove("MB_LLM_API_KEY")
            .stdout(std::process::Stdio::null()),
    );
    let http = reqwest::Client::new();
    let readyz = format!("{}/readyz", bound.admin_url());
    let mut ready = false;
    for _ in 0..200 {
        if let Ok(r) = http.get(&readyz).send().await
            && r.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !ready {
        let _ = child.kill();
        panic!("serve did not become ready");
    }
    std::fs::write(&stop_file, b"").unwrap();
    let mut status = None;
    for _ in 0..150 {
        if let Some(s) = child.try_wait().unwrap() {
            status = Some(s);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let Some(status) = status else {
        let _ = child.kill();
        panic!("serve did not stop within 15 s of the stop file");
    };
    assert!(status.success(), "a graceful stop exits 0: {status}");
    assert!(
        !stop_file.exists(),
        "serve removes the stop file it acted on"
    );
}
