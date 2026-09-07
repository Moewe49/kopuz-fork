//! In-app YouTube / Google sign-in for desktop (issue: one-click login).
//!
//! The universal, zero-setup way to sign in: no external browser to install, no
//! F12 / "copy the Cookie header from the Network tab", no cookie paste. We host
//! a real `wry` WebView (the same engine the app itself runs on — WebView2 on
//! Windows, WKWebView on macOS, WebKitGTK on Linux, all already present), point
//! it at Google's normal web sign-in, let the user log in, and read the resulting
//! youtube.com cookies straight out of the WebView's cookie store via
//! [`wry::WebView::cookies_for_url`]. Those cookies feed the exact same
//! `persist_yt_session` path every other sign-in method uses.
//!
//! This mirrors the Android in-app sign-in (`android-src/.../YtLogin.kt`), which
//! is already proven in production. The one gotcha both share: Google refuses
//! sign-in inside an *embedded* WebView ("this browser or app may not be secure")
//! when it sniffs an embedded/webview User-Agent. We override the UA with a plain
//! desktop-Chrome string so the standard web sign-in proceeds — and because
//! WebView2 *is* Chromium, Google accepts it just as it does the real browser the
//! CDP path drives.
//!
//! The WebView must live on the main/event-loop thread, so this module is driven
//! entirely from [`pump`], called every tick from the app's
//! `Config::with_custom_event_handler` (exactly like `pot_minter`). The trigger
//! and the captured-cookie result cross threads through `player::systemint`'s
//! login channel, so the settings UI can start + poll it with no direct handle
//! to the WebView.

#![cfg(not(any(target_os = "android", target_os = "ios", target_arch = "wasm32")))]

use std::cell::RefCell;
use std::time::{Duration, Instant};

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::tao::event_loop::EventLoopWindowTarget;
use dioxus::desktop::tao::window::{Window, WindowBuilder, WindowId};
use dioxus::desktop::wry::{WebContext, WebView, WebViewBuilder};

#[cfg(target_os = "linux")]
use dioxus::desktop::tao::platform::unix::WindowExtUnix;

/// Google's normal web sign-in, told to continue to YT Music once signed in — the
/// same URL the CDP browser-login and the Android WebView use.
const SIGNIN_URL: &str =
    "https://accounts.google.com/ServiceLogin?continue=https%3A%2F%2Fmusic.youtube.com%2F";
/// Cookies are read for this origin: it's where the app's InnerTube calls go, so
/// this returns exactly the jar those requests will send.
const COOKIE_URL: &str = "https://music.youtube.com";
/// A plain desktop-Chrome UA so Google serves the standard sign-in instead of the
/// "this browser may not be secure" block it shows embedded WebViews. Matches the
/// Android login's UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// Give up (report a cancel) if no signed-in session appears in this long, so a
/// window left open forever doesn't wedge the settings poll.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(360);
/// How often to check the cookie jar — WebView2's cookie read is a UI-thread call
/// and the event loop ticks far more often than we need to poll.
const POLL_EVERY: Duration = Duration::from_millis(800);

/// The live sign-in window, kept on the main thread for its lifetime. Dropping it
/// closes the window and tears down the WebView.
struct Session {
    window_id: WindowId,
    _window: Window,
    webview: WebView,
    _web_context: WebContext,
    deadline: Instant,
    last_poll: Instant,
}

thread_local! {
    static STATE: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Called every event-loop tick from the custom event handler. Opens the sign-in
/// window when [`player::systemint::start_yt_login`] has been requested, reacts to
/// the user closing it, and polls its cookie jar until a signed-in session
/// appears (or we time out).
pub fn pump<T: 'static>(event: &Event<'_, T>, target: &EventLoopWindowTarget<T>) {
    open_if_wanted(target);

    // The user closed the sign-in window before a session was captured → cancel.
    if let Event::WindowEvent {
        window_id,
        event: WindowEvent::CloseRequested,
        ..
    } = event
    {
        let ours = STATE.with(|s| s.borrow().as_ref().map(|sess| sess.window_id) == Some(*window_id));
        if ours {
            STATE.with(|s| *s.borrow_mut() = None);
            player::systemint::set_yt_login_result(String::new());
        }
    }

    poll();
}

/// Stand up the sign-in window + WebView once a login has been requested and none
/// is already open. On any hard failure, report a cancel so the settings poll
/// stops waiting.
fn open_if_wanted<T: 'static>(target: &EventLoopWindowTarget<T>) {
    if !player::systemint::take_yt_login_want() {
        return;
    }
    if STATE.with(|s| s.borrow().is_some()) {
        // A window is already open — treat the extra request as a no-op rather
        // than stacking a second window.
        return;
    }

    let window = match WindowBuilder::new()
        .with_title("Sign in to YouTube Music")
        .with_inner_size(LogicalSize::new(480.0, 720.0))
        .with_focused(true)
        .build(target)
    {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("[yt-login] window build failed: {e}");
            player::systemint::set_yt_login_result(String::new());
            return;
        }
    };
    let window_id = window.id();

    // A dedicated, persistent profile so a returning user may land already
    // signed in, and so a future silent refresh could reuse it. Kept separate
    // from the app's own WebView data dir (two WebView2 environments must not
    // share one user-data folder).
    let profile = directories::ProjectDirs::from("com", "temidaradev", "kopuz")
        .map(|dirs| dirs.cache_dir().join("yt-login"))
        .unwrap_or_else(|| std::env::temp_dir().join("kopuz-yt-login"));
    let _ = std::fs::create_dir_all(&profile);
    let mut web_context = WebContext::new(Some(profile));

    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(SIGNIN_URL)
        .with_user_agent(UA);

    #[cfg(target_os = "linux")]
    let built = match window.default_vbox() {
        Some(vbox) => {
            use dioxus::desktop::wry::WebViewBuilderExtUnix;
            builder.build_gtk(vbox)
        }
        None => {
            tracing::error!("[yt-login] no GTK vbox on window");
            player::systemint::set_yt_login_result(String::new());
            return;
        }
    };
    #[cfg(not(target_os = "linux"))]
    let built = builder.build(&window);

    let webview = match built {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("[yt-login] webview build failed: {e}");
            player::systemint::set_yt_login_result(String::new());
            return;
        }
    };

    let now = Instant::now();
    STATE.with(|s| {
        *s.borrow_mut() = Some(Session {
            window_id,
            _window: window,
            webview,
            _web_context: web_context,
            deadline: now + LOGIN_TIMEOUT,
            last_poll: now,
        });
    });
    tracing::info!("[yt-login] in-app sign-in window opened");
}

/// What the current poll decided, computed while borrowing `STATE`, then applied
/// after the borrow is released (so we can drop the session cleanly).
enum Outcome {
    /// Keep waiting.
    Pending,
    /// Signed in — the normalized `Cookie:` header.
    SignedIn(String),
    /// Time ran out; report a cancel.
    TimedOut,
}

fn poll() {
    let outcome = STATE.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(sess) = guard.as_mut() else {
            return Outcome::Pending;
        };
        if Instant::now() >= sess.deadline {
            return Outcome::TimedOut;
        }
        if sess.last_poll.elapsed() < POLL_EVERY {
            return Outcome::Pending;
        }
        sess.last_poll = Instant::now();

        let Ok(cookies) = sess.webview.cookies_for_url(COOKIE_URL) else {
            return Outcome::Pending;
        };
        let header = cookies
            .iter()
            .filter_map(|c| {
                let (name, value) = (c.name(), c.value());
                (!name.is_empty() && !value.is_empty()).then(|| format!("{name}={value}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
        // `sanitize_header` both validates the session is actually signed in
        // (SAPISID/SID present) and normalizes the header — the same gate the
        // paste path uses, so a captured session is indistinguishable from a
        // pasted one downstream.
        match ::server::ytmusic::manual_cookies::sanitize_header(&header) {
            Ok(h) => Outcome::SignedIn(h),
            Err(_) => Outcome::Pending,
        }
    });

    match outcome {
        Outcome::Pending => {}
        Outcome::SignedIn(header) => {
            STATE.with(|s| *s.borrow_mut() = None);
            player::systemint::set_yt_login_result(header);
            tracing::info!("[yt-login] captured a signed-in session");
        }
        Outcome::TimedOut => {
            STATE.with(|s| *s.borrow_mut() = None);
            player::systemint::set_yt_login_result(String::new());
            tracing::warn!("[yt-login] timed out waiting for sign-in");
        }
    }
}
