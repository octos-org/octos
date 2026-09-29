//! The person's browser: results pages that need a real browser (Google)
//! are loaded in a Chrome that octos runs with its own persistent profile,
//! as the person would load them (OctoSense ADR 0002 §6 amendment, octos
//! issue #2608).
//!
//! What that means here:
//! - A plain Chrome. No stealth patches, no `navigator.webdriver` masking,
//!   no User-Agent override: the browser identifies itself as it would for
//!   the person (headless Chrome says it is headless). `chromiumoxide`'s
//!   test-harness defaults (`--enable-automation` and friends) are left out
//!   because this is not a test browser.
//! - One persistent profile (`~/.octos/browser-profile`, or
//!   [`BROWSER_PROFILE_ENV`]). Cookies, consent choices and a sign-in persist.
//!   Google refuses sign-in in a browser under remote control, so the person
//!   signs in by opening a plain Chrome on the same profile once. A second
//!   octos process connects to the running browser instead of launching
//!   another.
//! - Challenges ("unusual traffic", CAPTCHAs) are never solved or worked
//!   around. Where there is a display they are shown to the person
//!   ([`PersonBrowser::show`]); otherwise they are only reported.
//! - What it means for the person: with a signed-in profile, searches run
//!   as their Google account (personalised results, saved to its search
//!   activity). Every search that used the browser says so, with the terms
//!   caveat and the opt-out ([`crate::BROWSER_SEARCH_NOTICE`]), and it is
//!   logged once when the browser starts. `OCTOS_BROWSER=off` stops it.
//! - Never a browser's own profile: the DevTools port octos attaches to is
//!   unauthenticated (loopback only) and gives full control of every
//!   signed-in session in that profile. [`check_profile`] refuses browsers'
//!   user-data directories, and a custom location must be new, empty or
//!   already an octos profile ([`PROFILE_MARKER`]).
//! - A browser octos launched closes after [`IDLE_CLOSE`] without use, and
//!   short-lived hosts close it on exit ([`close_shared`]).
//!
//! Modes ([`BROWSER_ENV`]): `off` (default: searching needs no browser and
//! no person; Google comes from its page for simple phones, see
//! `metasearch::impersonate`), `auto` (pages load headless; a challenge
//! opens a window when there is a display), `window` (pages load in a
//! minimised window), `headless` (never a window).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::{
    Bounds, GetWindowForTargetParams, SetWindowBoundsParams, WindowState,
};
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use futures::StreamExt;
use tokio::sync::Mutex;

use crate::metasearch::http::{Fetch, FetchFuture, HandOverFuture, HttpRequest, HttpResponse};

/// `off` (default) | `auto` | `window` | `headless`.
pub use crate::BROWSER_ENV;

/// Profile directory override (default `~/.octos/browser-profile`).
pub const BROWSER_PROFILE_ENV: &str = "OCTOS_BROWSER_PROFILE";

/// Chrome/Chromium executable override (same variable the `browser` tool
/// honours).
pub const CHROME_ENV: &str = "CHROME";

/// A browser octos launched closes after this long without use.
pub const IDLE_CLOSE: Duration = Duration::from_secs(5 * 60);

/// Rendered pages larger than this are cut: the engine sandbox parses at
/// most 3 MiB, so a larger page would fetch fine and then fail to parse.
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

/// A challenge for the same host is shown at most once in this window, so
/// repeated searches do not pile up tabs while the person deals with it.
const SHOW_AGAIN_AFTER: Duration = Duration::from_secs(120);

/// How long a hand-over waits for the challenge tab before answering (the
/// core allows 5 s); opening continues in the background after that.
const HAND_OVER_WAIT: Duration = Duration::from_secs(4);

/// Two-second waits for a self-clearing challenge page
/// ([`crate::access::is_interstitial`]).
const INTERSTITIAL_WAITS: u32 = 5;

/// Pause after the load event for script-built results to settle.
const SETTLE: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pages load headless; a challenge opens a window.
    Auto,
    /// Pages load in a minimised window.
    Window,
    /// Never a window: challenges are only reported.
    Headless,
    Off,
}

impl Mode {
    /// Resolve the mode (env lookup and display probe injected for tests).
    /// Without a display `auto` and `window` are `headless`. An
    /// unrecognised value is `off` (`1` and `true` too: name the mode), so a
    /// typo never opens windows.
    pub fn resolve(lookup: impl Fn(&str) -> Option<String>, has_display: bool) -> Mode {
        let windowed = |m: Mode| if has_display { m } else { Mode::Headless };
        match lookup(BROWSER_ENV) {
            // Off unless asked for: searching needs no browser and no person
            // (Google comes from its page for simple phones, see
            // `metasearch::impersonate`).
            None => Mode::Off,
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" | "off" => Mode::Off,
                "auto" => windowed(Mode::Auto),
                "window" => windowed(Mode::Window),
                "headless" => Mode::Headless,
                _ => Mode::Off,
            },
        }
    }

    /// Whether pages load headless.
    fn loads_headless(self) -> bool {
        self != Mode::Window
    }

    /// Whether a challenge can be shown to the person.
    fn can_show(self) -> bool {
        matches!(self, Mode::Auto | Mode::Window)
    }
}

/// Whether this machine has a display a Chrome window can open on.
pub fn has_display(lookup: impl Fn(&str) -> Option<String>) -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|k| lookup(k).is_some_and(|v| !v.is_empty()))
}

/// Marker octos writes into a profile it launched a browser on. A custom
/// profile ([`BROWSER_PROFILE_ENV`]) must carry it, or be empty/new.
pub const PROFILE_MARKER: &str = ".octos-browser-profile";

/// Browsers' own user-data directories (relative to home). octos never uses
/// one: attaching to a browser there over the DevTools port (loopback, no
/// authentication) would give full control of every signed-in session in
/// it, far beyond searching.
const REAL_PROFILE_DIRS: &[&str] = &[
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Google/Chrome Beta",
    "Library/Application Support/Google/Chrome Canary",
    "Library/Application Support/Chromium",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Vivaldi",
    "Library/Application Support/Arc",
    ".config/google-chrome",
    ".config/google-chrome-beta",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    "snap/chromium",
    "AppData/Local/Google/Chrome",
    "AppData/Local/Chromium",
    "AppData/Local/BraveSoftware",
    "AppData/Local/Microsoft/Edge",
    "AppData/Local/Vivaldi",
];

/// Whether octos may use `profile`: never a browser's own profile; a custom
/// location only if it is new, empty, or already an octos profile.
/// `default` is the octos-owned default location.
pub fn check_profile(profile: &Path, default: bool, home: Option<&Path>) -> Result<(), String> {
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let profile_r = resolve(profile);
    if let Some(home) = home {
        let home_r = resolve(home);
        for dir in REAL_PROFILE_DIRS {
            for h in [home, home_r.as_path()] {
                let real = h.join(dir);
                if profile.starts_with(&real) || profile_r.starts_with(&real) {
                    return Err(format!(
                        "{} is a browser's own profile; octos will not use it (attaching to it \
                         would give octos control of every signed-in session). Leave \
                         {BROWSER_PROFILE_ENV} unset to use ~/.octos/browser-profile.",
                        profile.display()
                    ));
                }
            }
        }
    }
    if default || profile.join(PROFILE_MARKER).exists() {
        return Ok(());
    }
    let empty = match std::fs::read_dir(profile) {
        Err(_) => true, // does not exist yet: octos creates it
        Ok(mut entries) => entries.next().is_none(),
    };
    if empty {
        return Ok(());
    }
    Err(format!(
        "{} is not an octos browser profile (no {PROFILE_MARKER}); octos only uses a new or \
         empty directory, or one it created.",
        profile.display()
    ))
}

/// Default profile directory: `$HOME/.octos/browser-profile`.
pub fn default_profile(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(p) = lookup(BROWSER_PROFILE_ENV).filter(|p| !p.trim().is_empty()) {
        return Some(PathBuf::from(p));
    }
    let home = lookup("HOME").or_else(|| lookup("USERPROFILE"))?;
    Some(Path::new(&home).join(".octos").join("browser-profile"))
}

/// Chrome's `DevToolsActivePort` file (first line port, second line the
/// browser target path) as a WebSocket URL.
fn devtools_ws_url(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with("/devtools/browser/")
        .then(|| format!("ws://127.0.0.1:{port}{path}"))
}

/// Chrome's arguments (without leading dashes: chromiumoxide adds them):
/// only what a long-running personal browser needs.
fn launch_args(headless: bool) -> Vec<String> {
    let mut args = vec![
        "no-first-run".to_string(),
        "no-default-browser-check".to_string(),
        // Pages load in a minimised window; keep them from being throttled.
        "disable-backgrounding-occluded-windows".to_string(),
        "disable-renderer-backgrounding".to_string(),
    ];
    if headless {
        args.push("headless=new".to_string());
    }
    args
}

/// The process-wide person's browser (`None` when off), shared by every
/// metasearch in the process.
pub fn shared() -> Option<Arc<PersonBrowser>> {
    static SHARED: OnceLock<Option<Arc<PersonBrowser>>> = OnceLock::new();
    SHARED
        .get_or_init(|| PersonBrowser::from_env().map(Arc::new))
        .clone()
}

/// Close the shared browser if this process launched it (letting Chrome
/// save the profile). Short-lived hosts call this before exiting: statics
/// are never dropped, so the browser would outlive them.
pub async fn close_shared() {
    if let Some(b) = shared() {
        b.close().await;
    }
}

struct Session {
    browser: Browser,
    handler: tokio::task::JoinHandle<()>,
    /// Launched by this process (it owns the Chrome child) rather than
    /// connected to one another octos process started.
    launched: bool,
    headless: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.handler.abort();
    }
}

async fn close_session(mut s: Session) {
    if s.launched {
        let _ = s.browser.close().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), s.browser.wait()).await;
    }
}

/// The person's browser, launched (or connected to) on first use.
pub struct PersonBrowser {
    mode: Mode,
    profile: PathBuf,
    executable: Option<PathBuf>,
    session: Arc<Mutex<Option<Session>>>,
    last_used: Arc<StdMutex<Instant>>,
    shown: StdMutex<HashMap<String, Instant>>,
    /// The last start failed (no usable Chrome): engines that render are
    /// left out instead of failing every search ([`Self::available`]).
    unavailable: std::sync::atomic::AtomicBool,
}

impl PersonBrowser {
    /// From the environment; `None` when the mode is `off` or there is no
    /// profile location.
    pub fn from_env() -> Option<Self> {
        let env = |k: &str| std::env::var(k).ok();
        let mode = Mode::resolve(env, has_display(env));
        if mode == Mode::Off {
            return None;
        }
        let profile = default_profile(env)?;
        let custom = env(BROWSER_PROFILE_ENV).is_some_and(|p| !p.trim().is_empty());
        let home = env("HOME")
            .or_else(|| env("USERPROFILE"))
            .map(PathBuf::from);
        if let Err(e) = check_profile(&profile, !custom, home.as_deref()) {
            tracing::warn!(error = %e, "person's browser off");
            return None;
        }
        let executable = env(CHROME_ENV).filter(|p| !p.is_empty()).map(PathBuf::from);
        Some(Self::new(mode, profile, executable))
    }

    pub fn new(mode: Mode, profile: PathBuf, executable: Option<PathBuf>) -> Self {
        Self {
            mode,
            profile,
            executable,
            session: Arc::new(Mutex::new(None)),
            last_used: Arc::new(StdMutex::new(Instant::now())),
            shown: StdMutex::new(HashMap::new()),
            unavailable: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    /// Connect to the browser already running on this profile, or launch it.
    async fn ensure(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Session>>, String> {
        self.touch();
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            // A connected browser may have been closed by its owner.
            if s.browser.version().await.is_ok() {
                return Ok(guard);
            }
            *guard = None;
        }
        let session = match self.connect().await {
            Some(s) => s,
            None => self.launch(self.mode.loads_headless()).await?,
        };
        *guard = Some(session);
        Ok(guard)
    }

    async fn connect(&self) -> Option<Session> {
        let file = std::fs::read_to_string(self.profile.join("DevToolsActivePort")).ok()?;
        let url = devtools_ws_url(&file)?;
        let (browser, mut handler) =
            tokio::time::timeout(Duration::from_secs(3), Browser::connect(url))
                .await
                .ok()?
                .ok()?;
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let headless = browser
            .version()
            .await
            .map(|v| v.user_agent.contains("HeadlessChrome"))
            .unwrap_or(true);
        Some(Session {
            browser,
            handler,
            launched: false,
            headless,
        })
    }

    async fn launch(&self, headless: bool) -> Result<Session, String> {
        std::fs::create_dir_all(&self.profile)
            .map_err(|e| format!("browser profile {}: {e}", self.profile.display()))?;
        let _ = std::fs::write(
            self.profile.join(PROFILE_MARKER),
            "A browser profile octos uses for searching (octos_research::browser).\n",
        );
        static NOTICE: std::sync::Once = std::sync::Once::new();
        NOTICE.call_once(|| {
            tracing::warn!(profile = %self.profile.display(), "{}", crate::BROWSER_SEARCH_NOTICE);
        });
        // `with_head`: the `headless` switch, when wanted, is in
        // `launch_args`; chromiumoxide would add its own flags otherwise.
        let mut builder = BrowserConfig::builder()
            .with_head()
            .disable_default_args()
            .user_data_dir(&self.profile)
            .launch_timeout(Duration::from_secs(20))
            .args(launch_args(headless));
        if let Some(exe) = &self.executable {
            builder = builder.chrome_executable(exe);
        }
        let config = builder.build().map_err(|e| {
            self.unavailable
                .store(true, std::sync::atomic::Ordering::Relaxed);
            format!("no browser: {e}")
        })?;
        let launched = Browser::launch(config).await;
        self.unavailable
            .store(launched.is_err(), std::sync::atomic::Ordering::Relaxed);
        let (browser, mut handler) = launched.map_err(|e| {
            format!(
                "could not start the browser (is a Chrome already open on {}?): {e}",
                self.profile.display()
            )
        })?;
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        self.close_when_idle();
        Ok(Session {
            browser,
            handler,
            launched: true,
            headless,
        })
    }

    /// Close a browser this process launched once it has gone unused for
    /// [`IDLE_CLOSE`].
    fn close_when_idle(&self) {
        let session = self.session.clone();
        let last_used = self.last_used.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let idle = last_used
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .elapsed();
                if idle < IDLE_CLOSE {
                    continue;
                }
                let mut guard = session.lock().await;
                if let Some(s) = guard.take() {
                    close_session(s).await;
                }
                return;
            }
        });
    }

    /// Load `url` in a background tab and return the page after it settles.
    pub async fn render(&self, url: &str, timeout: Duration) -> Result<HttpResponse, String> {
        let page = {
            let guard = self.ensure().await?;
            let session = guard.as_ref().ok_or("browser not running")?;
            let mut params = CreateTargetParams::new("about:blank");
            params.background = Some(true);
            let page = session
                .browser
                .new_page(params)
                .await
                .map_err(|e| format!("browser tab: {e}"))?;
            if self.mode == Mode::Window && session.launched {
                // Out of the person's way until a page needs them.
                let _ = set_window_state(&page, WindowState::Minimized).await;
            }
            page
        };
        let loaded = tokio::time::timeout(timeout, async {
            page.goto(url)
                .await
                .map_err(|e| format!("browser load: {e}"))?;
            tokio::time::sleep(SETTLE).await;
            let mut html = page
                .content()
                .await
                .map_err(|e| format!("browser read: {e}"))?;
            // A check that clears itself in a real browser: wait for it
            // (up to ~10 s) rather than report it.
            for _ in 0..INTERSTITIAL_WAITS {
                if !crate::access::is_interstitial(&html) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Ok(h) = page.content().await {
                    html = h;
                }
            }
            let final_url = page
                .url()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| url.to_string());
            Ok::<_, String>((html, final_url))
        })
        .await;
        let _ = page.close().await;
        self.touch();
        let (mut html, final_url) = loaded.map_err(|_| "browser timed out".to_string())??;
        check_final_url(&final_url)?;
        if html.len() > MAX_BODY_BYTES {
            let mut cut = MAX_BODY_BYTES;
            while !html.is_char_boundary(cut) {
                cut -= 1;
            }
            html.truncate(cut);
        }
        Ok(HttpResponse {
            status: 200,
            headers: vec![
                ("content-type".into(), "text/html; charset=utf-8".into()),
                ("x-octos-final-url".into(), final_url),
            ],
            body: html,
        })
    }

    /// Show `url` (a challenge page) to the person: a foreground tab in a
    /// visible window. A headless browser this process launched is closed
    /// (saving the profile) and reopened with a window. `Ok(false)` when
    /// nothing can be shown: no display, `headless` mode, or a headless
    /// browser another process owns.
    pub async fn show(&self, url: &str) -> Result<bool, String> {
        if !self.mode.can_show() {
            return Ok(false);
        }
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .ok_or("bad challenge url")?;
        {
            let mut shown = self.shown.lock().unwrap_or_else(|p| p.into_inner());
            if shown
                .get(&host)
                .is_some_and(|t| t.elapsed() < SHOW_AGAIN_AFTER)
            {
                // Open, or being opened, for this host moments ago.
                return Ok(true);
            }
            shown.insert(host.clone(), Instant::now());
        }
        let r = self.open_for_person(url).await;
        // Only a tab that actually opened counts as shown.
        if !matches!(r, Ok(true)) {
            self.forget_shown(&host);
        }
        r
    }

    async fn open_for_person(&self, url: &str) -> Result<bool, String> {
        let mut guard = self.ensure().await?;
        if guard.as_ref().is_some_and(|s| s.headless) {
            let s = guard.take().expect("checked above");
            if !s.launched {
                *guard = Some(s);
                return Ok(false);
            }
            close_session(s).await;
            *guard = Some(self.launch(false).await?);
        }
        let session = guard.as_ref().ok_or("browser not running")?;
        let page = session
            .browser
            .new_page(url)
            .await
            .map_err(|e| format!("browser tab: {e}"))?;
        let _ = set_window_state(&page, WindowState::Normal).await;
        let _ = page.bring_to_front().await;
        self.touch();
        Ok(true)
    }

    /// Whether the browser can be used: false after it failed to start,
    /// until a later start succeeds.
    pub fn available(&self) -> bool {
        !self.unavailable.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn forget_shown(&self, host: &str) {
        self.shown
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(host);
    }

    /// Close the browser if this process started it, letting Chrome save
    /// the profile (cookies from a cleared challenge, a sign-in).
    pub async fn close(&self) {
        let s = self.session.lock().await.take();
        if let Some(s) = s {
            close_session(s).await;
        }
    }
}

async fn set_window_state(page: &Page, state: WindowState) -> Result<(), String> {
    let win = page
        .execute(GetWindowForTargetParams::default())
        .await
        .map_err(|e| e.to_string())?;
    let bounds = Bounds {
        window_state: Some(state),
        ..Default::default()
    };
    page.execute(SetWindowBoundsParams::new(win.result.window_id, bounds))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// A results page that redirected to a private address is not read (the
/// engine's own URL was checked before loading).
fn check_final_url(final_url: &str) -> Result<(), String> {
    let Ok(u) = url::Url::parse(final_url) else {
        return Ok(());
    };
    match u.host_str() {
        Some(h) if crate::net::is_private_host(h) => {
            Err(format!("browser ended on a private address: {h}"))
        }
        _ => Ok(()),
    }
}

/// A metasearch fetcher: plain requests over HTTP, `render` requests in the
/// person's browser, challenges shown to the person.
pub struct PersonBrowserFetch<F> {
    http: F,
    browser: std::sync::Arc<PersonBrowser>,
}

impl<F: Fetch> PersonBrowserFetch<F> {
    pub fn new(http: F, browser: std::sync::Arc<PersonBrowser>) -> Self {
        Self { http, browser }
    }
}

impl<F: Fetch> Fetch for PersonBrowserFetch<F> {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        self.http.fetch(req)
    }

    fn render(&self, req: HttpRequest) -> FetchFuture<'_> {
        // Headers are not forwarded: the browser sends its own, as it would
        // for the person.
        Box::pin(async move { self.browser.render(&req.url, req.timeout).await })
    }

    fn hand_over(&self, url: String) -> HandOverFuture<'_> {
        // Opening can outlast the caller's budget (a headless browser is
        // closed and reopened with a window). It carries on in the
        // background; the answer is "shown" only if the tab opened in time,
        // and the person is told to open the page otherwise.
        let browser = self.browser.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(browser.show(&url).await.unwrap_or(false));
        });
        Box::pin(async move {
            tokio::time::timeout(HAND_OVER_WAIT, rx)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false)
        })
    }

    /// Not after the browser failed to start: engines that render are then
    /// left out rather than failing every search.
    fn can_render(&self) -> bool {
        self.browser.available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn should_load_headless_and_show_challenges_only_with_a_display() {
        // Off unless asked for: no browser and no person by default.
        assert_eq!(Mode::resolve(env(&[]), true), Mode::Off);
        assert_eq!(Mode::resolve(env(&[]), false), Mode::Off);
        assert_eq!(
            Mode::resolve(env(&[(BROWSER_ENV, "auto")]), true),
            Mode::Auto
        );
        assert_eq!(
            Mode::resolve(env(&[(BROWSER_ENV, "auto")]), false),
            Mode::Headless
        );
        assert_eq!(
            Mode::resolve(env(&[(BROWSER_ENV, "window")]), true),
            Mode::Window
        );
        assert_eq!(
            Mode::resolve(env(&[(BROWSER_ENV, "window")]), false),
            Mode::Headless
        );
        assert_eq!(
            Mode::resolve(env(&[(BROWSER_ENV, "HEADLESS")]), true),
            Mode::Headless
        );
        assert_eq!(Mode::resolve(env(&[(BROWSER_ENV, "off")]), true), Mode::Off);
        assert_eq!(
            Mode::resolve(env(&[(BROWSER_ENV, "sometimes")]), true),
            Mode::Off,
            "unrecognised: off"
        );
        assert!(Mode::Auto.loads_headless() && Mode::Auto.can_show());
        assert!(!Mode::Window.loads_headless() && Mode::Window.can_show());
        assert!(Mode::Headless.loads_headless() && !Mode::Headless.can_show());
    }

    #[test]
    fn should_find_a_display_on_linux_only_when_one_is_set() {
        if cfg!(any(target_os = "macos", windows)) {
            assert!(has_display(env(&[])));
        } else {
            assert!(!has_display(env(&[])));
            assert!(!has_display(env(&[("DISPLAY", "")])));
            assert!(has_display(env(&[("DISPLAY", ":0")])));
            assert!(has_display(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        }
    }

    #[test]
    fn should_keep_the_profile_under_octos_home() {
        assert_eq!(
            default_profile(env(&[("HOME", "/home/p")])),
            Some(PathBuf::from("/home/p/.octos/browser-profile"))
        );
        assert_eq!(
            default_profile(env(&[("HOME", "/home/p"), (BROWSER_PROFILE_ENV, "/x")])),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(default_profile(env(&[])), None);
    }

    #[test]
    fn should_read_the_running_browsers_devtools_port() {
        assert_eq!(
            devtools_ws_url("54321\n/devtools/browser/abc-123\n").as_deref(),
            Some("ws://127.0.0.1:54321/devtools/browser/abc-123")
        );
        assert_eq!(devtools_ws_url("54321\n/elsewhere\n"), None);
        assert_eq!(devtools_ws_url("x\n/devtools/browser/a\n"), None);
        assert_eq!(devtools_ws_url(""), None);
    }

    #[test]
    fn should_never_use_a_browsers_own_profile() {
        let home = Path::new("/home/p");
        for real in [
            "/home/p/.config/google-chrome",
            "/home/p/.config/google-chrome/Default",
            "/home/p/Library/Application Support/Google/Chrome",
            "/home/p/AppData/Local/Microsoft/Edge/User Data",
        ] {
            let err = check_profile(Path::new(real), false, Some(home)).unwrap_err();
            assert!(err.contains("browser's own profile"), "{real}: {err}");
        }
        assert!(
            check_profile(
                Path::new("/home/p/.octos/browser-profile"),
                true,
                Some(home)
            )
            .is_ok()
        );
    }

    #[test]
    fn should_use_a_custom_profile_only_if_new_empty_or_octos() {
        let dir = std::env::temp_dir().join(format!("octos-profile-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(check_profile(&dir, false, None).is_ok(), "new");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(check_profile(&dir, false, None).is_ok(), "empty");
        std::fs::write(dir.join("Local State"), "{}").unwrap();
        assert!(
            check_profile(&dir, false, None)
                .unwrap_err()
                .contains(PROFILE_MARKER)
        );
        assert!(
            check_profile(&dir, true, None).is_ok(),
            "the default location is octos-owned"
        );
        std::fs::write(dir.join(PROFILE_MARKER), "").unwrap();
        assert!(check_profile(&dir, false, None).is_ok(), "marked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn should_leave_the_browser_out_after_it_fails_to_start() {
        let dir = std::env::temp_dir().join(format!("octos-nobrowser-{}", std::process::id()));
        let b = PersonBrowser::new(
            Mode::Headless,
            dir.clone(),
            Some(PathBuf::from("/nonexistent/chrome")),
        );
        assert!(b.available(), "not known to be missing yet");
        assert!(
            b.render("https://example.org/", Duration::from_secs(5))
                .await
                .is_err()
        );
        assert!(!b.available(), "a failed start makes it unavailable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn should_not_claim_a_challenge_is_shown_when_it_could_not_be() {
        let dir = std::env::temp_dir().join(format!("octos-noshow-{}", std::process::id()));
        let b = PersonBrowser::new(
            Mode::Auto,
            dir.clone(),
            Some(PathBuf::from("/nonexistent/chrome")),
        );
        let url = "https://www.google.com/sorry/index";
        assert!(b.show(url).await.is_err());
        // Not remembered as shown: asked again, it tries again (and fails
        // again) instead of answering "shown".
        assert!(b.show(url).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_launch_a_plain_browser() {
        let args = launch_args(false).join(" ");
        for banned in [
            "enable-automation",
            "AutomationControlled",
            "user-agent",
            "headless",
        ] {
            assert!(!args.contains(banned), "{banned} in {args}");
        }
        assert!(launch_args(true).contains(&"headless=new".to_string()));
        // chromiumoxide adds the leading dashes itself.
        assert!(launch_args(true).iter().all(|a| !a.starts_with('-')));
    }

    #[test]
    fn should_not_read_pages_that_end_on_private_addresses() {
        assert!(check_final_url("https://www.google.com/search?q=x").is_ok());
        assert!(check_final_url("http://127.0.0.1/admin").is_err());
        assert!(check_final_url("http://localhost:8080/").is_err());
    }
}
