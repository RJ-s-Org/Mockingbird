//! `mockingbird run -- <command>`: starts the server and proxy, runs the
//! command with the proxy environment, returns its exit code and stops the
//! server again.
#![cfg(test)]

use crate::support::{fixture, free_port, logged_admin, on_free_ports, port_was_busy};
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

/// Writes a config on fresh ports and runs `go`, a `mockingbird run` that
/// starts its server; again on other ports while one was taken before the
/// server bound it. Returns the output and the admin port.
fn on_fresh_config(
    dir: &Path,
    go: impl Fn() -> std::process::Output,
) -> (std::process::Output, u16) {
    on_free_ports(|| {
        let admin = write_config(dir);
        let out = go();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if port_was_busy(&stderr) {
            return Err(stderr.into_owned());
        }
        // `run` reuses a server that already answers on the admin port:
        // confirm it started its own instead of finding another process's.
        logged_admin(&dir.join("state/server.log"), admin).map(|()| (out, admin))
    })
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
    // curl reaches the mock by its real host through the proxy, trusting
    // the CA from the environment.
    let (out, admin) = on_fresh_config(dir.path(), || {
        run(
            dir.path(),
            &[
                "sh",
                "-c",
                "curl -sSf -o /dev/null -w '%{http_code}' https://api.orders.test/orders",
            ],
        )
    });
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "200");
    assert!(!admin_answers(admin), "the server run started is stopped");

    let (out, admin) = on_fresh_config(dir.path(), || {
        run(
            dir.path(),
            &["sh", "-c", "test -n \"$NODE_OPTIONS\" && exit 7"],
        )
    });
    assert_eq!(out.status.code(), Some(7));
    assert!(!admin_answers(admin));

    let (out, _) = on_fresh_config(dir.path(), || {
        run(dir.path(), &["mockingbird-no-such-command"])
    });
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
    let (first, admin) = on_fresh_config(dir.path(), || {
        assert_cmd::Command::cargo_bin("mockingbird")
            .unwrap()
            .current_dir(dir.path())
            .env("MB_CA_DIR", dir.path().join("ca"))
            .args(["run", "--keep", "--state-dir"])
            .arg(dir.path().join("state"))
            .args(["--", "true"])
            .timeout(std::time::Duration::from_secs(90))
            .output()
            .unwrap()
    });
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

/// A server started with another config that holds this config's admin
/// port is never used: `run` names it and runs nothing.
#[tokio::test]
async fn run_refuses_another_configs_server_on_its_admin_port() {
    use wiremock::matchers::path;
    let stub = wiremock::MockServer::start().await;
    wiremock::Mock::given(path("/readyz"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ready",
                "role": "all",
                "config": "/elsewhere/other/mockingbird.yaml",
                "dir": "/elsewhere/other",
            })),
        )
        .mount(&stub)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let admin = stub.address().port();
    std::fs::write(
        dir.path().join("mockingbird.yaml"),
        format!(
            "storage:\n  url: \"sqlite::memory:\"\nserver:\n  port: {}\n  admin_port: {admin}\n  grpc_port: {}\nproxy:\n  port: {}\nmocks:\n  orders:\n    contract: {}\n",
            free_port(),
            free_port(),
            free_port(),
            fixture("openapi/orders-3.0.yaml").display()
        ),
    )
    .unwrap();
    let marker = dir.path().join("ran");
    let out = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        let marker = marker.clone();
        move || run(&dir, &["touch", marker.to_str().unwrap()])
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("started with config /elsewhere/other/mockingbird.yaml"),
        "{stderr}"
    );
    assert!(!marker.exists(), "the command did not run");
    assert!(!dir.path().join("state/server.pid").exists());
}

/// Stops the background server of a state directory when dropped.
struct StopServer(std::path::PathBuf);

impl Drop for StopServer {
    fn drop(&mut self) {
        let _ = assert_cmd::Command::cargo_bin("mockingbird")
            .unwrap()
            .args(["server", "stop", "--state-dir"])
            .arg(&self.0)
            .output();
    }
}

/// The field trial: another project's Mockingbird proxy holds the proxy
/// port of this project's config, and this project's server runs no proxy
/// (`server start` without `--proxy`). Neither `proxy env` nor `run` ever
/// sends the app to that proxy, which has none of these mocks and would
/// pass the app's calls on to the real API. Once the port is free, `run`
/// puts this project's own proxy in front of the server and the app reaches
/// the mock.
#[tokio::test]
async fn a_foreign_proxy_on_the_proxy_port_is_never_handed_out() {
    use wiremock::matchers::path;
    // Not from wiremock's pool, so that dropping it frees the port.
    let foreign = wiremock::MockServer::builder().start().await;
    let other = "/elsewhere/other/mockingbird.yaml";
    wiremock::Mock::given(path("/mockingbird-proxy.json"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "proxy": "mockingbird",
                "config": other,
                "dir": "/elsewhere/other",
            })),
        )
        .mount(&foreign)
        .await;
    let proxy_port = foreign.address().port();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("mockingbird.yaml");
    let state = dir.path().join("state");
    let mockingbird = {
        let dir = dir.path().to_path_buf();
        move |args: &[&str]| {
            assert_cmd::Command::cargo_bin("mockingbird")
                .unwrap()
                .current_dir(&dir)
                .env("MB_CA_DIR", dir.join("ca"))
                .env_remove("HTTPS_PROXY")
                .env_remove("HTTP_PROXY")
                .env_remove("https_proxy")
                .env_remove("http_proxy")
                .args(args)
                .timeout(std::time::Duration::from_secs(90))
                .output()
                .unwrap()
        }
    };
    let blocking = |args: Vec<String>| {
        let mockingbird = mockingbird.clone();
        tokio::task::spawn_blocking(move || {
            mockingbird(&args.iter().map(String::as_str).collect::<Vec<_>>())
        })
    };
    let s = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    // This project's server, without a proxy.
    let _server = StopServer(state.clone());
    let started = {
        let (config, state, mockingbird) = (config.clone(), state.clone(), mockingbird.clone());
        let dir = dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            on_free_ports(|| {
                let admin = free_port();
                std::fs::write(
                    &config,
                    format!(
                        "storage:\n  url: \"sqlite::memory:\"\nserver:\n  port: {}\n  admin_port: {admin}\n  grpc_port: {}\nproxy:\n  port: {proxy_port}\n  unmapped: block\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [api.orders.test]\n",
                        free_port(),
                        free_port(),
                        fixture("openapi/orders-3.0.yaml").display()
                    ),
                )
                .unwrap();
                let out = mockingbird(&[
                    "server",
                    "start",
                    "--state-dir",
                    state.to_str().unwrap(),
                    "-c",
                    config.to_str().unwrap(),
                    "--offline",
                    "--wait",
                    "60s",
                ]);
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                if !out.status.success() {
                    return Err(text);
                }
                logged_admin(&dir.join("state/server.log"), admin).map(|()| admin)
            })
        })
        .await
        .unwrap()
    };
    assert!(admin_answers(started));
    let config_arg = config.to_str().unwrap().to_owned();

    // `proxy env` names the other project's proxy and hands out nothing.
    let out = blocking(s(&["proxy", "env", "--json", "-c", &config_arg]))
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(other), "{stderr}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // `run` reuses this project's server but refuses the proxy port, which
    // the config sets, and runs nothing.
    let marker = dir.path().join("ran");
    let out = blocking(s(&[
        "run",
        "-c",
        &config_arg,
        "--state-dir",
        state.to_str().unwrap(),
        "--",
        "touch",
        marker.to_str().unwrap(),
    ]))
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(other), "{stderr}");
    assert!(!marker.exists(), "the command did not run");

    // With the port free, `proxy env` says this project's server has no
    // proxy instead of handing out an address nothing of it serves.
    drop(foreign);
    crate::support::wait_until(
        "the other proxy lets go of its port",
        std::time::Duration::from_secs(10),
        || std::net::TcpStream::connect(("127.0.0.1", proxy_port)).is_err(),
    )
    .await;
    let out = blocking(s(&["proxy", "env", "--json", "-c", &config_arg]))
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("runs no proxy"), "{stderr}");

    // `run` puts this project's proxy in front of its server: the app
    // reaches the mock by its real host, through a proxy that says it is
    // this config's.
    let out = blocking(s(&[
        "run",
        "-c",
        &config_arg,
        "--state-dir",
        state.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        "curl -sSf -o /dev/null -w '%{http_code} ' https://api.orders.test/orders && curl -sSf \"$HTTPS_PROXY/mockingbird-proxy.json\"",
    ]))
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let (code, origin) = stdout.split_once(' ').unwrap();
    assert_eq!(code, "200", "{stdout}");
    let origin: serde_json::Value = serde_json::from_str(origin).unwrap();
    let ours = std::fs::canonicalize(&config).unwrap();
    assert_eq!(origin["config"], ours.to_str().unwrap(), "{origin}");
    assert!(
        stderr.contains(&format!("proxy http://127.0.0.1:{proxy_port} in front")),
        "{stderr}"
    );
}
