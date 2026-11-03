//! Helpers shared by the modules of this test binary. Each one replaces
//! copies that were identical across the former per-file test binaries.
#![cfg(test)]

use std::collections::BTreeSet;
use std::hash::{BuildHasher, RandomState};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

/// The repository root.
pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `rel` under the repository root.
pub fn repo_path(rel: &str) -> PathBuf {
    repo().join(rel)
}

/// `rel` under the repository's `fixtures/`.
pub fn fixture(rel: &str) -> PathBuf {
    repo().join("fixtures").join(rel)
}

/// The contents of `path`, panicking with the path on error.
pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// How many times [`on_free_ports`] runs its attempt.
const PORT_ATTEMPTS: usize = 5;

/// The ports [`free_port`] picks from: below the ephemeral range of Linux
/// (32768-60999), macOS and Windows (49152-65535), so no port-0 listener of
/// another test can be given one of them.
const FIXED_PORTS: std::ops::Range<u16> = 20_000..32_000;

/// A loopback port for a process that must be told its port up front (a
/// `server start`, a config for `mockingbird run`): free when checked and
/// never handed out before in this process. Another process can still take
/// it before the server binds it, so start that server through
/// [`on_free_ports`]. A `serve` child is better given port 0 (see
/// [`spawn_serve`]), and an in-process server keeps its listener.
pub fn free_port() -> u16 {
    static HANDED_OUT: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
    let span = u64::from(FIXED_PORTS.end - FIXED_PORTS.start);
    let mut handed_out = HANDED_OUT.lock().unwrap();
    loop {
        assert!(
            (handed_out.len() as u64) < span,
            "every port in {FIXED_PORTS:?} was handed out"
        );
        let pick = RandomState::new().hash_one(handed_out.len()) % span;
        let port = FIXED_PORTS.start + u16::try_from(pick).unwrap();
        if handed_out.insert(port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// Runs `attempt`, which starts a server on ports from [`free_port`], and
/// runs it again (it picks fresh ports each time) when it failed because one
/// of them was taken before the server bound it: at most [`PORT_ATTEMPTS`]
/// times. `attempt` returns the text of its failure (stderr, a tool error);
/// any other failure panics with that text.
pub fn on_free_ports<T>(mut attempt: impl FnMut() -> Result<T, String>) -> T {
    let mut last = String::new();
    for _ in 0..PORT_ATTEMPTS {
        match attempt() {
            Ok(done) => return done,
            Err(text) if port_was_busy(&text) => last = text,
            Err(text) => panic!("{text}"),
        }
    }
    panic!("a port was still taken after {PORT_ATTEMPTS} attempts: {last}");
}

/// Whether `text` (a server's stderr or error) says a port was in use,
/// including by a server of another process that answered in place of ours
/// (see [`logged_admin`]).
pub fn port_was_busy(text: &str) -> bool {
    // Unix, then Windows (WSAEADDRINUSE).
    text.contains("Address already in use")
        || text.contains("os error 10048")
        || text.contains(NOT_OURS)
}

/// What [`logged_admin`] reports when the server that answered is not ours.
const NOT_OURS: &str = "a server of another process answered on";

/// Confirms that the server whose log is `log` bound the admin port
/// `admin`, so that the server a readiness check reached there is ours and
/// not another process's that took the port first. `serve` logs its bound
/// addresses after binding every listener and before serving, so a server
/// that answered has written the line by then.
///
/// # Errors
///
/// A text [`port_was_busy`] accepts, when the log lacks the line.
pub fn logged_admin(log: &Path, admin: u16) -> Result<(), String> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let line = format!("admin API on http://127.0.0.1:{admin}");
    match text.contains(&line) {
        true => Ok(()),
        false => Err(format!(
            "{NOT_OURS} {admin}: {} lacks `{line}`:\n{text}",
            log.display()
        )),
    }
}

/// The addresses a `mockingbird serve` child logged once it had bound them.
pub struct Bound {
    pub data: SocketAddr,
    pub admin: Option<SocketAddr>,
}

impl Bound {
    /// `http://<data plane><path>`.
    pub fn data_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.data)
    }

    /// `http://<admin port>`; panics when the child binds no admin port.
    pub fn admin_url(&self) -> String {
        let admin = self.admin.expect("the server binds an admin port");
        format!("http://{admin}")
    }
}

/// Spawns `cmd`, a `mockingbird serve` given port 0 for each listener it
/// should bind (the OS picks free ports, which nothing can take in between),
/// with stderr piped. Waits for the log lines naming the bound addresses and
/// returns the child with them; the rest of stderr is read and dropped.
pub fn spawn_serve(cmd: &mut std::process::Command) -> (std::process::Child, Bound) {
    use std::io::BufRead;

    let mut child = cmd
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning mockingbird serve");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            // Once the addresses are read nobody receives: keep draining.
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut data, mut admin, mut lines) = (None, None, Vec::new());
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(line) => line,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "serve did not log its addresses ({e}):\n{}",
                    lines.join("\n")
                );
            }
        };
        data = data.or_else(|| logged_addr(&line, "data plane on "));
        admin = admin
            .or_else(|| logged_addr(&line, "admin API on "))
            .or_else(|| logged_addr(&line, "ops endpoints on "));
        // The gRPC line ("gRPC (h2c) on ..." or "no gRPC listener") is
        // logged after the data plane and admin lines.
        if line.contains("gRPC (") || line.contains("no gRPC listener") {
            break;
        }
        lines.push(line);
    }
    let Some(data) = data else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("serve logged no data plane:\n{}", lines.join("\n"));
    };
    (child, Bound { data, admin })
}

/// The address after `marker` in a log line, without `http://` or
/// `https://`.
fn logged_addr(line: &str, marker: &str) -> Option<SocketAddr> {
    let (_, rest) = line.split_once(marker)?;
    let addr = rest.split_whitespace().next()?;
    let addr = addr
        .strip_prefix("http://")
        .or_else(|| addr.strip_prefix("https://"))
        .unwrap_or(addr);
    addr.parse().ok()
}

/// A listener on an ephemeral loopback port.
pub async fn ephemeral() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// Serves `app` on an ephemeral loopback port; returns its base URL.
pub async fn serve(app: axum::Router) -> String {
    let listener = ephemeral().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Polls `done` every 20 ms until it returns true; panics naming `what`
/// once `within` has passed.
pub async fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?} waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
