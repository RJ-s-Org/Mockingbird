//! The CI commands end to end through the binary: `server start --record`
//! against a stub LLM, `snapshot export`, `server start --replay` with no
//! LLM at all, `report` and `verify`, and `server start --wait` failing.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
    let ports = Ports::new();
    let state = tmp.path().join("record");
    let uri = llm.uri();
    let env = [
        ("MB_LLM_BASE_URL", uri.as_str()),
        ("MB_LLM_API_KEY", "k"),
        ("MB_LLM_MODEL", "m"),
    ];
    let out = start(&state, &ports, &["--record", snap_s], &env);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let ports = Ports::new();
    let state = tmp.path().join("replay");
    let out = start(&state, &ports, &["--replay", snap_s], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    // listOrders was never recorded: the faker answers.
    let listed = http.get(ports.data_url("/orders")).send().await.unwrap();
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
    let junit = mockingbird(&["--admin-url", &admin, "report", "--format", "junit"], &[]);
    assert!(
        String::from_utf8_lossy(&junit.stdout).contains(
            "<testcase classname=\"mockingbird.orders-3-0\" name=\"listOrders\"><failure"
        )
    );
}

#[test]
fn server_start_fails_when_not_ready_in_time() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let out = start(&state, &Ports::new(), &["--offline", "--wait", "0s"], &[]);
    let _server = Server(state);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_out(&out)["ready"], false);
    assert!(String::from_utf8_lossy(&out.stderr).contains("was not ready within 0s"));
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
