//! The key setup page: a one-shot HTTP server on `127.0.0.1:<random>` whose
//! only URL carries a random token. The user types the key into its form;
//! the server writes it to the key store and points the profile at the
//! file. The key never goes back to the caller, a log or a response.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Router;
use axum::extract::rejection::FormRejection;
use axum::extract::{DefaultBodyLimit, Form, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::{Mutex, oneshot};

use super::store;
use crate::onboard;

/// How long the page waits for a key by default.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(600);

/// Longest key the form accepts.
const MAX_KEY_CHARS: usize = 4096;

/// What the page stores, and the profile it updates.
#[derive(Debug, Clone)]
pub struct PageOptions {
    /// The key file's name in the store.
    pub name: String,
    pub store_dir: PathBuf,
    /// The profile whose `api_key` should reference the file.
    pub profile: Option<ProfileTarget>,
    /// How long [`KeyPage::wait`] waits.
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ProfileTarget {
    pub config: PathBuf,
    pub profile: String,
}

/// A saved key: where it went, never its value.
#[derive(Debug, Clone)]
pub struct SavedKey {
    pub name: String,
    pub file: PathBuf,
    pub profile: Option<String>,
    pub config: Option<PathBuf>,
}

/// A running key page; dropping it (or [`KeyPage::wait`] returning) stops
/// the server.
pub struct KeyPage {
    /// `http://127.0.0.1:<port>/<token>`.
    pub url: String,
    name: String,
    timeout: Duration,
    saved: oneshot::Receiver<SavedKey>,
    _shutdown: oneshot::Sender<()>,
}

impl std::fmt::Debug for KeyPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPage")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The posted form. No `Debug`: it holds the key.
#[derive(Deserialize)]
struct KeyForm {
    key: String,
}

struct PageState {
    opts: PageOptions,
    token: String,
    host: String,
    origin: String,
    /// `Some` until a key is saved.
    done: Mutex<Option<oneshot::Sender<SavedKey>>>,
}

/// Starts the page on `127.0.0.1:<random port>`.
pub async fn start(opts: PageOptions) -> anyhow::Result<KeyPage> {
    if !store::valid_name(&opts.name) {
        return Err(anyhow!(
            "key names are 1-64 characters of [A-Za-z0-9._-], not starting with a dot"
        ));
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding the key setup page")?;
    let port = listener.local_addr()?.port();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let url = format!("{origin}/{token}");
    let (done_tx, saved) = oneshot::channel();
    let (shutdown, stop) = oneshot::channel::<()>();
    let name = opts.name.clone();
    let timeout = opts.timeout;
    let state = Arc::new(PageState {
        opts,
        token,
        host,
        origin,
        done: Mutex::new(Some(done_tx)),
    });
    let app = Router::new()
        .fallback(handle)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum::middleware::map_response(safe_headers))
        .with_state(state);
    tokio::spawn(async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = stop.await;
        });
        if let Err(e) = serve.await {
            tracing::warn!("key setup page: {e}");
        }
    });
    Ok(KeyPage {
        url,
        name,
        timeout,
        saved,
        _shutdown: shutdown,
    })
}

impl KeyPage {
    /// The saved key, when one is saved within `wait`; the page stays open
    /// otherwise.
    pub async fn saved_within(&mut self, wait: Duration) -> Option<SavedKey> {
        tokio::time::timeout(wait, &mut self.saved).await.ok()?.ok()
    }

    /// Waits until a key is saved, at most the page's timeout.
    pub async fn wait(self) -> anyhow::Result<SavedKey> {
        let timeout = self.timeout;
        match tokio::time::timeout(timeout, self.saved).await {
            Ok(Ok(saved)) => Ok(saved),
            Ok(Err(_)) => Err(anyhow!("the key setup page stopped before a key was saved")),
            Err(_) => Err(anyhow!(
                "no key was saved within {}; run it again",
                humantime::format_duration(timeout)
            )),
        }
    }
}

/// Lets the page run on without a caller waiting: logs the outcome.
pub fn detach(page: KeyPage) {
    tokio::spawn(async move {
        match page.wait().await {
            Ok(saved) => tracing::info!("saved key {}", saved.name),
            Err(e) => tracing::info!("key setup page: {e:#}"),
        }
    });
}

/// Opens `url` in the default browser; whether that worked.
pub fn open_browser(url: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn safe_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Not `no-referrer`: with it browsers send `Origin: null` on the form
    // post, which the origin check refuses. Nothing here links off-site.
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    resp
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem}input{width:100%;padding:.5rem;margin:.5rem 0;box-sizing:border-box}button{padding:.5rem 1rem}";

fn page(body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Mockingbird key setup</title><style>{STYLE}</style></head><body><h1>Mockingbird key setup</h1>{body}</body></html>"
    )
}

/// The form; `name` is a valid key name, so it needs no escaping.
fn form(name: &str, error: Option<&str>) -> Html<String> {
    let error = error
        .map(|e| format!("<p role=\"alert\">{}</p>", escape(e)))
        .unwrap_or_default();
    Html(page(&format!(
        "{error}<form method=\"post\"><label for=\"key\">API key for {name}</label><input id=\"key\" type=\"password\" name=\"key\" autocomplete=\"off\" required autofocus><button type=\"submit\">Save</button></form><p>The key is stored in a private file on this machine; it is not shown to the agent.</p>"
    )))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn status(code: StatusCode, text: &str) -> Response {
    (code, Html(page(&format!("<p>{}</p>", escape(text))))).into_response()
}

async fn handle(
    State(st): State<Arc<PageState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    form_data: Result<Form<KeyForm>, FormRejection>,
) -> Response {
    let token = uri.path().strip_prefix('/').unwrap_or_default();
    if !crate::mcp::same(token.as_bytes(), st.token.as_bytes()) {
        return status(StatusCode::NOT_FOUND, "Not found.");
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    if host != Some(st.host.as_str()) {
        return status(
            StatusCode::FORBIDDEN,
            "Open this page at the address you were given.",
        );
    }
    let name = &st.opts.name;
    match method {
        Method::GET => form(name, None).into_response(),
        Method::POST => {
            if let Some(origin) = headers.get(header::ORIGIN)
                && origin.to_str().ok() != Some(st.origin.as_str())
            {
                return status(StatusCode::FORBIDDEN, "Cross-site submissions are refused.");
            }
            let mut done = st.done.lock().await;
            if done.is_none() {
                return status(
                    StatusCode::GONE,
                    "The key was already saved; this page is closed.",
                );
            }
            let key = match form_data {
                Ok(Form(KeyForm { key })) => key,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, form(name, Some("Enter the key.")))
                        .into_response();
                }
            };
            let trimmed = key.trim();
            if trimmed.is_empty() || trimmed.chars().count() > MAX_KEY_CHARS {
                return (
                    StatusCode::BAD_REQUEST,
                    form(name, Some("Enter the key (at most 4096 characters).")),
                )
                    .into_response();
            }
            match save(&st.opts, trimmed) {
                Ok(saved) => {
                    if let Some(tx) = done.take() {
                        let _ = tx.send(saved);
                    }
                    status(
                        StatusCode::OK,
                        &format!("Saved {name}. You can close this tab."),
                    )
                }
                Err(e) => status(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Could not save the key: {e:#}. Try again."),
                ),
            }
        }
        _ => status(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed."),
    }
}

/// Stores `key` and points the profile at it.
fn save(opts: &PageOptions, key: &str) -> anyhow::Result<SavedKey> {
    let file = store::write_secret(&opts.store_dir, &opts.name, key)?;
    if let Some(target) = &opts.profile {
        onboard::set_profile_api_key(
            &target.config,
            &target.profile,
            &store::file_reference(&file),
        )?;
    }
    Ok(SavedKey {
        name: opts.name.clone(),
        file,
        profile: opts.profile.as_ref().map(|t| t.profile.clone()),
        config: opts.profile.as_ref().map(|t| t.config.clone()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    /// `key=<value>` as a form post (reqwest is built without `form`).
    trait KeyFormExt {
        fn key_form(self, key: &str) -> Self;
    }

    impl KeyFormExt for reqwest::RequestBuilder {
        fn key_form(self, key: &str) -> Self {
            let encoded: String = key
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                        (b as char).to_string()
                    }
                    b' ' => "+".into(),
                    _ => format!("%{b:02X}"),
                })
                .collect();
            self.header("content-type", "application/x-www-form-urlencoded")
                .body(format!("key={encoded}"))
        }
    }

    fn opts(dir: &Path, profile: Option<ProfileTarget>) -> PageOptions {
        PageOptions {
            name: "openrouter".into(),
            store_dir: dir.to_path_buf(),
            profile,
            timeout: PAGE_TIMEOUT,
        }
    }

    /// A store dir and a config with profile `openrouter`.
    fn setup() -> (tempfile::TempDir, PathBuf, ProfileTarget) {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join(".mockingbird/secrets");
        let config = root.path().join("mockingbird.yaml");
        std::fs::write(
            &config,
            "llm:\n  default_profile: openrouter\n  profiles:\n    openrouter:\n      base_url: https://openrouter.ai/api/v1\n      model: m\n      api_key: env:OPENROUTER_API_KEY\n",
        )
        .unwrap();
        let target = ProfileTarget {
            config,
            profile: "openrouter".into(),
        };
        (root, store, target)
    }

    #[tokio::test]
    async fn get_shows_the_form_with_safe_headers() {
        let (_root, dir, _) = setup();
        let page = start(opts(&dir, None)).await.unwrap();
        let r = reqwest::get(&page.url).await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["cache-control"], "no-store");
        // `no-referrer` would make browsers send `Origin: null` with the
        // form post, which the origin check refuses.
        assert_eq!(r.headers()["referrer-policy"], "same-origin");
        assert!(
            r.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("form-action 'self'")
        );
        let body = r.text().await.unwrap();
        assert!(body.contains("API key for openrouter") && body.contains("type=\"password\""));
    }

    #[tokio::test]
    async fn post_with_token_writes_a_private_file() {
        let (_root, dir, target) = setup();
        let cfg = target.config.clone();
        let page = start(opts(&dir, Some(target))).await.unwrap();
        let r = reqwest::Client::new()
            .post(&page.url)
            .key_form("sk-or-v1-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = r.text().await.unwrap();
        assert!(body.contains("Saved openrouter") && !body.contains("sk-or-v1-secret"));
        let saved = page.wait().await.unwrap();
        assert_eq!(saved.name, "openrouter");
        assert_eq!(saved.profile.as_deref(), Some("openrouter"));
        assert!(!format!("{saved:?}").contains("sk-or-v1-secret"));
        assert_eq!(
            std::fs::read_to_string(&saved.file).unwrap(),
            "sk-or-v1-secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&saved.file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let config = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            config.contains(&format!("file:{}", saved.file.display()))
                && !config.contains("sk-or-v1-secret"),
            "{config}"
        );
    }

    #[tokio::test]
    async fn wrong_token_is_refused() {
        let (_root, dir, _) = setup();
        let page = start(opts(&dir, None)).await.unwrap();
        let base = page.url.rsplit_once('/').unwrap().0;
        let c = reqwest::Client::new();
        let r = c
            .post(format!("{base}/{}", "0".repeat(64)))
            .key_form("k")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(
            c.get(format!("{base}/")).send().await.unwrap().status(),
            404
        );
        assert!(!dir.join("openrouter").exists());
    }

    #[tokio::test]
    async fn foreign_host_or_origin_is_refused() {
        let (_root, dir, _) = setup();
        let page = start(opts(&dir, None)).await.unwrap();
        let c = reqwest::Client::new();
        let r = c
            .post(&page.url)
            .header("host", "evil.example")
            .key_form("k")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .get(&page.url)
            .header("host", "localhost.evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .post(&page.url)
            .header("origin", "https://evil.example")
            .key_form("k")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .post(&page.url)
            .header("origin", "null")
            .key_form("k")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert!(!dir.join("openrouter").exists());
        let origin = page.url.rsplit_once('/').unwrap().0.to_string();
        let r = c
            .post(&page.url)
            .header("origin", origin)
            .key_form("k")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn second_post_is_gone() {
        let (_root, dir, _) = setup();
        let page = start(opts(&dir, None)).await.unwrap();
        let c = reqwest::Client::new();
        let post = |key: &'static str| c.post(&page.url).key_form(key).send();
        assert_eq!(post("first").await.unwrap().status(), 200);
        assert_eq!(post("second").await.unwrap().status(), 410);
        assert_eq!(
            std::fs::read_to_string(dir.join("openrouter")).unwrap(),
            "first"
        );
    }

    #[tokio::test]
    async fn a_short_wait_leaves_the_page_open() {
        let (_root, dir, _) = setup();
        let mut page = start(opts(&dir, None)).await.unwrap();
        assert!(
            page.saved_within(Duration::from_millis(100))
                .await
                .is_none()
        );
        let r = reqwest::Client::new()
            .post(&page.url)
            .key_form("later")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let saved = page.saved_within(Duration::from_secs(5)).await.unwrap();
        assert_eq!(saved.name, "openrouter");
    }

    #[tokio::test]
    async fn empty_key_and_timeout() {
        let (_root, dir, _) = setup();
        let page = start(PageOptions {
            timeout: Duration::from_millis(300),
            ..opts(&dir, None)
        })
        .await
        .unwrap();
        let c = reqwest::Client::new();
        let r = c.post(&page.url).key_form("  ").send().await.unwrap();
        assert_eq!(r.status(), 400);
        assert!(r.text().await.unwrap().contains("type=\"password\""));
        let long = "k".repeat(4097);
        let r = c
            .post(&page.url)
            .key_form(long.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let err = page.wait().await.unwrap_err().to_string();
        assert!(err.contains("no key was saved within"), "{err}");
        assert!(!dir.join("openrouter").exists());
    }
}
