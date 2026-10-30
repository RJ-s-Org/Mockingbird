//! `mockingbird proxy browser`: Chrome, Chromium or Edge through the proxy,
//! trusting the local CA for that browser only (a throwaway profile and
//! `--ignore-certificate-errors-spki-list` with the CA's key), so pages,
//! client-side `fetch` and images reach the mocks.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use super::env::{options_for, proxy_env};

#[derive(clap::Args, Debug)]
pub struct ProxyBrowserArgs {
    /// Config file whose `proxy.port`, `proxy.host` and `proxy.ca_dir`
    /// apply.
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
    /// The proxy's port [default: proxy.port, else 8888].
    #[arg(long)]
    pub port: Option<u16>,
    /// The browser executable [default: MB_BROWSER, else the first Chrome,
    /// Chromium, Edge or Playwright Chromium found].
    #[arg(long, env = "MB_BROWSER")]
    pub browser: Option<PathBuf>,
    /// Print the browser command as JSON instead of starting it.
    #[arg(long)]
    pub print: bool,
    /// Extra browser arguments (repeatable), e.g. `--arg=--headless=new`.
    #[arg(long = "arg", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// The page to open.
    pub url: Option<String>,
}

/// The flags that send Chromium through the proxy at `proxy_url` and trust
/// the CA whose key hashes to `spki`, with its own profile in `profile`.
pub fn chromium_args(proxy_url: &str, spki: &str, profile: &Path) -> Vec<String> {
    vec![
        format!("--proxy-server={proxy_url}"),
        format!("--ignore-certificate-errors-spki-list={spki}"),
        // The SPKI list is honoured only with a profile of its own.
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
    ]
}

/// Well-known browser locations, then names looked up on `PATH`.
fn candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
            "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ] {
            out.push(Path::new("/Applications").join(app));
        }
    }
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(dir) = std::env::var_os(base) {
                let dir = PathBuf::from(dir);
                out.push(dir.join("Google/Chrome/Application/chrome.exe"));
                out.push(dir.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
    }
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
    ];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                out.push(dir.join(name));
            }
        }
    }
    out.extend(playwright_chromium());
    out
}

/// Playwright's downloaded Chromium builds, newest first.
fn playwright_chromium() -> Vec<PathBuf> {
    let cache = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            Some(if cfg!(target_os = "macos") {
                home.join("Library/Caches/ms-playwright")
            } else {
                home.join(".cache/ms-playwright")
            })
        });
    let Some(Ok(entries)) = cache.map(std::fs::read_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("chromium-"))
        })
        .collect();
    dirs.sort();
    dirs.reverse();
    let inner = [
        "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
        "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
        "chrome-linux/chrome",
        "chrome-linux64/chrome",
        "chrome-win/chrome.exe",
    ];
    dirs.iter()
        .flat_map(|d| inner.iter().map(move |i| d.join(i)))
        .collect()
}

fn find_browser() -> anyhow::Result<PathBuf> {
    candidates()
        .into_iter()
        .find(|p| p.is_file())
        .context("no Chrome, Chromium or Edge found; pass --browser <path> (or set MB_BROWSER)")
}

/// `mockingbird proxy browser`.
///
/// # Errors
///
/// Fails when the proxy options or environment cannot be set up, or the
/// browser cannot be started or exits with an error.
pub async fn run(args: ProxyBrowserArgs) -> anyhow::Result<()> {
    let opts = options_for(args.config.as_deref(), args.port)?;
    let env = proxy_env(&opts)?;
    let profile = opts.ca_dir.join("chromium-profile");
    let browser = match args.browser {
        Some(path) => path,
        None => find_browser()?,
    };
    let mut argv = chromium_args(&env.proxy_url, &env.ca_spki_sha256, &profile);
    argv.extend(args.args);
    if let Some(url) = args.url {
        argv.push(url);
    }
    if args.print {
        let out = serde_json::json!({
            "browser": browser,
            "args": argv,
            "proxy": env.proxy_url,
            "pac": format!("{}{}", env.proxy_url, super::PAC_PATH),
            "ca_spki_sha256": env.ca_spki_sha256,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    eprintln!(
        "mockingbird: {} through {} (profile {})",
        browser.display(),
        env.proxy_url,
        profile.display()
    );
    let status = tokio::process::Command::new(&browser)
        .args(&argv)
        .status()
        .await
        .with_context(|| format!("starting {}", browser.display()))?;
    if !status.success() {
        bail!("{} exited with {status}", browser.display());
    }
    Ok(())
}
