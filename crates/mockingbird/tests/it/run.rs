//! `mockingbird run -- <command>`: starts the server and proxy, runs the
//! command with the proxy environment, returns its exit code and stops the
//! server again.
#![cfg(test)]

use crate::support::{fixture, free_port};
use std::path::Path;

/// A config serving `orders` for `api.orders.test` on free ports; returns
/// the admin port.
fn write_config(dir: &Path) -> u16 {
    let admin = free_port();
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
    admin
}

fn run(dir: &Path, command: &[&str]) -> std::process::Output {
    assert_cmd::Command::cargo_bin("mockingbird")
        .unwrap()
        .current_dir(dir)
        .env("MB_CA_DIR", dir.join("ca"))
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("https_proxy")
        .env_remove("http_proxy")
        .args(["run", "--offline", "--state-dir"])
        .arg(dir.join("state"))
        .arg("--")
        .args(command)
        .timeout(std::time::Duration::from_secs(90))
        .output()
        .unwrap()
}

fn admin_answers(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

#[test]
fn run_routes_the_command_and_propagates_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let admin = write_config(dir.path());

    // curl reaches the mock by its real host through the proxy, trusting
    // the CA from the environment.
    let out = run(
        dir.path(),
        &[
            "sh",
            "-c",
            "curl -sSf -o /dev/null -w '%{http_code}' https://api.orders.test/orders",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "200");
    assert!(!admin_answers(admin), "the server run started is stopped");

    let out = run(
        dir.path(),
        &["sh", "-c", "test -n \"$NODE_OPTIONS\" && exit 7"],
    );
    assert_eq!(out.status.code(), Some(7));
    assert!(!admin_answers(admin));

    let out = run(dir.path(), &["mockingbird-no-such-command"]);
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn run_needs_a_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("mockingbird")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a config"));
}

/// MOCKINGBIRD-STUB-2: `--offline` (like `--record`, `--replay` and `-c`)
/// applies only to a server `run` starts; reusing a running server says so.
#[test]
fn run_warns_when_its_server_flags_do_not_apply() {
    let dir = tempfile::tempdir().unwrap();
    let admin = write_config(dir.path());
    let first = assert_cmd::Command::cargo_bin("mockingbird")
        .unwrap()
        .current_dir(dir.path())
        .env("MB_CA_DIR", dir.path().join("ca"))
        .args(["run", "--keep", "--state-dir"])
        .arg(dir.path().join("state"))
        .args(["--", "true"])
        .timeout(std::time::Duration::from_secs(90))
        .output()
        .unwrap();
    let stop = || {
        let _ = assert_cmd::Command::cargo_bin("mockingbird")
            .unwrap()
            .args(["server", "stop", "--state-dir"])
            .arg(dir.path().join("state"))
            .output();
    };
    if !first.status.success() {
        stop();
        panic!("{}", String::from_utf8_lossy(&first.stderr));
    }
    assert!(admin_answers(admin), "--keep leaves the server running");
    let out = run(dir.path(), &["true"]);
    stop();
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already answers") && stderr.contains("--offline"),
        "{stderr}"
    );
    let first = String::from_utf8_lossy(&first.stderr);
    assert!(!first.contains("already answers"), "{first}");
}
