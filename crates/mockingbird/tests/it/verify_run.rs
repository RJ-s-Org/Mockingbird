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

/// A small app server for `--serve`: it writes its pid to `pid`, prints
/// `listening on <port>` and answers each GET with the status of its own
/// call to the mock (`GET https://api.orders.test/orders`, through the
/// proxy its environment names).
fn write_app(dir: &Path) {
    std::fs::write(
        dir.join("app.py"),
        r#"import http.server, os, subprocess, sys
port, pid = int(sys.argv[1]), sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        out = subprocess.run(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", "https://api.orders.test/orders"], capture_output=True, text=True).stdout.encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)
    def log_message(self, *a):
        pass
server = http.server.HTTPServer(("127.0.0.1", port), H)
with open(pid, "w") as f:
    f.write(str(os.getpid()))
print("listening on", port, flush=True)
server.serve_forever()
"#,
    )
    .unwrap();
}

/// `python3 -u app.py <port> <pid file>`.
fn app(dir: &Path, port: u16) -> String {
    format!(
        "python3 -u {} {port} {}",
        dir.join("app.py").display(),
        dir.join("app.pid").display()
    )
}

/// A test command that asks the served app on `port` (not through the
/// proxy) and passes when its call to the mock answered 200.
fn ask_app(port: u16) -> String {
    format!("test \"$(curl -sS --noproxy '*' http://127.0.0.1:{port}/)\" = 200")
}

/// Whether the process whose pid `file` holds is gone (it may take a
/// moment to be reaped).
#[cfg(unix)]
fn gone(file: &Path) -> bool {
    let pid = std::fs::read_to_string(file).unwrap();
    for _ in 0..50 {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// The served app's call to the mock counts in the window; the app is
/// stopped after the tests.
#[cfg(unix)]
#[test]
fn verify_run_serves_the_app_until_the_tests_end_and_counts_its_calls() {
    let dir = tempfile::tempdir().unwrap();
    write_app(dir.path());
    let port = free_port();
    let (code, json) = fresh(
        dir.path(),
        &ask_app(port),
        &[
            "--serve",
            &app(dir.path(), port),
            "--serve-port",
            &port.to_string(),
        ],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(json["ok"], true, "{json:#}");
    assert_eq!(json["window"]["requests"], 1, "the app's call: {json:#}");
    let serve = &json["serve"];
    assert_eq!(serve["ready"], true, "{json:#}");
    assert_eq!(serve["ready_by"], format!("port {port}"));
    assert!(serve["ready_ms"].as_u64().is_some(), "{json:#}");
    assert_eq!(serve["stopped"], true);
    assert!(
        serve["output_tail"]
            .as_str()
            .unwrap()
            .contains(&format!("listening on {port}")),
        "{json:#}"
    );
    assert!(
        gone(&dir.path().join("app.pid")),
        "the app outlived the run"
    );
}

#[cfg(unix)]
#[test]
fn verify_run_waits_for_the_serve_ready_line() {
    let dir = tempfile::tempdir().unwrap();
    write_app(dir.path());
    let port = free_port();
    let (code, json) = fresh(
        dir.path(),
        &ask_app(port),
        &[
            "--serve",
            &app(dir.path(), port),
            "--serve-ready",
            "^listening on \\d+$",
        ],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(
        json["serve"]["ready_by"],
        "a line matching \"^listening on \\\\d+$\""
    );
    assert_eq!(json["window"]["requests"], 1, "{json:#}");
    assert!(gone(&dir.path().join("app.pid")));
}

/// A serve command that exits before it is ready fails the run, and the
/// tests are not run.
#[cfg(unix)]
#[test]
fn verify_run_fails_when_the_serve_command_exits_early() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let port = free_port();
    let (code, json) = fresh(
        dir.path(),
        &format!("touch {}", ran.display()),
        &[
            "--serve",
            "echo cannot find module express >&2; exit 3",
            "--serve-port",
            &port.to_string(),
        ],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["ok"], false);
    assert_eq!(json["command"]["not_run"], true, "{json:#}");
    assert!(!ran.exists(), "the tests ran");
    assert_eq!(kinds(&json), ["serve_failed"], "{json:#}");
    let failure = &json["failures"][0];
    assert!(
        failure["detail"]
            .as_str()
            .unwrap()
            .contains("exited 3 before it was ready"),
        "{failure}"
    );
    assert!(
        failure["next_step"]
            .as_str()
            .unwrap()
            .contains("serve.output_tail")
    );
    assert_eq!(json["serve"]["exit_code"], 3, "{json:#}");
    assert!(
        json["serve"]["output_tail"]
            .as_str()
            .unwrap()
            .contains("cannot find module express"),
        "{json:#}"
    );
}

/// A serve command that never becomes ready is stopped at
/// `--serve-timeout`, and the tests are not run.
#[cfg(unix)]
#[test]
fn verify_run_stops_a_serve_command_that_never_becomes_ready() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("app.pid");
    let port = free_port();
    let started = std::time::Instant::now();
    let (code, json) = fresh(
        dir.path(),
        "true",
        &[
            "--serve",
            &format!("echo $$ > {}; exec sleep 60", pid.display()),
            "--serve-port",
            &port.to_string(),
            "--serve-timeout",
            "2s",
        ],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(kinds(&json), ["serve_failed"], "{json:#}");
    assert!(
        json["failures"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("was not ready within 2s"),
        "{json:#}"
    );
    assert_eq!(json["command"]["not_run"], true);
    assert_eq!(json["serve"]["stopped"], true);
    assert!(gone(&pid), "the serve command outlived its timeout");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(45),
        "{:?}",
        started.elapsed()
    );
}

/// The tests' timeout and Ctrl-C both stop the served app too.
#[cfg(unix)]
#[test]
fn verify_run_stops_the_served_app_on_a_timeout_and_on_ctrl_c() {
    let dir = tempfile::tempdir().unwrap();
    write_app(dir.path());
    let port = free_port();
    let pid = dir.path().join("app.pid");
    let (code, json) = fresh(
        dir.path(),
        "sleep 60",
        &[
            "--timeout",
            "2s",
            "--serve",
            &app(dir.path(), port),
            "--serve-port",
            &port.to_string(),
        ],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["command"]["timed_out"], true, "{json:#}");
    assert_eq!(json["serve"]["stopped"], true);
    assert!(gone(&pid), "the app outlived the timeout");

    std::fs::remove_file(&pid).unwrap();
    let started = dir.path().join("tests-started");
    let port = free_port();
    let out = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        interrupted_when(
            dir.path(),
            &[
                "--serve",
                &app(dir.path(), port),
                "--serve-port",
                &port.to_string(),
            ],
            &format!("touch {}; sleep 60", started.display()),
            &started,
            admin,
        )
    });
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
    });
    assert_eq!(out.status.code(), Some(1), "{json:#}");
    assert_eq!(json["command"]["interrupted"], true, "{json:#}");
    assert_eq!(json["serve"]["stopped"], true, "{json:#}");
    assert!(gone(&pid), "the app outlived Ctrl-C");
}

/// `verify --run <command>` (plus `extra`) in `dir`, sent Ctrl-C (SIGINT,
/// to this test's own child) once `marker` exists; its output.
#[cfg(unix)]
fn interrupted_when(
    dir: &Path,
    extra: &[&str],
    command: &str,
    marker: &Path,
    admin: u16,
) -> Result<std::process::Output, String> {
    let child = std::process::Command::cargo_bin("mockingbird")
        .unwrap()
        .current_dir(dir)
        .env("MB_CA_DIR", dir.join("ca"))
        .env_remove("MB_ADMIN_URL")
        .env_remove("MB_STATE_DIR")
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("https_proxy")
        .env_remove("http_proxy")
        .args(["verify", "--offline", "--format", "json", "--state-dir"])
        .arg(dir.join("state"))
        .args(extra)
        .args(["--run", command])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if port_was_busy(&stderr) {
        return Err(stderr.into_owned());
    }
    logged_admin(&dir.join("state/server.log"), admin)?;
    Ok(out)
}

/// A serve command whose group ignores TERM is killed after the grace.
#[cfg(unix)]
#[test]
fn verify_run_kills_a_serve_group_that_ignores_term_after_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("stubborn.pid");
    let (code, json) = fresh(
        dir.path(),
        "true",
        &[
            "--serve",
            &format!(
                "sh -c 'trap \"\" TERM; echo $$ > {}; echo ready; while :; do sleep 0.2; done' & wait",
                pid.display()
            ),
            "--serve-ready",
            "^ready$",
        ],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(json["serve"]["stopped"], true, "{json:#}");
    // It ignores TERM: only the KILL after the grace ends it.
    assert!(
        gone(&pid),
        "the grandchild that ignores TERM outlived the run"
    );
}

/// A port something already listens on, or one of Mockingbird's own, fails
/// before the serve command starts.
#[cfg(unix)]
#[test]
fn verify_run_refuses_a_serve_port_already_taken() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let (code, json) = fresh(
        dir.path(),
        "true",
        &[
            "--serve",
            &format!("touch {}; sleep 30", ran.display()),
            "--serve-port",
            &port.to_string(),
        ],
    );
    drop(taken);
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(kinds(&json), ["serve_failed"], "{json:#}");
    assert!(
        json["failures"][0]["detail"]
            .as_str()
            .unwrap()
            .contains(&format!("port {port} already accepted connections")),
        "{json:#}"
    );
    assert!(!ran.exists(), "the serve command started");
    // Mockingbird's own data-plane port.
    let out = on_free_ports(|| {
        let (data, admin) = write_config(dir.path());
        let out = verify_run(
            dir.path(),
            "true",
            &["--serve", "sleep 30", "--serve-port", &data.to_string()],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        if port_was_busy(&stderr) {
            return Err(stderr.into_owned());
        }
        logged_admin(&dir.path().join("state/server.log"), admin)?;
        Ok(out)
    });
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let failure = &json["failures"][0];
    assert_eq!(failure["kind"], "serve_failed", "{json:#}");
    assert!(
        failure["detail"]
            .as_str()
            .unwrap()
            .contains("Mockingbird's own data-plane port")
            && failure["next_step"]
                .as_str()
                .unwrap()
                .contains("Mockingbird's server uses it"),
        "{failure}"
    );
}

/// With no port and no ready line, the port the detection proposes for
/// that serve command is waited for.
#[cfg(unix)]
#[test]
fn verify_run_waits_for_the_detected_port() {
    let dir = tempfile::tempdir().unwrap();
    write_app(dir.path());
    let port = free_port();
    std::fs::write(dir.path().join(".rspec"), "").unwrap();
    std::fs::create_dir_all(dir.path().join("spec")).unwrap();
    std::fs::write(
        dir.path().join("spec/spec_helper.rb"),
        format!("SERVER_URL = ENV.fetch('SERVER_URL', 'http://localhost:{port}')\n"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("server")).unwrap();
    std::fs::write(
        dir.path().join("server/app.py"),
        format!(
            "import sys\nsys.argv = ['app', '{port}', {:?}]\nexec(open({:?}).read())\n",
            dir.path().join("app.pid").display().to_string(),
            dir.path().join("app.py").display().to_string()
        ),
    )
    .unwrap();
    let (code, json) = fresh(
        dir.path(),
        &ask_app(port),
        &["--serve", "cd server && python3 app.py"],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(
        json["serve"]["ready_by"],
        format!("port {port} (detected)"),
        "{json:#}"
    );
    assert_eq!(json["window"]["requests"], 1, "{json:#}");
    assert!(gone(&dir.path().join("app.pid")));
}

/// With no readiness signal at all, the serve command must still run after
/// 3 s, and a note says readiness was assumed.
#[cfg(unix)]
#[test]
fn verify_run_assumes_a_serve_command_ready_after_the_settle_time() {
    let dir = tempfile::tempdir().unwrap();
    write_app(dir.path());
    let port = free_port();
    let (code, json) = fresh(
        dir.path(),
        &ask_app(port),
        &["--serve", &app(dir.path(), port)],
    );
    assert_eq!(code, Some(0), "{json:#}");
    let serve = &json["serve"];
    assert_eq!(serve["ready_by"], "running for 3s", "{json:#}");
    assert!(serve["ready_ms"].as_u64().unwrap() >= 3000, "{json:#}");
    assert!(
        json["notes"]
            .to_string()
            .contains("taken as ready because it still ran after 3s"),
        "{json:#}"
    );
    assert!(gone(&dir.path().join("app.pid")));
}

/// An invalid ready pattern is refused before anything starts.
#[test]
fn verify_run_refuses_an_invalid_serve_ready_pattern() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());
    let out = verify_run(
        dir.path(),
        "true",
        &["--serve", "true", "--serve-ready", "(unclosed"],
    );
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--serve-ready \"(unclosed\" is not a valid regular expression"),
        "{stderr}"
    );
    assert!(
        !dir.path().join("state/server.log").exists(),
        "a server was started"
    );
}

/// Ctrl-C while the serve command is not ready yet stops it, and the tests
/// are not run.
#[cfg(unix)]
#[test]
fn verify_run_stops_the_serve_command_on_ctrl_c_while_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("waiting.pid");
    let ran = dir.path().join("ran");
    let port = free_port();
    let out = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        interrupted_when(
            dir.path(),
            &[
                "--serve",
                &format!("echo $$ > {}; exec sleep 60", pid.display()),
                "--serve-port",
                &port.to_string(),
            ],
            &format!("touch {}", ran.display()),
            &pid,
            admin,
        )
    });
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
    });
    assert_eq!(out.status.code(), Some(1), "{json:#}");
    assert_eq!(json["command"]["interrupted"], true, "{json:#}");
    assert_eq!(json["command"]["not_run"], true, "{json:#}");
    assert_eq!(json["serve"]["stopped"], true, "{json:#}");
    assert!(!ran.exists(), "the tests ran");
    assert!(gone(&pid), "the serve command outlived Ctrl-C");
}

/// A serve command that exits while the tests run is a note.
#[cfg(unix)]
#[test]
fn verify_run_notes_a_serve_command_that_exits_during_the_tests() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "sleep 3",
        &[
            "--serve",
            "echo ready; sleep 1; exit 4",
            "--serve-ready",
            "^ready$",
        ],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(json["serve"]["exit_code"], 4, "{json:#}");
    assert!(
        json["notes"]
            .to_string()
            .contains("the serve command exited 4 while the command ran"),
        "{json:#}"
    );
}

/// The command runs from the caller's folder, so a relative path works
/// from a subfolder.
#[cfg(unix)]
#[test]
fn verify_run_runs_from_the_callers_folder() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("tests");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(
        sub.join("check.sh"),
        "#!/bin/sh\ncurl -sSf -o /dev/null https://api.orders.test/orders\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(sub.join("check.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let out = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        let out = mockingbird(&sub)
            .args([
                "verify",
                "-c",
                "../mockingbird.yaml",
                "--offline",
                "--format",
                "json",
                "--state-dir",
            ])
            .arg(dir.path().join("state"))
            .args(["--run", "./check.sh"])
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
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
    });
    assert_eq!(out.status.code(), Some(0), "{json:#}");
    assert!(
        json["command"]["cwd"].as_str().unwrap().ends_with("tests"),
        "{json:#}"
    );
    assert_eq!(json["window"]["requests"], 1, "{json:#}");
}

/// A line-delimited JSON-RPC client of `mockingbird mcp` over stdio.
struct Mcp {
    child: std::process::Child,
    /// `None` once closed.
    stdin: Option<std::process::ChildStdin>,
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
            stdin: Some(stdin),
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
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
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

/// The MCP tool's `serve` (with `serve_port` and `cwd`) runs the app
/// beside the command and stops it; closing stdin while a run waits stops
/// the command and the app.
#[cfg(unix)]
#[test]
fn mcp_verify_run_serves_the_app_and_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    write_app(dir.path());
    std::fs::create_dir(dir.path().join("app")).unwrap();
    let mut mcp = Mcp::start(dir.path());
    mcp.rpc(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    let pid = dir.path().join("app.pid");
    let port = free_port();
    let outcome = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        let (failed, text) = mcp.tool(
            "verify_run",
            serde_json::json!({
                "command": format!("test \"$(basename \"$PWD\")\" = app && {}", ask_app(port)),
                "serve": app(dir.path(), port),
                "serve_port": port,
                "cwd": "app",
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
    assert_eq!(outcome["serve"]["stopped"], true, "{outcome:#}");
    assert!(gone(&pid), "the app outlived the run");

    // A cwd outside the project, or none, and bad serve arguments are
    // refused.
    for (args, want) in [
        (
            serde_json::json!({"cwd": dir.path().join("app")}),
            "is absolute",
        ),
        (
            serde_json::json!({"cwd": "app/../.."}),
            "leaves the project",
        ),
        (serde_json::json!({"cwd": "missing"}), "is no folder"),
        (
            serde_json::json!({"serve": "true", "serve_ready": "(unclosed"}),
            "serve_ready \"(unclosed\" is not a valid",
        ),
        (
            serde_json::json!({"serve": "true", "serve_port": 0}),
            "serve_port must be",
        ),
    ] {
        let mut args = args;
        args["command"] = "true".into();
        args["config"] = "mockingbird.yaml".into();
        let (failed, text) = mcp.tool("verify_run", args.clone());
        assert!(failed && text.contains(want), "{args}: {text}");
    }

    // The client goes away while the tests run.
    std::fs::remove_file(&pid).unwrap();
    let port = free_port();
    let started = dir.path().join("tests-started");
    {
        use std::io::Write as _;
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 99, "method": "tools/call", "params": {"name": "verify_run", "arguments": {
            "command": format!("touch {}; sleep 60", started.display()),
            "serve": app(dir.path(), port),
            "serve_port": port,
            "config": "mockingbird.yaml",
            "offline": true,
        }}});
        let stdin = mcp.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !started.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(started.exists(), "the tests did not start");
    drop(mcp.stdin.take());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while mcp.child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let exited = mcp.child.try_wait().unwrap().is_some();
    assert!(exited, "the MCP server did not exit after stdin closed");
    assert!(gone(&pid), "the app outlived the MCP client");
}

/// Stops the background server of folder `dir` (its default state
/// directory) when dropped.
struct StopServer(std::path::PathBuf);

impl Drop for StopServer {
    fn drop(&mut self) {
        let _ = mockingbird(&self.0).args(["server", "stop"]).output();
    }
}

/// A Mockingbird server of another project, answering `/readyz` on its own
/// port.
async fn foreign_server() -> wiremock::MockServer {
    use wiremock::matchers::path;
    let foreign = wiremock::MockServer::builder().start().await;
    wiremock::Mock::given(path("/readyz"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ready",
                "role": "all",
                "config": "/elsewhere/other/mockingbird.yaml",
                "dir": "/elsewhere/other",
            })),
        )
        .mount(&foreign)
        .await;
    foreign
}

/// A config serving `orders` for `api.orders.test` whose admin port is
/// `admin` and whose other ports are free.
fn write_config_on(dir: &Path, admin: u16) {
    std::fs::write(
        dir.join("mockingbird.yaml"),
        format!(
            "storage:\n  url: \"sqlite::memory:\"\nserver:\n  port: {}\n  admin_port: {admin}\n  grpc_port: {}\nproxy:\n  port: {}\n  unmapped: block\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [api.orders.test]\n",
            free_port(),
            free_port(),
            free_port(),
            fixture("openapi/orders-3.0.yaml").display()
        ),
    )
    .unwrap();
}

/// `server start` of the config in `dir` with `--proxy`, extra arguments
/// `extra` and environment `env`; returns its JSON once the server runs
/// and logged the admin port `admin`.
fn start(
    dir: &Path,
    extra: &[String],
    env: &[(&str, String)],
    admin: u16,
) -> Result<Value, String> {
    let mut cmd = mockingbird(dir);
    cmd.args([
        "server",
        "start",
        "-c",
        "mockingbird.yaml",
        "--offline",
        "--proxy",
        "--no-watch",
    ])
    .args(extra)
    .timeout(std::time::Duration::from_secs(90));
    for (name, value) in env {
        cmd.env(name, value);
    }
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        let _ = mockingbird(dir).args(["server", "stop"]).output();
        return Err(format!("{stderr}{}", String::from_utf8_lossy(&out.stdout)));
    }
    if let Err(e) = logged_admin(&dir.join(".mockingbird/server.log"), admin) {
        let _ = mockingbird(dir).args(["server", "stop"]).output();
        return Err(e);
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap())
}

/// `run`, `verify --run` and `mocks list` from `dir` all reach this
/// project's background server, never start a second one, and never use
/// the other project's server on the config's admin port.
fn uses_the_projects_server(dir: &Path) {
    let curl = "curl -sSf -o /dev/null -w '%{http_code}' https://api.orders.test/orders";
    let run = mockingbird(dir)
        .args(["run", "--", "sh", "-c", curl])
        .timeout(std::time::Duration::from_secs(90))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(0), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "200", "{stderr}");
    assert!(
        !stderr.contains("already runs") && !stderr.contains("server started"),
        "{stderr}"
    );

    let out = mockingbird(dir)
        .args(["verify", "--format", "json", "--run", curl])
        .timeout(std::time::Duration::from_secs(120))
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(out.status.code(), Some(0), "{json:#}");
    assert_eq!(json["window"]["server_started"], false, "{json:#}");

    let mocks = mockingbird(dir).args(["mocks", "list"]).output().unwrap();
    assert!(
        mocks.status.success() && String::from_utf8_lossy(&mocks.stdout).contains("orders"),
        "{}{}",
        String::from_utf8_lossy(&mocks.stdout),
        String::from_utf8_lossy(&mocks.stderr)
    );
}

/// The field trial: another project's server took the admin port the
/// config names, so the project's server is started on ports given to
/// `server start`. Those are written into the config, and `run`, `verify
/// --run` and the client subcommands then use that server.
#[tokio::test(flavor = "multi_thread")]
async fn ports_given_to_server_start_are_written_and_used_by_run_verify_and_the_client() {
    let foreign = foreign_server().await;
    let taken = foreign.address().port();
    tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir().unwrap();
        write_config_on(dir.path(), taken);
        // The config's admin port is taken, and set: the start refuses.
        let refused = start(dir.path(), &[], &[], taken).unwrap_err();
        assert!(refused.contains("other/mockingbird.yaml"), "{refused}");

        let _stop = StopServer(dir.path().to_path_buf());
        let (out, admin) = on_free_ports(|| {
            let (port, admin, grpc) = (free_port(), free_port(), free_port());
            let flags = [
                ("--port", port),
                ("--admin-port", admin),
                ("--grpc-port", grpc),
            ]
            .iter()
            .flat_map(|(flag, port)| [(*flag).to_owned(), port.to_string()])
            .collect::<Vec<_>>();
            start(dir.path(), &flags, &[], admin).map(|out| (out, admin))
        });
        assert!(
            out["note"]
                .as_str()
                .is_some_and(|n| n.contains(&format!("server.admin_port {admin} (was {taken})"))),
            "{out:#}"
        );
        assert_eq!(out["ports_written"]["ports"][1]["key"], "server.admin_port");
        let text = std::fs::read_to_string(dir.path().join("mockingbird.yaml")).unwrap();
        assert!(text.contains(&format!("admin_port: {admin}")), "{text}");
        uses_the_projects_server(dir.path());
    })
    .await
    .unwrap();
}

/// A background server started on another admin port than the config's
/// (here by `MB_ADMIN_PORT`, which is not written into the config) is
/// found from the project's state directory.
#[tokio::test(flavor = "multi_thread")]
async fn the_projects_recorded_server_is_used_wherever_it_listens() {
    let foreign = foreign_server().await;
    let taken = foreign.address().port();
    tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir().unwrap();
        write_config_on(dir.path(), taken);
        let _stop = StopServer(dir.path().to_path_buf());
        on_free_ports(|| {
            let admin = free_port();
            start(
                dir.path(),
                &[],
                &[("MB_ADMIN_PORT", admin.to_string())],
                admin,
            )
        });
        let text = std::fs::read_to_string(dir.path().join("mockingbird.yaml")).unwrap();
        assert!(text.contains(&format!("admin_port: {taken}")), "{text}");
        uses_the_projects_server(dir.path());
    })
    .await
    .unwrap();
}
