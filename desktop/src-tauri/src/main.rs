// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! # InferenceHub Desktop — sidecar strategy
//!
//! ## Dev mode (debug_assertions OR IH_DEV_SIDECAR env var)
//!
//! The Next.js standalone output lives in the monorepo at
//! `../../web/.next/standalone/`.  Next standalone with a sub-directory app
//! may place the entry at either:
//!   - `<standalone>/server.js`           (flat, single-app workspace)
//!   - `<standalone>/web/server.js`       (monorepo hoisted output)
//!
//! We probe both at runtime and pick whichever exists (see `resolve_dev_server_js`).
//! Node is assumed to be on PATH.
//!
//! ## Release mode
//!
//! CI stages two items into `src-tauri/resources/`:
//!   - `resources/node`          — bundled Node.js binary (platform-matched)
//!   - `resources/standalone/`  — Next.js standalone build tree
//!
//! At runtime we resolve both via `tauri::Manager::path().resource_dir()`.
//!
//! The `bundle.resources` key in tauri.conf.json tells Tauri to copy
//! `resources/**/*` into the bundle so they are available via resource_dir().

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read as IoRead, Write as IoWrite};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime};
use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, Menu, MenuBuilder, MenuItem, MenuItemKind, PredefinedMenuItem, SubmenuBuilder,
    HELP_SUBMENU_ID,
};
use tauri::tray::TrayIconBuilder;
#[cfg(not(target_os = "macos"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::Wry;
use tauri::{
    webview::PageLoadPayload, AppHandle, Manager, Webview, WebviewUrl, WebviewWindowBuilder,
};
use url::Url;
#[cfg(target_os = "macos")]
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

// ============================================================================
// Constants
// ============================================================================

const TRAY_ID: &str = "inferencehub-tray";
const TRAY_ICON_BYTES: &[u8] = include_bytes!("../icons/tray-icon.png");
const TRAY_MENU_OPEN_APP_ID: &str = "tray_open_app";
const TRAY_MENU_OPEN_CHAT_ID: &str = "tray_open_chat";
/// Tray toggle for the budget tray *title* — macOS-only (Windows/Linux trays have no title text).
#[cfg(target_os = "macos")]
const TRAY_MENU_SHOW_IN_BAR_ID: &str = "tray_show_in_menu_bar";
const TRAY_MENU_QUIT_ID: &str = "tray_quit";
const MENU_SHOW_MENU_BAR_ID: &str = "show_menu_bar";
const MENU_ALWAYS_ON_TOP_ID: &str = "always_on_top";
/// Stealth Mode (privacy): blank the window to other processes during screen-share/recording.
/// macOS-only (NSWindow sharingType) — the menu item is not built on other platforms.
#[cfg(target_os = "macos")]
const MENU_STEALTH_MODE_ID: &str = "stealth_mode";
/// Opacity presets (percent) shown under Window > Opacity. Item ids are `opacity_<pct>`.
const OPACITY_PRESETS: &[u8] = &[100, 90, 80, 70, 60, 50];
#[cfg(target_os = "linux")]
const MENU_HIDE_DECORATIONS_ID: &str = "hide_window_decorations";
const MENU_TOGGLE_DEVTOOLS_ID: &str = "toggle_devtools";
const MENU_OPEN_DEBUG_LOG_ID: &str = "open_debug_log";

/// Default InferenceHub-hosted Onyx instance the desktop points at.
/// Override at runtime with the IH_SERVER_URL env var. The backend is hosted
/// (see deploy/onyx/) — this app is a thin client.
const DEFAULT_SERVER_URL: &str = "https://chat.inferencehub.tech";

/// InferenceHub GATEWAY origin (the account/billing app, distinct from the chat origin above). Used by the
/// desktop SSO flow: the gateway mints a short-lived chat token after the user logs in. Override with
/// IH_GATEWAY_URL. See spawn_desktop_sso / docs/ihsso in the inference-hub + inferencehub-chat repos.
const DEFAULT_GATEWAY_URL: &str = "https://app.inferencehub.tech";

fn gateway_base_url() -> String {
    std::env::var("IH_GATEWAY_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string())
}

/// Readiness poll parameters: wait up to 30 s in 250 ms intervals.
const POLL_INTERVAL_MS: u64 = 250;
const POLL_TIMEOUT_SECS: u64 = 30;

const CHAT_LINK_INTERCEPT_SCRIPT: &str = r##"
(() => {
  if (window.__IH_CHAT_LINK_INTERCEPT_INSTALLED__) {
    return;
  }

  window.__IH_CHAT_LINK_INTERCEPT_INSTALLED__ = true;

  function isChatSessionPage() {
    try {
      const currentUrl = new URL(window.location.href);
      return (
        currentUrl.pathname.startsWith("/app") &&
        currentUrl.searchParams.has("chatId")
      );
    } catch {
      return false;
    }
  }

  function getAllowedNavigationUrl(rawUrl) {
    try {
      const parsed = new URL(String(rawUrl), window.location.href);
      const scheme = parsed.protocol.toLowerCase();
      if (!["http:", "https:", "mailto:", "tel:"].includes(scheme)) {
        return null;
      }
      return parsed;
    } catch {
      return null;
    }
  }

  async function openWithTauri(url) {
    try {
      const invoke =
        window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
      if (typeof invoke !== "function") {
        return false;
      }

      await invoke("open_in_browser", { url });
      return true;
    } catch {
      return false;
    }
  }

  function handleChatNavigation(rawUrl) {
    const parsedUrl = getAllowedNavigationUrl(rawUrl);
    if (!parsedUrl) {
      return false;
    }

    const safeUrl = parsedUrl.toString();
    const scheme = parsedUrl.protocol.toLowerCase();
    if (scheme === "mailto:" || scheme === "tel:") {
      void openWithTauri(safeUrl).then((opened) => {
        if (!opened) {
          window.location.assign(safeUrl);
        }
      });
      return true;
    }

    window.location.assign(safeUrl);
    return true;
  }

  document.addEventListener(
    "click",
    (event) => {
      if (!isChatSessionPage() || event.defaultPrevented) {
        return;
      }

      const element = event.target;
      if (!(element instanceof Element)) {
        return;
      }

      const anchor = element.closest("a");
      if (!(anchor instanceof HTMLAnchorElement)) {
        return;
      }

      const target = (anchor.getAttribute("target") || "").toLowerCase();
      if (target !== "_blank") {
        return;
      }

      const href = anchor.getAttribute("href");
      if (!href || href.startsWith("#")) {
        return;
      }

      if (!handleChatNavigation(href)) {
        return;
      }

      event.preventDefault();
      event.stopPropagation();
    },
    true
  );

  const nativeWindowOpen = window.open;
  window.open = function(url, target, features) {
    const resolvedTarget = typeof target === "string" ? target.toLowerCase() : "";
    const shouldNavigateInPlace = resolvedTarget === "" || resolvedTarget === "_blank";

    if (
      isChatSessionPage() &&
      shouldNavigateInPlace &&
      url != null &&
      String(url).length > 0
    ) {
      if (!handleChatNavigation(url)) {
        return null;
      }
      return null;
    }

    if (typeof nativeWindowOpen === "function") {
      return nativeWindowOpen.call(window, url, target, features);
    }
    return null;
  };
})();
"##;

#[cfg(not(target_os = "macos"))]
const MENU_KEY_HANDLER_SCRIPT: &str = r#"
(() => {
  if (window.__IH_MENU_KEY_HANDLER__) return;
  window.__IH_MENU_KEY_HANDLER__ = true;

  let altPressedAlone = false;

  document.addEventListener('keydown', (e) => {
    altPressedAlone = e.key === 'Alt' && !e.repeat;
  }, true);

  document.addEventListener('keyup', (e) => {
    if (e.key !== 'Alt' || !altPressedAlone) return;
    altPressedAlone = false;
    e.preventDefault();
    const invoke =
      window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
    if (typeof invoke === 'function') invoke('toggle_menu_bar');
  }, true);
})();
"#;

const CONSOLE_CAPTURE_SCRIPT: &str = r#"
(() => {
  if (window.__IH_CONSOLE_CAPTURE__) return;
  window.__IH_CONSOLE_CAPTURE__ = true;

  const levels = ['log', 'warn', 'error', 'info', 'debug'];
  const originals = {};

  levels.forEach(level => {
    originals[level] = console[level];
    console[level] = function(...args) {
      originals[level].apply(console, args);
      try {
        const invoke =
          window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
        if (typeof invoke === 'function') {
          const message = args.map(a => {
            try { return typeof a === 'string' ? a : JSON.stringify(a); }
            catch { return String(a); }
          }).join(' ');
          invoke('log_from_frontend', { level, message });
        }
      } catch {}
    };
  });

  window.addEventListener('error', (event) => {
    try {
      const invoke =
        window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
      if (typeof invoke === 'function') {
        invoke('log_from_frontend', {
          level: 'error',
          message: `[uncaught] ${event.message} at ${event.filename}:${event.lineno}:${event.colno}`
        });
      }
    } catch {}
  });

  window.addEventListener('unhandledrejection', (event) => {
    try {
      const invoke =
        window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
      if (typeof invoke === 'function') {
        invoke('log_from_frontend', {
          level: 'error',
          message: `[unhandled rejection] ${event.reason}`
        });
      }
    } catch {}
  });
})();
"#;

/// Full-screen InferenceHub login gate, injected over Onyx's native `/auth/login` page so the user
/// never sees (or types into) the Onyx email/password form. Identity is unified: the only login path
/// is the gateway SSO. The "Log in" button navigates to the `ih-sso.localhost` sentinel, which the
/// `on_navigation` hook intercepts to start the loopback SSO (no IPC — Tauri firewalls __TAURI__ from
/// remote origins, and the app defines no capabilities). Idempotent (guards on the overlay element id),
/// so re-evaluating on each detector tick / page load is safe.
const IH_LOGIN_OVERLAY_SCRIPT: &str = r##"
(() => {
  const ID = "__ih_login_overlay__";
  function build() {
    if (document.getElementById(ID)) return;
    if (!document.body) { document.addEventListener("DOMContentLoaded", build); return; }
    const err = new URLSearchParams(location.search).has("ih_sso_error");
    const root = document.createElement("div");
    root.id = ID;
    root.setAttribute("style", [
      "position:fixed","inset:0","z-index:2147483647",
      "background:#0d0d0d","color:rgba(255,255,255,0.9)",
      "display:flex","flex-direction:column","align-items:center","justify-content:center",
      "gap:22px","font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif",
      "-webkit-user-select:none","user-select:none"
    ].join(";"));
    root.innerHTML = `
      <svg width="56" height="56" viewBox="0 0 64 64" fill="none" xmlns="http://www.w3.org/2000/svg">
        <g fill="rgba(255,255,255,0.9)">
          <path d="M8 11 L24 11 L33.5 26.5 L27 29.5 Z"/>
          <path d="M8 53 L24 53 L33.5 37.5 L27 34.5 Z"/>
          <path d="M56 11 L40 11 L30.5 26.5 L37 29.5 Z"/>
          <path d="M56 53 L40 53 L30.5 37.5 L37 34.5 Z"/>
        </g>
        <path d="M27 30.2 L36 32 L36 34 L27 32.2 Z" fill="#a6e22e"/>
      </svg>
      <div style="font-size:20px;font-weight:600;letter-spacing:-0.01em">InferenceHub</div>
      <div style="font-size:13px;color:rgba(255,255,255,0.45);max-width:300px;text-align:center;line-height:1.5">
        ${err ? "Sign-in didn’t complete. Please try again." : "Log in with your InferenceHub account to start chatting."}
      </div>
      <button id="__ih_login_btn__" style="appearance:none;border:0;border-radius:10px;cursor:pointer;padding:11px 22px;font-size:14px;font-weight:600;background:#a6e22e;color:#0d0d0d;letter-spacing:0.01em">Log in to InferenceHub</button>
      <div style="font-size:11px;color:rgba(255,255,255,0.3)">Opens your browser to sign in securely</div>
    `;
    document.body.appendChild(root);
    const btn = root.querySelector("#__ih_login_btn__");
    btn.addEventListener("click", () => {
      btn.disabled = true;
      btn.textContent = "Opening your browser…";
      btn.style.opacity = "0.6";
      window.location.href = "http://ih-sso.localhost/start";
    });
  }
  build();
})();
"##;

// ============================================================================
// App config (window/menu preferences only — no server URL)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_show_menu_bar")]
    pub show_menu_bar: bool,

    #[serde(default)]
    pub hide_window_decorations: bool,

    /// Keep the window floating above all other apps' windows.
    #[serde(default)]
    pub always_on_top: bool,

    /// Stealth Mode: mark the window's backing store unreadable by other processes
    /// (NSWindow.sharingType = .none) so screen-sharers/recorders see a black rectangle
    /// instead of the chat contents. macOS-only effect; no-op on Windows/Linux.
    /// Manual toggle — there is no public macOS API to auto-detect active screen capture,
    /// so a reliable auto-blank is not available (a flaky detector would silently leak).
    #[serde(default)]
    pub stealth_mode: bool,

    /// Window opacity as a percent (50..=100). 100 = fully opaque. macOS-only effect.
    #[serde(default = "default_window_opacity")]
    pub window_opacity: u8,

    /// Opaque per-install id (uuid v4), generated once and persisted. Sent as a coarse, non-identifying
    /// attribution param on the desktop SSO request (X-IH-Client analogue). Empty until first run.
    #[serde(default)]
    pub install_id: String,

    /// READ-ONLY gateway status token for the plan-status poller (menubar budget). A bearer credential in
    /// plaintext config.json — ACCEPTED for v1: its blast radius is the plan summary only (no key
    /// management / checkout reachable), it's revocable via password reset, and it shares the trust
    /// boundary of the webview's persisted session cookie. Keychain is a follow-up if posture changes.
    /// Never logged. Empty = signed out / feature idle.
    #[serde(default)]
    pub status_token: String,

    /// Show the remaining-budget title next to the tray icon (macOS menu bar). Off = icon only —
    /// the privacy switch for screen-share (dollars visible on stream; same concern as Stealth Mode).
    #[serde(default = "default_show_menu_bar")]
    pub show_budget_in_tray: bool,
}

fn default_show_menu_bar() -> bool {
    true
}

fn default_window_opacity() -> u8 {
    100
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            show_menu_bar: true,
            hide_window_decorations: false,
            always_on_top: false,
            stealth_mode: false,
            window_opacity: 100,
            install_id: String::new(),
            status_token: String::new(),
            show_budget_in_tray: true,
        }
    }
}

const CONFIG_FILE_NAME: &str = "config.json";

fn get_config_dir() -> Option<PathBuf> {
    // Use the Tauri identifier as the config dir name for consistency.
    // On macOS: ~/Library/Application Support/tech.inferencehub.app/
    directories::ProjectDirs::from("tech", "inferencehub", "inferencehub-desktop")
        .map(|d| d.config_dir().to_path_buf())
}

fn get_config_path() -> Option<PathBuf> {
    get_config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

fn load_config() -> AppConfig {
    let path = match get_config_path() {
        Some(p) => p,
        None => return AppConfig::default(),
    };
    if !path.exists() {
        return AppConfig::default();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(config: &AppConfig) -> Result<(), String> {
    let dir = get_config_dir().ok_or("Could not determine config directory")?;
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create config dir: {}", e))?;
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Serialize error: {}", e))?;
    fs::write(dir.join(CONFIG_FILE_NAME), json)
        .map_err(|e| format!("Write error: {}", e))
}

// ============================================================================
// Sidecar resolution
// ============================================================================

/// Returns the Next.js standalone `server.js` entry and the `node` binary path
/// for **dev mode** (debug build or IH_DEV_SIDECAR env var set).
///
/// Searches for `server.js` in:
///   1. `<repo_root>/web/.next/standalone/server.js`      (single-app)
///   2. `<repo_root>/web/.next/standalone/web/server.js`  (monorepo hoisted)
fn resolve_dev_sidecar() -> Option<(PathBuf, PathBuf)> {
    // main.rs lives at desktop/src-tauri/src/main.rs
    // repo root is four directories up: src → src-tauri → desktop → inferencehub-desktop
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // .../desktop/src-tauri
    let repo_root = manifest_dir
        .parent()? // desktop/
        .parent()?; // inferencehub-desktop/

    let standalone_root = repo_root.join("web").join(".next").join("standalone");

    let candidates = [
        standalone_root.join("server.js"),
        standalone_root.join("web").join("server.js"),
    ];

    let server_js = candidates.into_iter().find(|p| p.exists())?;

    // Use `node` from PATH in dev.
    let node = PathBuf::from("node");

    Some((node, server_js))
}

/// Returns `(node_binary, server_js)` for **release** builds.
/// Both are expected inside the Tauri resource directory staged by CI.
///
/// Layout inside resource dir:
///   resources/node           — bundled Node binary
///   resources/standalone/    — Next.js standalone tree (server.js at root)
fn resolve_release_sidecar(app: &AppHandle) -> Option<(PathBuf, PathBuf)> {
    // TODO(verify): confirm that tauri::Manager::path().resource_dir() resolves
    // correctly on all three platforms after CI staging.
    let res_dir = app.path().resource_dir().ok()?;

    let node = res_dir.join("resources").join("node");
    let standalone = res_dir.join("resources").join("standalone");

    // Probe both flat and monorepo-hoisted layouts in the bundled standalone.
    let candidates = [
        standalone.join("server.js"),
        standalone.join("web").join("server.js"),
    ];
    let server_js = candidates.into_iter().find(|p| p.exists())?;

    Some((node, server_js))
}

// ============================================================================
// Native STT helper (live transcription bridge)
// ============================================================================
//
// The chat page cannot run any STT engine inside WKWebView (Apple blocks Web
// Speech outside Safari; onnxruntime-web WASM never initializes there), so the
// page triggers this native bridge via cancelled `ih-stt.localhost` navigations
// (same channel as the SSO sentinel — remote origins have no __TAURI__/IPC).
// The Swift helper (resources/ih-stt-helper) streams one JSON object per stdout
// line ({"type":"ready"|"partial"|"final"|"error", ...}); each line is forwarded
// into the page as `window.__ihStt.onEvent(<json>)`, which useLiveTranscribe
// installs before navigating to /start.

/// Resolve the compiled Swift helper: bundle resources in release, the
/// swiftc output staged next to the manifest in dev.
fn resolve_stt_helper(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(res_dir) = app.path().resource_dir() {
        let p = res_dir.join("resources").join("ih-stt-helper");
        if p.exists() {
            return Some(p);
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("ih-stt-helper");
    dev.exists().then_some(dev)
}

/// Forward one helper event into the page. The payload is a JSON line from our
/// own helper; embed it as a JS string via serde_json to keep the eval safe.
fn eval_stt_event(app: &AppHandle, json_line: &str) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let payload = serde_json::to_string(json_line).unwrap_or_else(|_| "\"{}\"".into());
    let _ = window.eval(&format!(
        "window.__ihStt && window.__ihStt.onEvent({payload});"
    ));
}

fn start_stt_helper(app: AppHandle, source: String) {
    {
        let state = app.state::<AppState>();
        let guard = state.stt_child.lock().unwrap();
        if guard.is_some() {
            eprintln!("[IH] STT: helper already running, ignoring start");
            return;
        }
    }
    let Some(helper) = resolve_stt_helper(&app) else {
        eprintln!("[IH] STT: helper binary not found");
        eval_stt_event(
            &app,
            r#"{"type":"error","message":"Transcription helper missing from this build"}"#,
        );
        return;
    };
    let mut cmd = Command::new(helper);
    cmd.arg("--source")
        .arg(&source)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[IH] STT: spawn failed: {e}");
            eval_stt_event(
                &app,
                r#"{"type":"error","message":"Could not start the transcription helper"}"#,
            );
            return;
        }
    };
    let stdout = child.stdout.take();
    {
        let state = app.state::<AppState>();
        *state.stt_child.lock().unwrap() = Some(child);
    }
    eprintln!("[IH] STT: helper started");
    let reader_app = app.clone();
    std::thread::spawn(move || {
        if let Some(stdout) = stdout {
            let reader = std::io::BufReader::new(stdout);
            for line in std::io::BufRead::lines(reader) {
                let Ok(line) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let evt_app = reader_app.clone();
                let _ = reader_app.run_on_main_thread(move || {
                    eval_stt_event(&evt_app, &line);
                });
            }
        }
        // Helper exited (or stdout broke): clear state so a restart works. If
        // the page still has a listener, tell it the session ended.
        eprintln!("[IH] STT: helper exited");
        let leftover = {
            let state = reader_app.state::<AppState>();
            let taken = state.stt_child.lock().unwrap().take();
            taken
        };
        if let Some(mut old) = leftover {
            let _ = old.wait();
        }
        let end_app = reader_app.clone();
        let _ = reader_app.run_on_main_thread(move || {
            eval_stt_event(&end_app, r#"{"type":"stopped"}"#);
        });
    });
}

fn stop_stt_helper(app: &AppHandle) {
    let child = {
        let state = app.state::<AppState>();
        let taken = state.stt_child.lock().unwrap().take();
        taken
    };
    if let Some(mut child) = child {
        eprintln!("[IH] STT: stopping helper");
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ============================================================================
// Sidecar state
// ============================================================================

struct SidecarState {
    child: Mutex<Option<Child>>,
}

impl SidecarState {
    fn new() -> Self {
        Self {
            child: Mutex::new(None),
        }
    }

    fn kill(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("[IH] sidecar killed");
            }
            *guard = None;
        }
    }
}

/// Pick a free TCP port by binding to 127.0.0.1:0 and reading the OS-assigned port.
fn pick_free_port() -> Option<u16> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    // listener drops here, freeing the port
    Some(port)
}

/// Probe readiness by attempting a TCP connect to 127.0.0.1:<port>.
/// A successful connect means the server is listening (even before HTTP is ready
/// to serve the first request).  This avoids any HTTP client dependency.
///
/// Polls every POLL_INTERVAL_MS ms, gives up after POLL_TIMEOUT_SECS seconds.
fn wait_for_port(port: u16) -> bool {
    let addr = format!("127.0.0.1:{}", port);
    let deadline = std::time::Instant::now() + Duration::from_secs(POLL_TIMEOUT_SECS);

    while std::time::Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &addr.parse().unwrap(),
            Duration::from_millis(POLL_INTERVAL_MS),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }
    false
}

// ============================================================================
// Debug helpers
// ============================================================================

fn is_debug_mode() -> bool {
    std::env::args().any(|arg| arg == "--debug") || std::env::var("IH_DEBUG").is_ok()
}

fn get_debug_log_path() -> Option<PathBuf> {
    get_config_dir().map(|d| d.join("frontend_debug.log"))
}

fn init_debug_log_file() -> Option<fs::File> {
    let log_path = get_debug_log_path()?;
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok()
}

fn format_utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let millis = now.subsec_millis();

    let days = total_secs / 86400;
    let secs_of_day = total_secs % 86400;
    let hours = secs_of_day / 3600;
    let mins = (secs_of_day % 3600) / 60;
    let secs = secs_of_day % 60;

    let z = days as i64 + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hours, mins, secs, millis
    )
}

fn inject_console_capture(webview: &Webview) {
    let _ = webview.eval(CONSOLE_CAPTURE_SCRIPT);
}

/// External-link interception. v1 no-op: the app navigates only within the local
/// sidecar (127.0.0.1), so in-webview navigation is the common case. TODO(polish):
/// route external (non-localhost) links to the system browser via
/// tauri-plugin-shell. Defined here so the `on_page_load` hook resolves.
fn inject_chat_link_intercept(_webview: &Webview) {}

fn maybe_open_devtools(app: &AppHandle, window: &tauri::WebviewWindow) {
    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
        let state = app.state::<AppState>();
        if state.debug_mode {
            window.open_devtools();
        }
    }
    #[cfg(not(any(debug_assertions, feature = "devtools")))]
    {
        let _ = (app, window);
    }
}

// ============================================================================
// Plan status (menubar budget) — poll GET /portal/plan-status with the read-only
// status token and render the tightest window as the tray title ("$8.73 · 32m").
// ============================================================================

/// Mirror of the gateway's plan-status JSON. Every field serde-defaults so gateway evolution
/// (new fields, absent optionals) can't break deserialization.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct PlanWindow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    used_micros: i64,
    #[serde(default)]
    cap_micros: i64,
    #[serde(default)]
    resets_in_sec: i64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct PlanCoverage {
    #[serde(default)]
    paused: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct PlanStatus {
    #[serde(default)]
    label: String,
    #[serde(default)]
    windows: Vec<PlanWindow>,
    #[serde(default)]
    coverage: PlanCoverage,
}

#[derive(Debug, Default, Deserialize)]
struct PlanStatusResponse {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    plan: Option<PlanStatus>,
}

fn remaining_micros(w: &PlanWindow) -> i64 {
    (w.cap_micros - w.used_micros).max(0)
}

fn exhausted(w: &PlanWindow) -> bool {
    w.cap_micros > 0 && remaining_micros(w) == 0
}

/// The tightest window by remaining fraction among the given windows; ties broken by the sooner
/// reset. None when no window has a positive cap.
fn tightest_window<'a, I: IntoIterator<Item = &'a PlanWindow>>(windows: I) -> Option<&'a PlanWindow> {
    windows
        .into_iter()
        .filter(|w| w.cap_micros > 0)
        .min_by(|a, b| {
            let fa = remaining_micros(a) as f64 / a.cap_micros as f64;
            let fb = remaining_micros(b) as f64 / b.cap_micros as f64;
            fa.partial_cmp(&fb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.resets_in_sec.cmp(&b.resets_in_sec))
        })
}

fn remaining_fraction(w: &PlanWindow) -> f64 {
    if w.cap_micros <= 0 {
        return f64::INFINITY;
    }
    remaining_micros(w) as f64 / w.cap_micros as f64
}

/// The window the TITLE shows: the tightest window the user can still SPEND from — runway, not
/// obituary. Two exclusions from the plain tightest-fraction pick:
/// - An EXHAUSTED bucket (e.g. the Claude monthly budget at $0 with a 28-day reset) is not runway:
///   requests keep working (they fall back to paid balance / other lanes), so "$0.00 · 28d" reads
///   as "dead for a month" — the wrong signal. Exhausted buckets get the "!" title prefix + a
///   plain-words menu line instead. Only when EVERY window is exhausted does the title show $0
///   (then it's honest), with the soonest reset.
/// - The MONTHLY CEILING is an aggregate backstop, not a pacing lane: any premium burn drags its
///   fraction below the actual daily-driver windows early in the month (the $11.30/$270 case),
///   which would park an unactionable "$258 · 28d" in the menubar. It only takes the title when
///   it's genuinely LOW (<25% left) AND tighter than every pacing lane — the point where it really
///   is what stops everything.
fn title_window(windows: &[PlanWindow]) -> Option<&PlanWindow> {
    let lane = tightest_window(windows.iter().filter(|w| !exhausted(w) && w.id != "monthly_ceiling"));
    let ceiling = windows.iter().find(|w| w.id == "monthly_ceiling" && !exhausted(w) && w.cap_micros > 0);
    match (lane, ceiling) {
        (Some(l), Some(c))
            if remaining_fraction(c) < 0.25 && remaining_fraction(c) < remaining_fraction(l) =>
        {
            Some(c)
        }
        (Some(l), _) => Some(l),
        (None, Some(c)) => Some(c),
        (None, None) => windows.iter().filter(|w| exhausted(w)).min_by_key(|w| w.resets_in_sec),
    }
}

/// Compact reset countdown: minutes under 2 h ("32m"), hours under 48 h ("7h"), else days ("3d").
fn format_reset(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 2 * 3600 {
        format!("{}m", (secs + 59) / 60)
    } else if secs < 48 * 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn format_dollars(micros: i64) -> String {
    format!("${:.2}", micros.max(0) as f64 / 1_000_000.0)
}

/// The full tray title: the title_window's remaining budget + reset ("$8.73 · 32m"), prefixed with
/// "!" when some OTHER bucket is exhausted ("! $58.73 · 43h") — glanceable "something's used up,
/// click for detail" without burying the actual runway.
fn format_tray_title(windows: &[PlanWindow]) -> Option<String> {
    let w = title_window(windows)?;
    let any_exhausted = windows.iter().any(|x| exhausted(x));
    let base = format!("{} · {}", format_dollars(remaining_micros(w)), format_reset(w.resets_in_sec));
    Some(if any_exhausted && !exhausted(w) { format!("! {}", base) } else { base })
}

/// One disabled tray-menu info line per window, e.g. "Open-weight — 5-hour: $8.73 of $10.00 · 32m".
/// An exhausted bucket says it in plain words (mirrors the portal's amber banner).
fn format_window_menu_line(w: &PlanWindow) -> String {
    if exhausted(w) {
        return format!(
            "{}: used up — resets in {} · billing balance now",
            w.label,
            format_reset(w.resets_in_sec)
        );
    }
    format!(
        "{}: {} of {} · {}",
        w.label,
        format_dollars(remaining_micros(w)),
        format_dollars(w.cap_micros),
        format_reset(w.resets_in_sec)
    )
}

/// Apply a poll result to the tray: title (macOS only) + menu info items. MUST run on the main thread.
fn apply_plan_status_to_tray(app: &AppHandle, status: Option<&PlanStatus>, show_title: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    #[cfg(target_os = "macos")]
    {
        let title = match status {
            Some(s) if show_title => format_tray_title(&s.windows),
            _ => None,
        };
        let _ = tray.set_title(title.as_deref());
    }
    #[cfg(not(target_os = "macos"))]
    let _ = show_title; // tray title is macOS-only; the menu below still updates
    if let Ok(menu) = build_tray_menu(app, status) {
        let _ = tray.set_menu(Some(menu));
    }
}

/// Single long-lived poller thread. Re-reads config every tick, so sign-in/sign-out/toggle need no
/// thread lifecycle management. Fail-silent: no token / no plan / offline => blank title, keep going.
fn spawn_plan_status_poller(app: AppHandle) {
    std::thread::spawn(move || {
        let gateway = gateway_base_url();
        let url = format!("{}/portal/plan-status", gateway.trim_end_matches('/'));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        let mut consecutive_failures: u32 = 0;
        let mut last_applied: Option<(Option<PlanStatus>, bool)> = None;

        loop {
            let (token, show) = {
                let state = app.state::<AppState>();
                let cfg = state.config.read().unwrap();
                (cfg.status_token.clone(), cfg.show_budget_in_tray)
            };

            // Compute the desired tray state for this tick; None = blank (signed out / no plan / stale).
            let mut next: Option<Option<PlanStatus>> = None; // outer None = leave as-is (transient error)
            if token.is_empty() {
                next = Some(None);
            } else {
                match agent
                    .get(&url)
                    .header("authorization", &format!("Bearer {}", token))
                    .call()
                {
                    Ok(mut resp) => {
                        consecutive_failures = 0;
                        match resp.body_mut().read_json::<PlanStatusResponse>() {
                            Ok(body) if body.enabled && body.plan.is_some() => next = Some(body.plan),
                            _ => next = Some(None), // subs off / no plan / unparseable => blank, keep token
                        }
                    }
                    Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
                        // Token expired/revoked or account suspended: drop it and blank until the next SSO.
                        let state = app.state::<AppState>();
                        let mut cfg = state.config.write().unwrap();
                        cfg.status_token = String::new();
                        let _ = save_config(&cfg);
                        next = Some(None);
                    }
                    Err(_) => {
                        // Network / 5xx: keep the last title up to ~5 min of failures, then blank.
                        consecutive_failures += 1;
                        if consecutive_failures >= 5 {
                            next = Some(None);
                        }
                    }
                }
            }

            if let Some(status) = next {
                {
                    let state = app.state::<AppState>();
                    *state.latest_plan_status.lock().unwrap() = status.clone();
                }
                // Skip the main-thread hop when nothing changed (no menu flicker).
                let desired = (status, show);
                if last_applied.as_ref() != Some(&desired) {
                    last_applied = Some(desired.clone());
                    let ui_app = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        apply_plan_status_to_tray(&ui_app, desired.0.as_ref(), desired.1);
                    });
                }
            }

            // Sleep ~60s in 1s slices, honoring the poll-now nudge (SSO success / tray toggle).
            // ±5s deterministic jitter from the install id so a fleet doesn't align on the same second.
            let jitter = {
                let state = app.state::<AppState>();
                (state.install_id.bytes().map(|b| b as u64).sum::<u64>() % 11) as i64 - 5
            };
            let ticks = (60 + jitter).max(30);
            for _ in 0..ticks {
                let state = app.state::<AppState>();
                if state.plan_poll_now.swap(false, Ordering::SeqCst) {
                    break;
                }
                drop(state);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

// ============================================================================
// Global app state
// ============================================================================

struct AppState {
    config: RwLock<AppConfig>,
    /// The localhost URL where the sidecar is serving, e.g. http://127.0.0.1:3456
    server_base_url: RwLock<Option<String>>,
    debug_mode: bool,
    debug_log_file: Mutex<Option<fs::File>>,
    /// Opaque per-install id (uuid v4), persisted in AppConfig; sent as a coarse attribution param on SSO.
    install_id: String,
    /// Guards against overlapping SSO runs (a second trigger while one loopback listener is live).
    sso_in_progress: AtomicBool,
    /// Running native STT helper (see spawn_stt_helper), killed on stop/reload.
    stt_child: Mutex<Option<Child>>,
    /// Last successful plan-status poll (menubar budget) — read by the tray toggle for an instant
    /// title restore without waiting for the next poll.
    latest_plan_status: Mutex<Option<PlanStatus>>,
    /// Set to make the budget poller skip the remainder of its sleep (SSO success, tray toggle).
    plan_poll_now: AtomicBool,
}

// ============================================================================
// Window helpers
// ============================================================================

fn focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    } else {
        trigger_new_window(app);
    }
}

fn trigger_new_chat(app: &AppHandle) {
    let state = app.state::<AppState>();
    let base = state.server_base_url.read().unwrap().clone();
    if let Some(window) = app.get_webview_window("main") {
        let url = format!("{}/chat", base.unwrap_or_default());
        let _ = window.eval(&format!("window.location.href = '{}'", url));
    }
}

fn trigger_new_window(app: &AppHandle) {
    let state = app.state::<AppState>();
    let base = state
        .server_base_url
        .read()
        .unwrap()
        .clone()
        .unwrap_or_default();
    let handle = app.clone();

    tauri::async_runtime::spawn(async move {
        let window_label = format!("ih-{}", uuid::Uuid::new_v4());
        let builder = WebviewWindowBuilder::new(
            &handle,
            &window_label,
            WebviewUrl::External(base.parse().unwrap_or("http://127.0.0.1".parse().unwrap())),
        )
        .title("InferenceHub")
        .inner_size(1200.0, 800.0)
        .min_inner_size(800.0, 600.0);

        // Standard decorated window (native, draggable title bar).
        if let Ok(window) = builder.build() {
            apply_settings_to_window(&handle, &window);
            maybe_open_devtools(&handle, &window);
            let _ = window.set_focus();
        }
    });
}

fn open_in_default_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return Command::new("open").arg(url).status().is_ok();
    }
    #[cfg(target_os = "linux")]
    {
        return Command::new("xdg-open").arg(url).status().is_ok();
    }
    #[cfg(target_os = "windows")]
    {
        return Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .status()
            .is_ok();
    }
    #[allow(unreachable_code)]
    false
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_chat_session_url(url: &Url) -> bool {
    url.path().starts_with("/app") && url.query_pairs().any(|(key, _)| key == "chatId")
}

fn should_open_in_external_browser(current_url: &Url, destination_url: &Url) -> bool {
    if !is_chat_session_url(current_url) {
        return false;
    }
    match destination_url.scheme() {
        "mailto" | "tel" => true,
        "http" | "https" => !same_origin(current_url, destination_url),
        _ => false,
    }
}

// ============================================================================
// Desktop SSO (loopback) — log into the hosted chat as the InferenceHub identity
// ============================================================================
//
// Google bans OAuth inside embedded webviews, so login runs in the user's SYSTEM browser and the chat
// token comes back over a one-shot loopback HTTP listener. Flow:
//   1. bind 127.0.0.1:<ephemeral> ; open the system browser to
//      ${gateway}/portal/desktop-auth?redirect=http://127.0.0.1:<port>/cb&client=…&install_id=…
//   2. the gateway runs the user through its existing login, then 302s the chat token to our loopback
//   3. we read the token and navigate the MAIN webview to ${chat}/api/auth/ih-sso?token=… ; the Onyx
//      bridge verifies it and sets the persistent fastapiusersauth cookie — the user is now logged in.
// Nothing here is exposed to the webview (no new command / capability); open uses std::process::Command.

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The tokens the gateway 302s to the loopback: the (required) short-lived chat SSO token and the
/// (optional — old gateways / mint failure) long-lived read-only status token for the budget poller.
struct SsoTokens {
    chat: String,
    status: Option<String>,
}

/// Extract the `token` (+ optional `status_token`) query params from a raw HTTP request's first line
/// ("GET /cb?token=…&status_token=… HTTP/1.1"). None while `token` is absent/empty (keep waiting).
fn parse_tokens_from_request(req: &str) -> Option<SsoTokens> {
    let target = req.lines().next()?.split_whitespace().nth(1)?; // "/cb?token=…"
    let query = target.split_once('?')?.1;
    let mut chat: Option<String> = None;
    let mut status: Option<String> = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "token" if !v.is_empty() => chat = Some(v.into_owned()),
            "status_token" if !v.is_empty() => status = Some(v.into_owned()),
            _ => {}
        }
    }
    chat.map(|chat| SsoTokens { chat, status })
}

/// Open the system browser to the gateway desktop-auth URL and block (up to 120 s) on a one-shot loopback
/// listener for the redirected tokens. Returns them, or None on timeout / browser-open failure / abort.
/// Runs on a dedicated thread (blocking sockets); never on the main thread.
fn run_loopback_sso(gateway_base: &str, chat_base: &str, install_id: &str) -> Option<SsoTokens> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?; // bind first -> the port can't race
    let port = listener.local_addr().ok()?.port();
    listener.set_nonblocking(true).ok()?;

    let redirect = format!("http://127.0.0.1:{}/cb", port);
    let url = format!(
        "{}/portal/desktop-auth?redirect={}&client={}&install_id={}",
        gateway_base.trim_end_matches('/'),
        urlencode(&redirect),
        urlencode(&format!("InferenceHub-Desktop/{}", env!("CARGO_PKG_VERSION"))),
        urlencode(install_id),
    );
    eprintln!("[IH] SSO: opening system browser (loopback :{}) to log into {}", port, chat_base);
    if !open_in_default_browser(&url) {
        eprintln!("[IH] SSO: failed to open the system browser");
        return None;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let tokens = parse_tokens_from_request(&String::from_utf8_lossy(&buf[..n]));
                let body = if tokens.is_some() {
                    "<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;padding:3rem;text-align:center\"><h2>Signed in to InferenceHub</h2><p>You can close this tab and return to the app.</p>"
                } else {
                    "<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;padding:3rem;text-align:center\"><h2>Sign-in didn’t complete</h2><p>You can close this tab and try again from the app.</p>"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                if tokens.is_some() {
                    return tokens; // never logged — bearer credentials
                }
                // a request without a token (e.g. a stray /favicon.ico) — keep waiting for the real callback
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(_) => return None,
        }
    }
    eprintln!("[IH] SSO: timed out waiting for the loopback callback");
    None
}

/// Kick off the loopback SSO on a background thread and, on success, navigate the main webview to the Onyx
/// bridge (which sets the session cookie). Guarded so overlapping triggers are no-ops.
fn spawn_desktop_sso(app: AppHandle) {
    let chat_base;
    let install_id;
    {
        let state = app.state::<AppState>();
        if state.sso_in_progress.swap(true, Ordering::SeqCst) {
            return; // a run is already in flight
        }
        // Statements (not a tuple tail-expression) so the RwLockReadGuard temporary drops before `state`.
        chat_base = state
            .server_base_url
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        install_id = state.install_id.clone();
    }
    let gateway_base = gateway_base_url();

    std::thread::spawn(move || {
        let tokens = run_loopback_sso(&gateway_base, &chat_base, &install_id);
        app.state::<AppState>().sso_in_progress.store(false, Ordering::SeqCst);
        let Some(tokens) = tokens else { return };
        // Persist the read-only status token (if the gateway minted one) and nudge the budget poller so
        // the menubar title appears seconds after sign-in, not at the next 60s tick. Never logged.
        if let Some(status) = tokens.status {
            let state = app.state::<AppState>();
            {
                let mut cfg = state.config.write().unwrap();
                cfg.status_token = status;
                let _ = save_config(&cfg);
            }
            state.plan_poll_now.store(true, Ordering::SeqCst);
        }
        let bridge = format!(
            "{}/api/auth/ih-sso?token={}",
            chat_base.trim_end_matches('/'),
            urlencode(&tokens.chat)
        );
        let nav_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(window) = nav_app.get_webview_window("main") {
                match bridge.parse::<tauri::Url>() {
                    Ok(u) => {
                        let _ = window.navigate(u);
                    }
                    Err(e) => eprintln!("[IH] SSO: bad bridge URL: {}", e),
                }
            }
        });
    });
}

/// Watch the main webview's URL and show the InferenceHub login gate when it lands on Onyx's login route.
///
/// Onyx (Next.js) gates auth CLIENT-SIDE: the chat root `/` returns 200 (no server redirect) and the
/// router pushes an unauthenticated user to `/auth/login` via history.pushState — with NO full page
/// load. So the `on_page_load` hook never sees that client-side bounce. We poll `WebviewWindow::url()`
/// from Rust (WKWebView's `url` reflects history-API changes) on the main thread and, when the path is
/// `/auth/login`, inject the IH login overlay (masking Onyx's native email/password form). The overlay
/// itself is idempotent, so re-evaluating each tick is harmless; the login is started explicitly by the
/// user clicking the overlay button (→ `ih-sso.localhost` sentinel → `on_navigation` → `spawn_desktop_sso`).
///
/// Bounded (~90s): a logged-in launch goes straight to chat (never hits `/auth/login`), so it just polls
/// a few times and exits. Full-page `/auth/login` loads (initial direct nav, or the bridge's
/// `ih_sso_error` redirect) are covered separately by the `on_page_load` hook.
fn spawn_login_redirect_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        for _ in 0..130 {
            std::thread::sleep(Duration::from_millis(700));
            let probe = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(w) = probe.get_webview_window("main") {
                    if let Ok(u) = w.url() {
                        if u.path().starts_with("/auth/login") {
                            let _ = w.eval(IH_LOGIN_OVERLAY_SCRIPT);
                        }
                    }
                }
            });
        }
    });
}

// ============================================================================
// Tauri Commands
// ============================================================================

#[tauri::command]
fn open_in_browser(url: String) -> Result<(), String> {
    let parsed = Url::parse(&url).map_err(|_| "Invalid URL".to_string())?;
    match parsed.scheme() {
        "http" | "https" | "mailto" | "tel" => {}
        _ => return Err("Unsupported URL scheme".to_string()),
    }
    if open_in_default_browser(parsed.as_str()) {
        Ok(())
    } else {
        Err("Failed to open URL in default browser".to_string())
    }
}

#[tauri::command]
fn reload_page(window: tauri::WebviewWindow) {
    let _ = window.eval("window.location.reload()");
}

#[tauri::command]
fn go_back(window: tauri::WebviewWindow) {
    let _ = window.eval("window.history.back()");
}

#[tauri::command]
fn go_forward(window: tauri::WebviewWindow) {
    let _ = window.eval("window.history.forward()");
}

#[tauri::command]
async fn new_window(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let base = state
        .server_base_url
        .read()
        .unwrap()
        .clone()
        .unwrap_or_default();
    let window_label = format!("ih-{}", uuid::Uuid::new_v4());

    let builder = WebviewWindowBuilder::new(
        &app,
        &window_label,
        WebviewUrl::External(
            base.parse().map_err(|e| format!("Invalid URL: {}", e))?,
        ),
    )
    .title("InferenceHub")
    .inner_size(1200.0, 800.0)
    .min_inner_size(800.0, 600.0);

    // Standard decorated window (native, draggable title bar).
    let window = builder.build().map_err(|e| e.to_string())?;

    apply_settings_to_window(&app, &window);
    maybe_open_devtools(&app, &window);

    Ok(())
}

#[tauri::command]
async fn start_drag_window(window: tauri::Window) -> Result<(), String> {
    window.start_dragging().map_err(|e| e.to_string())
}

#[tauri::command]
fn log_from_frontend(level: String, message: String, state: tauri::State<AppState>) {
    if !state.debug_mode {
        return;
    }
    let timestamp = format_utc_timestamp();
    let log_line = format!("[{}] [{}] {}", timestamp, level.to_uppercase(), message);
    eprintln!("{}", log_line);
    if let Ok(mut guard) = state.debug_log_file.lock() {
        if let Some(ref mut file) = *guard {
            let _ = writeln!(file, "{}", log_line);
            let _ = file.flush();
        }
    }
}

#[tauri::command]
fn toggle_menu_bar(app: AppHandle) {
    if cfg!(target_os = "macos") {
        return;
    }
    handle_menu_bar_toggle(&app);
    let state = app.state::<AppState>();
    let checked = state.config.read().unwrap().show_menu_bar;
    if let Some(check) = find_check_menu_item(&app, MENU_SHOW_MENU_BAR_ID) {
        let _ = check.set_checked(checked);
    }
}

// ============================================================================
// Window settings
// ============================================================================

fn find_check_menu_item(app: &AppHandle, id: &str) -> Option<CheckMenuItem<tauri::Wry>> {
    let menu = app.menu()?;
    for item in menu.items().ok()? {
        if let Some(found) = find_check_in_item(&item, id) {
            return Some(found);
        }
    }
    None
}

/// Recursively search a menu item (and any nested submenus) for a check item with `id`.
/// Needed because the opacity check items live two levels deep (Window > Opacity > item).
fn find_check_in_item(
    item: &MenuItemKind<tauri::Wry>,
    id: &str,
) -> Option<CheckMenuItem<tauri::Wry>> {
    if let Some(check) = item.as_check_menuitem() {
        if check.id().as_ref() == id {
            return Some(check.clone());
        }
    }
    if let Some(submenu) = item.as_submenu() {
        for sub_item in submenu.items().ok()? {
            if let Some(found) = find_check_in_item(&sub_item, id) {
                return Some(found);
            }
        }
    }
    None
}

fn apply_settings_to_window(app: &AppHandle, window: &tauri::WebviewWindow) {
    let state = app.state::<AppState>();
    let config = state.config.read().unwrap();

    // Cross-platform overlay settings (restored on startup and on each spawned window).
    let _ = window.set_always_on_top(config.always_on_top);
    set_window_opacity(window, config.window_opacity);
    set_window_sharing(window, config.stealth_mode);

    // Menu-bar / decoration toggles are Windows/Linux only (macOS uses the native title bar
    // and has no in-window menu bar).
    if cfg!(target_os = "macos") {
        return;
    }
    if !config.show_menu_bar {
        let _ = window.hide_menu();
    }
    #[cfg(target_os = "linux")]
    if config.hide_window_decorations {
        let _ = window.set_decorations(false);
    }
}

/// Apply window opacity (percent, clamped to 50..=100). macOS sets the NSWindow alphaValue so
/// the whole window — chrome + webview content — becomes translucent and the windows below show
/// through. Non-macOS is a no-op for now (macOS-first; no cross-platform Tauri opacity API).
fn set_window_opacity(window: &tauri::WebviewWindow, pct: u8) {
    let pct = pct.clamp(50, 100);
    #[cfg(target_os = "macos")]
    {
        use objc::{msg_send, sel, sel_impl};
        if let Ok(ns_window) = window.ns_window() {
            let alpha = pct as f64 / 100.0;
            let opaque = pct >= 100;
            unsafe {
                let ns_window = ns_window as *mut objc::runtime::Object;
                let _: () = msg_send![ns_window, setOpaque: opaque];
                let _: () = msg_send![ns_window, setAlphaValue: alpha];
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, pct);
    }
}

/// Apply Stealth Mode: when on, set NSWindow sharingType to None so the window's backing store
/// cannot be read by another process (screen recorders / sharers see a solid black rectangle
/// where the chat would be). When off, restore the default shared (ReadOnly) backing so the
/// window contents are visible again. macOS-only; no-op on other platforms. Same `ns_window()` +
/// `msg_send!` shape as `set_window_opacity` (the `macos-private-api` feature unlocks `ns_window()`).
fn set_window_sharing(window: &tauri::WebviewWindow, stealth: bool) {
    #[cfg(target_os = "macos")]
    {
        use objc::{msg_send, sel, sel_impl};
        if let Ok(ns_window) = window.ns_window() {
            // NSWindowSharingType: 0 = None (unreadable by other processes),
            // 1 = ReadOnly (default — readable by other processes), 2 = ReadWrite.
            let sharing: u64 = if stealth { 0 } else { 1 };
            unsafe {
                let ns_window = ns_window as *mut objc::runtime::Object;
                let _: () = msg_send![ns_window, setSharingType: sharing];
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, stealth);
    }
}

fn handle_menu_bar_toggle(app: &AppHandle) {
    if cfg!(target_os = "macos") {
        return;
    }
    let state = app.state::<AppState>();
    let show = {
        let mut config = state.config.write().unwrap();
        config.show_menu_bar = !config.show_menu_bar;
        let _ = save_config(&config);
        config.show_menu_bar
    };
    for (_, window) in app.webview_windows() {
        if show {
            let _ = window.show_menu();
        } else {
            let _ = window.hide_menu();
        }
    }
}

fn handle_always_on_top_toggle(app: &AppHandle) {
    let state = app.state::<AppState>();
    let on = {
        let mut config = state.config.write().unwrap();
        config.always_on_top = !config.always_on_top;
        let _ = save_config(&config);
        config.always_on_top
    };
    for (_, window) in app.webview_windows() {
        let _ = window.set_always_on_top(on);
    }
    if let Some(check) = find_check_menu_item(app, MENU_ALWAYS_ON_TOP_ID) {
        let _ = check.set_checked(on);
    }
}

#[cfg(target_os = "macos")]
fn handle_stealth_toggle(app: &AppHandle) {
    let state = app.state::<AppState>();
    let on = {
        let mut config = state.config.write().unwrap();
        config.stealth_mode = !config.stealth_mode;
        let _ = save_config(&config);
        config.stealth_mode
    };
    for (_, window) in app.webview_windows() {
        set_window_sharing(&window, on);
    }
    if let Some(check) = find_check_menu_item(app, MENU_STEALTH_MODE_ID) {
        let _ = check.set_checked(on);
    }
}

fn handle_set_opacity(app: &AppHandle, pct: u8) {
    let state = app.state::<AppState>();
    {
        let mut config = state.config.write().unwrap();
        config.window_opacity = pct;
        let _ = save_config(&config);
    }
    for (_, window) in app.webview_windows() {
        set_window_opacity(&window, pct);
    }
    // Sync the radio-style check marks under Window > Opacity.
    for preset in OPACITY_PRESETS {
        if let Some(check) = find_check_menu_item(app, &format!("opacity_{}", preset)) {
            let _ = check.set_checked(*preset == pct);
        }
    }
}

#[cfg(target_os = "linux")]
fn handle_decorations_toggle(app: &AppHandle) {
    let state = app.state::<AppState>();
    let hide = {
        let mut config = state.config.write().unwrap();
        config.hide_window_decorations = !config.hide_window_decorations;
        let _ = save_config(&config);
        config.hide_window_decorations
    };
    for (_, window) in app.webview_windows() {
        let _ = window.set_decorations(!hide);
    }
}

// ============================================================================
// Menu setup
// ============================================================================

fn setup_app_menu(app: &AppHandle) -> tauri::Result<()> {
    let menu = app.menu().unwrap_or(Menu::default(app)?);

    let new_chat_item = MenuItem::with_id(app, "new_chat", "New Chat", true, Some("CmdOrCtrl+N"))?;
    let new_window_item = MenuItem::with_id(
        app,
        "new_window",
        "New Window",
        true,
        Some("CmdOrCtrl+Shift+N"),
    )?;
    // Manual re-auth trigger for the hosted-chat SSO (e.g. after logout / cookie expiry). The chat login
    // page also auto-triggers SSO once per launch; this lets the user re-run it on demand.
    let sign_in_item = MenuItem::with_id(app, "sign_in", "Sign in with InferenceHub", true, None::<&str>)?;
    // Docs now link to InferenceHub documentation.
    let docs_item = MenuItem::with_id(
        app,
        "open_docs",
        "InferenceHub Documentation",
        true,
        None::<&str>,
    )?;

    if let Some(file_menu) = menu
        .items()?
        .into_iter()
        .filter_map(|item| item.as_submenu().cloned())
        .find(|submenu| submenu.text().ok().as_deref() == Some("File"))
    {
        file_menu.insert_items(&[&new_chat_item, &new_window_item, &sign_in_item], 0)?;
    } else {
        let file_menu = SubmenuBuilder::new(app, "File")
            .items(&[
                &new_chat_item,
                &new_window_item,
                &sign_in_item,
                &PredefinedMenuItem::close_window(app, None)?,
            ])
            .build()?;
        menu.prepend(&file_menu)?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let config = app.state::<AppState>();
        let config_guard = config.config.read().unwrap();

        let show_menu_bar_item = CheckMenuItem::with_id(
            app,
            MENU_SHOW_MENU_BAR_ID,
            "Show Menu Bar",
            true,
            config_guard.show_menu_bar,
            None::<&str>,
        )?;

        #[cfg(target_os = "linux")]
        let hide_decorations_item = CheckMenuItem::with_id(
            app,
            MENU_HIDE_DECORATIONS_ID,
            "Hide Window Decorations",
            true,
            config_guard.hide_window_decorations,
            None::<&str>,
        )?;

        drop(config_guard);

        if let Some(window_menu) = menu
            .items()?
            .into_iter()
            .filter_map(|item| item.as_submenu().cloned())
            .find(|submenu| submenu.text().ok().as_deref() == Some("Window"))
        {
            window_menu.append(&show_menu_bar_item)?;
            #[cfg(target_os = "linux")]
            window_menu.append(&hide_decorations_item)?;
        } else {
            #[allow(unused_mut)]
            let mut window_menu_builder =
                SubmenuBuilder::new(app, "Window").item(&show_menu_bar_item);
            #[cfg(target_os = "linux")]
            {
                window_menu_builder = window_menu_builder.item(&hide_decorations_item);
            }
            let window_menu = window_menu_builder.build()?;

            let items = menu.items()?;
            let help_idx = items
                .iter()
                .position(|item| {
                    item.as_submenu()
                        .and_then(|s| s.text().ok())
                        .as_deref()
                        == Some("Help")
                })
                .unwrap_or(items.len());
            menu.insert(&window_menu, help_idx)?;
        }
    }

    // Overlay controls: "Always on Top" (all platforms) + an "Opacity" preset submenu
    // (macOS only — the effect is a no-op elsewhere). Appended to the existing Window submenu,
    // which exists on macOS via Menu::default and is created above on Windows/Linux.
    {
        let (aot, opacity, stealth) = {
            let state = app.state::<AppState>();
            let g = state.config.read().unwrap();
            (g.always_on_top, g.window_opacity, g.stealth_mode)
        };

        let always_on_top_item = CheckMenuItem::with_id(
            app,
            MENU_ALWAYS_ON_TOP_ID,
            "Always on Top",
            true,
            aot,
            None::<&str>,
        )?;

        // Stealth Mode + Opacity are macOS-only effects (NSWindow sharingType / alphaValue) —
        // don't build no-op menu items elsewhere.
        #[cfg(target_os = "macos")]
        let stealth_mode_item = CheckMenuItem::with_id(
            app,
            MENU_STEALTH_MODE_ID,
            "Stealth Mode",
            true,
            stealth,
            Some("CmdOrCtrl+."),
        )?;

        if let Some(window_menu) = menu
            .items()?
            .into_iter()
            .filter_map(|item| item.as_submenu().cloned())
            .find(|submenu| submenu.text().ok().as_deref() == Some("Window"))
        {
            window_menu.append(&always_on_top_item)?;

            #[cfg(target_os = "macos")]
            {
                window_menu.append(&stealth_mode_item)?;

                let mut opacity_builder = SubmenuBuilder::new(app, "Opacity");
                for preset in OPACITY_PRESETS {
                    let item = CheckMenuItem::with_id(
                        app,
                        format!("opacity_{}", preset),
                        format!("{}%", preset),
                        true,
                        *preset == opacity,
                        None::<&str>,
                    )?;
                    opacity_builder = opacity_builder.item(&item);
                }
                let opacity_menu = opacity_builder.build()?;
                window_menu.append(&opacity_menu)?;
            }
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (opacity, stealth);
    }

    if let Some(help_menu) = menu
        .get(HELP_SUBMENU_ID)
        .and_then(|item| item.as_submenu().cloned())
    {
        help_menu.append(&docs_item)?;
    } else {
        let help_menu = SubmenuBuilder::with_id(app, HELP_SUBMENU_ID, "Help")
            .item(&docs_item)
            .build()?;
        menu.append(&help_menu)?;
    }

    let state = app.state::<AppState>();
    if state.debug_mode {
        let toggle_devtools_item = MenuItem::with_id(
            app,
            MENU_TOGGLE_DEVTOOLS_ID,
            "Toggle DevTools",
            true,
            Some("F12"),
        )?;
        let open_log_item = MenuItem::with_id(
            app,
            MENU_OPEN_DEBUG_LOG_ID,
            "Open Debug Log",
            true,
            None::<&str>,
        )?;
        let debug_menu = SubmenuBuilder::new(app, "Debug")
            .item(&toggle_devtools_item)
            .item(&open_log_item)
            .build()?;
        menu.append(&debug_menu)?;
    }

    app.set_menu(menu)?;
    Ok(())
}

fn build_tray_menu(app: &AppHandle, status: Option<&PlanStatus>) -> tauri::Result<Menu<Wry>> {
    let open_app = MenuItem::with_id(
        app,
        TRAY_MENU_OPEN_APP_ID,
        "Open InferenceHub",
        true,
        None::<&str>,
    )?;
    let open_chat = MenuItem::with_id(
        app,
        TRAY_MENU_OPEN_CHAT_ID,
        "Open Chat Window",
        true,
        None::<&str>,
    )?;
    // "Show Budget in Menu Bar" toggles the tray *title*, which only exists on macOS —
    // don't offer a no-op toggle elsewhere.
    #[cfg(target_os = "macos")]
    let show_in_menu_bar = {
        // Statement (not a tail-expression) so the guard temporary drops before `state` (the
        // spawn_desktop_sso pattern).
        let state = app.state::<AppState>();
        let show_budget = state.config.read().unwrap().show_budget_in_tray;
        CheckMenuItem::with_id(
            app,
            TRAY_MENU_SHOW_IN_BAR_ID,
            "Show Budget in Menu Bar",
            true,
            show_budget,
            None::<&str>,
        )?
    };
    let quit = PredefinedMenuItem::quit(app, Some("Quit InferenceHub"))?;

    let mut builder = MenuBuilder::new(app).item(&open_app).item(&open_chat).separator();
    // Plan snapshot (disabled info lines mirroring the portal Plans bars) when the poller has data.
    if let Some(plan) = status {
        let header = MenuItem::with_id(app, "tray_plan_header", &plan.label, false, None::<&str>)?;
        builder = builder.item(&header);
        for (i, w) in plan.windows.iter().enumerate() {
            let line = MenuItem::with_id(
                app,
                format!("tray_plan_w{}", i),
                format_window_menu_line(w),
                false,
                None::<&str>,
            )?;
            builder = builder.item(&line);
        }
        if plan.coverage.paused {
            let paused = MenuItem::with_id(
                app,
                "tray_plan_paused",
                "Coverage paused — using balance until reset",
                false,
                None::<&str>,
            )?;
            builder = builder.item(&paused);
        }
        builder = builder.separator();
    }
    #[cfg(target_os = "macos")]
    {
        builder = builder.item(&show_in_menu_bar).separator();
    }
    builder.item(&quit).build()
}

fn handle_tray_menu_event(app: &AppHandle, id: &str) {
    match id {
        TRAY_MENU_OPEN_APP_ID => focus_main_window(app),
        TRAY_MENU_OPEN_CHAT_ID => {
            focus_main_window(app);
            trigger_new_chat(app);
        }
        TRAY_MENU_QUIT_ID => app.exit(0),
        #[cfg(target_os = "macos")]
        TRAY_MENU_SHOW_IN_BAR_ID => {
            // Toggle the budget title (privacy switch for screen-share). Apply instantly from the last
            // poll; the flipped checkbox state is re-read inside build_tray_menu.
            let state = app.state::<AppState>();
            let show = {
                let mut cfg = state.config.write().unwrap();
                cfg.show_budget_in_tray = !cfg.show_budget_in_tray;
                let _ = save_config(&cfg);
                cfg.show_budget_in_tray
            };
            let latest = state.latest_plan_status.lock().unwrap().clone();
            apply_plan_status_to_tray(app, latest.as_ref(), show); // menu events arrive on the main thread
        }
        _ => {}
    }
}

fn setup_tray_icon(app: &AppHandle) -> tauri::Result<()> {
    let mut builder = TrayIconBuilder::with_id(TRAY_ID).tooltip("InferenceHub");

    let tray_icon = Image::from_bytes(TRAY_ICON_BYTES)
        .ok()
        .or_else(|| app.default_window_icon().cloned());

    if let Some(icon) = tray_icon {
        builder = builder.icon(icon);
        #[cfg(target_os = "macos")]
        {
            builder = builder.icon_as_template(true);
        }
    }

    if let Ok(menu) = build_tray_menu(app, None) {
        builder = builder.menu(&menu);
    }

    // macOS: no on_tray_icon_event — with a menu attached, macOS opens the menu on left-click.
    // Also focusing the main window on that same click stole focus and dismissed the menu
    // (open/close flicker). The menu's "Open InferenceHub" item covers window-focus, so
    // left-click just drops the menu down.
    //
    // Windows convention is the opposite: left-click focuses the app, right-click opens the
    // menu — so disable menu-on-left-click (defaults to true) and focus on left-click-up.
    // Linux appindicators don't deliver click events, so the handler is inert there.
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder
            .show_menu_on_left_click(false)
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    focus_main_window(tray.app_handle());
                }
            });
    }
    builder
        .on_menu_event(|app, event| handle_tray_menu_event(app, event.id().as_ref()))
        .build(app)?;

    Ok(())
}

fn handle_toggle_devtools(app: &AppHandle) {
    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
        let windows: Vec<_> = app.webview_windows().into_values().collect();
        let any_open = windows.iter().any(|w| w.is_devtools_open());
        for window in &windows {
            if any_open {
                window.close_devtools();
            } else {
                window.open_devtools();
            }
        }
    }
    #[cfg(not(any(debug_assertions, feature = "devtools")))]
    {
        let _ = app;
    }
}

fn handle_open_debug_log() {
    let log_path = match get_debug_log_path() {
        Some(p) => p,
        None => return,
    };
    if !log_path.exists() {
        eprintln!("[IH DEBUG] Log file does not exist yet: {:?}", log_path);
        return;
    }
    let url_path = log_path.to_string_lossy().replace('\\', "/");
    let _ = open_in_default_browser(&format!("file:///{}", url_path.trim_start_matches('/')));
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    let mut config = load_config();
    if config.install_id.is_empty() {
        config.install_id = uuid::Uuid::new_v4().to_string();
        let _ = save_config(&config); // best-effort; a fresh id next launch is harmless
    }
    let install_id = config.install_id.clone();
    let debug_mode = is_debug_mode();

    let debug_log_file = if debug_mode {
        eprintln!("[IH DEBUG] Debug mode enabled");
        if let Some(path) = get_debug_log_path() {
            eprintln!("[IH DEBUG] Frontend logs: {}", path.display());
        }
        eprintln!("[IH DEBUG] DevTools will open automatically");
        eprintln!("[IH DEBUG] Capturing console.log/warn/error/info/debug from webview");
        init_debug_log_file()
    } else {
        None
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(
            tauri::plugin::Builder::<Wry>::new("chat-external-navigation-handler")
                .on_navigation(|webview, destination_url| {
                    // The IH login overlay's button navigates here; intercept it to start the loopback
                    // SSO and cancel the navigation (the sentinel host has no real page). This is the
                    // explicit login trigger — no IPC, since Tauri firewalls __TAURI__ from remote origins
                    // and the app declares no capabilities. `destination_url` is safe to read here (unlike
                    // `webview.url()`, which panics inside wry during the initial uncommitted load).
                    if destination_url.host_str() == Some("ih-sso.localhost") {
                        spawn_desktop_sso(webview.app_handle().clone());
                        return false;
                    }
                    // Native STT bridge: the chat page's live-transcribe button starts/stops the
                    // helper via the same cancelled-navigation channel (see ih-stt-helper).
                    if destination_url.host_str() == Some("ih-stt.localhost") {
                        let app = webview.app_handle().clone();
                        if destination_url.path() == "/start" {
                            // ?source=mic|system picks the capture source (system =
                            // ScreenCaptureKit, hears meetings on headphones).
                            let source = destination_url
                                .query_pairs()
                                .find(|(k, _)| k == "source")
                                .map(|(_, v)| v.into_owned())
                                .filter(|s| s == "system")
                                .unwrap_or_else(|| "mic".to_string());
                            start_stt_helper(app, source);
                        } else {
                            stop_stt_helper(&app);
                        }
                        return false;
                    }
                    // Allow all other in-webview navigation.
                    true
                })
                .build(),
        )
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(AppState {
            config: RwLock::new(config),
            server_base_url: RwLock::new(None),
            debug_mode,
            debug_log_file: Mutex::new(debug_log_file),
            install_id,
            sso_in_progress: AtomicBool::new(false),
            stt_child: Mutex::new(None),
            latest_plan_status: Mutex::new(None),
            plan_poll_now: AtomicBool::new(false),
        })
        .manage(SidecarState::new())
        .invoke_handler(tauri::generate_handler![
            open_in_browser,
            reload_page,
            go_back,
            go_forward,
            new_window,
            start_drag_window,
            toggle_menu_bar,
            log_from_frontend,
        ])
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open_docs" => {
                let _ = open_in_default_browser("https://inferencehub.tech/docs");
            }
            "new_chat" => trigger_new_chat(app),
            "new_window" => trigger_new_window(app),
            "sign_in" => spawn_desktop_sso(app.clone()),
            "show_menu_bar" => handle_menu_bar_toggle(app),
            MENU_ALWAYS_ON_TOP_ID => handle_always_on_top_toggle(app),
            #[cfg(target_os = "macos")]
            MENU_STEALTH_MODE_ID => handle_stealth_toggle(app),
            id if id.starts_with("opacity_") => {
                if let Ok(pct) = id["opacity_".len()..].parse::<u8>() {
                    handle_set_opacity(app, pct);
                }
            }
            #[cfg(target_os = "linux")]
            "hide_window_decorations" => handle_decorations_toggle(app),
            MENU_TOGGLE_DEVTOOLS_ID => handle_toggle_devtools(app),
            MENU_OPEN_DEBUG_LOG_ID => handle_open_debug_log(),
            _ => {}
        })
        .setup(move |app| {
            let app_handle = app.handle().clone();

            if let Err(e) = setup_app_menu(&app_handle) {
                eprintln!("Failed to setup menu: {}", e);
            }

            if let Err(e) = setup_tray_icon(&app_handle) {
                eprintln!("Failed to setup tray icon: {}", e);
            }

            // Standard decorated window (native, draggable title bar). No vibrancy/
            // custom title bar injection — those need a transparent/overlay window and
            // an in-page drag region, which the hosted Onyx UI doesn't provide.
            if let Some(window) = app.get_webview_window("main") {
                apply_settings_to_window(&app_handle, &window);
                maybe_open_devtools(&app_handle, &window);
                let _ = window.set_focus();
            }

            // ----------------------------------------------------------------
            // Hosted mode: the Onyx backend runs in the cloud (see deploy/onyx/).
            // Navigate the main window straight to the InferenceHub-hosted Onyx
            // instance. Override the URL with the IH_SERVER_URL env var (e.g. to
            // point at a local Onyx for development).
            // ----------------------------------------------------------------
            let server_url = std::env::var("IH_SERVER_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
            eprintln!("[IH] Hosted server: {}", server_url);
            {
                let state = app_handle.state::<AppState>();
                *state.server_base_url.write().unwrap() = Some(server_url.clone());
            }
            let nav_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(window) = nav_handle.get_webview_window("main") {
                    match server_url.parse::<tauri::Url>() {
                        Ok(u) => {
                            if let Err(e) = window.navigate(u) {
                                eprintln!("[IH] Navigation failed: {}", e);
                            }
                        }
                        Err(e) => eprintln!("[IH] Bad IH_SERVER_URL: {}", e),
                    }
                }
            });

            // Onyx gates auth client-side, so on_page_load can't catch the /auth/login redirect on a
            // logged-out launch — poll the webview URL from Rust and auto-trigger SSO when it appears.
            spawn_login_redirect_watcher(app_handle.clone());

            // Menubar budget: poll the gateway plan-status endpoint with the persisted read-only status
            // token (idle until the first sign-in stores one) and render the tightest window as the tray
            // title. Fail-silent by design.
            spawn_plan_status_poller(app_handle.clone());

            Ok(())
        })
        .on_page_load(|webview: &Webview, payload: &PageLoadPayload| {
            inject_chat_link_intercept(webview);

            if webview.label() == "main" {
                // Advertise the native STT bridge to the chat page (feature detection —
                // useLiveTranscribe only navigates the ih-stt sentinel when this is set),
                // and kill any helper left over from the page that just navigated away.
                // macOS-only: the helper is a compiled Swift binary, so don't advertise the
                // bridge (and thus the mic button) on other platforms.
                #[cfg(target_os = "macos")]
                let _ = webview.eval("window.__IH_STT_AVAILABLE = true;");
                stop_stt_helper(webview.app_handle());
            }

            // When a full page load lands on Onyx's login route (initial direct nav, or the bridge's
            // ih_sso_error redirect), show the InferenceHub login gate over the native email/password
            // form. Client-side pushState bounces to /auth/login are caught instead by the URL watcher.
            // The overlay is idempotent and the login is started explicitly by the user (overlay button →
            // ih-sso.localhost sentinel → on_navigation → spawn_desktop_sso). payload.url() is safe here.
            if webview.label() == "main" && payload.url().path().starts_with("/auth/login") {
                let _ = webview.eval(IH_LOGIN_OVERLAY_SCRIPT);
            }

            {
                let app = webview.app_handle();
                let state = app.state::<AppState>();
                if state.debug_mode {
                    inject_console_capture(webview);
                }
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = webview.eval(MENU_KEY_HANDLER_SCRIPT);
                let app = webview.app_handle();
                let label = webview.label().to_string();
                if let Some(win) = app.get_webview_window(&label) {
                    apply_settings_to_window(&app, &win);
                }
            }
        })
        .on_window_event(|window, event| {
            // Kill the sidecar when the last window closes.
            if let tauri::WindowEvent::Destroyed = event {
                let app = window.app_handle();
                // Only kill when no windows remain.
                if app.webview_windows().is_empty() {
                    let sidecar = app.state::<SidecarState>();
                    sidecar.kill();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ============================================================================
// Tests (pure helpers only — no Tauri runtime needed)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: &str, used: i64, cap: i64, reset: i64) -> PlanWindow {
        PlanWindow {
            id: id.into(),
            label: id.into(),
            used_micros: used,
            cap_micros: cap,
            resets_in_sec: reset,
        }
    }

    #[test]
    fn tightest_window_picks_smallest_remaining_fraction() {
        let ws = vec![
            w("5h", 8_730_000, 10_000_000, 1_920), // 12.7% remaining -> tightest
            w("wk", 1_270_000, 60_000_000, 400_000),
            w("mo", 1_430_000, 270_000_000, 1_200_000),
        ];
        assert_eq!(tightest_window(&ws).unwrap().id, "5h");
    }

    #[test]
    fn tightest_window_ignores_capless_and_ties_break_on_sooner_reset() {
        let ws = vec![
            w("zero", 0, 0, 1), // cap 0: skipped
            w("a", 5_000_000, 10_000_000, 9_000),
            w("b", 5_000_000, 10_000_000, 3_000), // same fraction, sooner reset -> wins
        ];
        assert_eq!(tightest_window(&ws).unwrap().id, "b");
        assert!(tightest_window(&[w("z", 1, 0, 1)]).is_none());
    }

    #[test]
    fn title_skips_exhausted_buckets_and_flags_them() {
        // The real repro: Claude monthly exhausted ($10.03/$10, 28d reset) must NOT become the title —
        // runway is the open-weight lanes; the "!" prefix carries the exhaustion signal.
        let ws = vec![
            w("open_weight_5h", 0, 10_000_000, 5_460),
            w("open_weight_weekly", 1_270_000, 60_000_000, 156_660),
            w("claude_monthly", 10_030_000, 10_000_000, 2_489_460), // exhausted
            w("gpt_monthly", 0, 10_000_000, 2_489_460),
            w("monthly_ceiling", 11_300_000, 270_000_000, 2_489_460),
        ];
        assert_eq!(title_window(&ws).unwrap().id, "open_weight_weekly"); // tightest NON-exhausted
        assert_eq!(format_tray_title(&ws).unwrap(), "! $58.73 · 43h");
        // no exhaustion -> no prefix
        let healthy = vec![w("5h", 1_270_000, 10_000_000, 1_920)];
        assert_eq!(format_tray_title(&healthy).unwrap(), "$8.73 · 32m");
    }

    #[test]
    fn ceiling_only_takes_title_when_low_and_binding() {
        // Backstop stays out of the title while healthy, even when fraction-tighter than the lanes…
        let healthy = vec![
            w("open_weight_5h", 0, 10_000_000, 5_460),
            w("monthly_ceiling", 100_000_000, 270_000_000, 2_489_460), // 63% left, tighter than 100%
        ];
        assert_eq!(title_window(&healthy).unwrap().id, "open_weight_5h");
        // …but takes it when genuinely low (<25%) and tighter than every lane.
        let low = vec![
            w("open_weight_5h", 0, 10_000_000, 5_460),
            w("monthly_ceiling", 250_000_000, 270_000_000, 2_489_460), // 7.4% left
        ];
        assert_eq!(title_window(&low).unwrap().id, "monthly_ceiling");
        // Ceiling alone (all lanes exhausted) is still shown over the obituary fallback.
        let only = vec![
            w("open_weight_5h", 10_000_000, 10_000_000, 5_460),
            w("monthly_ceiling", 100_000_000, 270_000_000, 2_489_460),
        ];
        assert_eq!(title_window(&only).unwrap().id, "monthly_ceiling");
    }

    #[test]
    fn title_all_exhausted_shows_zero_with_soonest_reset() {
        let ws = vec![
            w("a", 10_000_000, 10_000_000, 9_000),
            w("b", 12_000_000, 10_000_000, 3_000), // sooner reset -> shown
        ];
        assert_eq!(title_window(&ws).unwrap().id, "b");
        assert_eq!(format_tray_title(&ws).unwrap(), "$0.00 · 50m"); // no "!" — the $0 IS the message
        assert!(format_tray_title(&[]).is_none());
    }

    #[test]
    fn menu_line_says_used_up_for_exhausted_buckets() {
        assert_eq!(
            format_window_menu_line(&w("Claude budget (monthly)", 10_030_000, 10_000_000, 3 * 86_400)),
            "Claude budget (monthly): used up — resets in 3d · billing balance now"
        );
        assert_eq!(
            format_window_menu_line(&w("Open-weight — 5-hour", 1_270_000, 10_000_000, 1_920)),
            "Open-weight — 5-hour: $8.73 of $10.00 · 32m"
        );
    }

    #[test]
    fn format_reset_bands() {
        assert_eq!(format_reset(59), "1m"); // ceil to the next minute
        assert_eq!(format_reset(2 * 3600 - 1), "120m");
        assert_eq!(format_reset(2 * 3600), "2h");
        assert_eq!(format_reset(48 * 3600), "2d");
        assert_eq!(format_reset(-5), "0m"); // negative clamps
    }

    #[test]
    fn parse_tokens_reads_both_and_requires_the_chat_token() {
        let req = "GET /cb?token=abc&status_token=ihs_xyz HTTP/1.1\r\nHost: x\r\n\r\n";
        let t = parse_tokens_from_request(req).unwrap();
        assert_eq!(t.chat, "abc");
        assert_eq!(t.status.as_deref(), Some("ihs_xyz"));
        // status token alone (no chat token) is NOT a completed SSO — keep waiting
        assert!(parse_tokens_from_request("GET /cb?status_token=ihs_x HTTP/1.1\r\n\r\n").is_none());
        // old gateway: chat token only
        let old = parse_tokens_from_request("GET /cb?token=abc HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(old.chat, "abc");
        assert!(old.status.is_none());
        // favicon / tokenless requests keep waiting
        assert!(parse_tokens_from_request("GET /favicon.ico HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn plan_status_response_deserializes_gateway_shape() {
        let json = r#"{"enabled":true,"plan":{"tier":"indie","label":"All Access",
            "windows":[{"id":"open_weight_5h","label":"Open-weight — 5-hour","used_micros":1270000,
                        "cap_micros":10000000,"resets_in_sec":10380}],
            "coverage":{"paused":false,"pausedScope":null,"premiumExcluded":false,"premiumPaused":false},
            "funding":"crypto","cancel_at_period_end":false,"current_period_end":"2026-08-05T00:00:00.000Z"}}"#;
        let r: PlanStatusResponse = serde_json::from_str(json).unwrap();
        assert!(r.enabled);
        let plan = r.plan.unwrap();
        assert_eq!(plan.label, "All Access");
        assert_eq!(plan.windows.len(), 1);
        assert_eq!(plan.windows[0].cap_micros, 10_000_000);
        // no-plan shape
        let none: PlanStatusResponse = serde_json::from_str(r#"{"enabled":false,"plan":null}"#).unwrap();
        assert!(none.plan.is_none());
    }
}
