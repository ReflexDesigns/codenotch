#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod autostart;
mod config;
mod doctor;
mod focus;
mod hooks_install;
mod i18n;
mod server;
mod state;
mod tray;
mod usage;
mod codex;
mod glyphs;
mod activity;
mod diag;
mod watcher;
mod update;

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Logical size of the notch window: the 70 pt pill column on the right plus room for the hover card on the left.
pub const NOTCH_W: f64 = 340.0;
/// The version the exe was built from, written to run.log at startup and shown in the hover card, so a
/// running build can always be named. Taken from Cargo.toml rather than bumped by hand: a hand-bumped tag
/// goes stale exactly when it matters, and an installer that silently keeps the old binary looks identical
/// to one that worked.
pub const BUILD: &str = env!("CARGO_PKG_VERSION");
pub const NOTCH_H: f64 = 460.0; // 300 clipped the card once it held three window blocks plus the session list

pub struct AppState {
    pub store: Mutex<state::Store>,
    pub cfg: Mutex<config::Config>,
    pub usage: Mutex<usage::UsageSnapshot>,
    /// Codex snapshot (same UsageSnapshot shape; status may also be none/absent)
    pub codex: Mutex<usage::UsageSnapshot>,
    /// Provider glyph cache, collected at launch and again on a tray refresh
    pub glyphs: Mutex<std::collections::HashMap<String, glyphs::Glyph>>,
    /// Working state of Codex, inferred from recent writes to its rollout logs
    pub activity: Mutex<Vec<activity::Activity>>,
}

fn resolved_lang(raw: &str) -> String {
    if raw == "auto" {
        i18n::resolve_auto().to_string()
    } else {
        raw.to_string()
    }
}

pub fn broadcast(app: &AppHandle) {
    let st = app.state::<AppState>();
    let snap = {
        let store = st.store.lock().unwrap();
        let cfg = st.cfg.lock().unwrap();
        store.snapshot(&cfg.lang, &resolved_lang(&cfg.lang), false)
    };
    let _ = app.emit("state", &snap);
}

/// Pins the notch to the right edge of the primary monitor; the other edges are a later milestone.
pub fn place_notch(app: &AppHandle) {
    let Some(w) = app.get_webview_window("notch") else {
        return;
    };
    let scale = w.scale_factor().unwrap_or(1.0);
    if let Ok(Some(mon)) = w.primary_monitor() {
        // Two monitors at different scales (150 % and 200 % in practice): the physical size can
        // end up converted with the *other* monitor's scale factor depending on where the window
        // is created and then moved, leaving the WebView ~256 logical px wide instead of 340.
        // So the physical size is pinned straight from mon.scale_factor() before placing the
        // window; if it still reports a different scale afterwards, it is pinned once more.
        let ms = mon.scale_factor();
        let target = tauri::PhysicalSize::new((NOTCH_W * ms).round() as u32, (NOTCH_H * ms).round() as u32);
        let _ = w.set_size(target);
        // Position from the window's measured physical size — deriving it from the scale factor
        // pushed the window past the right edge at 125 % / 150 % (the ring's right side was clipped).
        let (ww, wh) = w
            .outer_size()
            .map(|s| (s.width as i32, s.height as i32))
            .unwrap_or(((NOTCH_W * scale) as i32, (NOTCH_H * scale) as i32));
        let x = mon.position().x + mon.size().width as i32 - ww;
        // Vertical position comes from the configured ratio (the pill can be dragged; it persists), clamped to the monitor
        let ratio = {
            let st = app.state::<AppState>();
            let c = st.cfg.lock().unwrap();
            c.notch_y.clamp(0.0, 1.0)
        };
        let mh = mon.size().height as i32;
        let y = (mon.position().y as f64 + mh as f64 * ratio - wh as f64 / 2.0).round() as i32;
        let y = y.clamp(mon.position().y, mon.position().y + (mh - wh).max(0));
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
        if w.outer_size().map(|s| s.width != target.width).unwrap_or(false) {
            let _ = w.set_size(target);
            let x = mon.position().x + mon.size().width as i32 - target.width as i32;
            let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
        }
        // Placement log line: the first thing to check when the notch is not visible
        let log = config::config_path().with_file_name("run.log");
        let _ = std::fs::write(
            log,
            format!(
                "notch placed build={BUILD}: pos=({x},{y}) size=({ww}x{wh}) inner={:?} win_scale={scale} mon_scale={ms} monitor=({},{} {}x{})\n",
                w.inner_size().map(|s| (s.width, s.height)).unwrap_or((0, 0)),
                mon.position().x,
                mon.position().y,
                mon.size().width,
                mon.size().height
            ),
        );
    }
}

/// Older entry point name still used by tray.rs
pub fn reset_bar(app: &AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.notch_y = 0.5;
        config::save(&c);
    }
    place_notch(app);
}

/// Drag along the right edge. The page calls this once after a press on the pill moves more than
/// 4 px; from then on a Rust thread follows the system cursor (WebView mousemove is unreliable
/// once the window itself starts moving). Releasing the left button ends the drag and the centre
/// ratio is written back to the config.
static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
fn left_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}
#[cfg(not(windows))]
fn left_button_down() -> bool {
    false
}

#[tauri::command]
fn drag_begin(app: AppHandle) {
    if DRAGGING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let Some(w) = app.get_webview_window("notch") else {
            DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let (Ok(start_cur), Ok(start_pos), Ok(size), Ok(Some(mon))) =
            (app.cursor_position(), w.outer_position(), w.outer_size(), w.primary_monitor())
        else {
            DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let (my, mh) = (mon.position().y, mon.size().height as i32);
        let wh = size.height as i32;
        let lo = my;
        let hi = my + (mh - wh).max(0);
        let mut last_y = start_pos.y;
        let mut moved = false;
        loop {
            if !left_button_down() {
                break;
            }
            if let Ok(cur) = app.cursor_position() {
                let ny = (start_pos.y as f64 + (cur.y - start_cur.y)).round() as i32;
                let ny = ny.clamp(lo, hi);
                if ny != last_y {
                    last_y = ny;
                    moved = true;
                    let _ = w.set_position(tauri::PhysicalPosition::new(start_pos.x, ny));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        if moved {
            let ratio = ((last_y + wh / 2 - my) as f64 / mh as f64).clamp(0.0, 1.0);
            let st = app.state::<AppState>();
            let mut c = st.cfg.lock().unwrap();
            c.notch_y = ratio;
            config::save(&c);
            applog(&format!("notch drag: y={last_y} ratio={ratio:.3}"));
        }
        DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = app.emit("drag_end", moved);
    });
}
pub fn place_bar(app: &AppHandle) {
    place_notch(app);
}
pub fn toggle_drag(app: &AppHandle) {
    // The notch stays welded to the edge; kept as a no-op for the tray menu code path
    let _ = app;
}

pub fn apply_lang(app: &AppHandle, lang: &str) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.lang = lang.to_string();
        config::save(&c);
    }
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = tray::build_menu(app, lang) {
            let _ = tray.set_menu(Some(menu));
        }
    }
    broadcast(app);
}

/// The notch must never take focus: WS_EX_NOACTIVATE + WS_EX_TOOLWINDOW
#[cfg(windows)]
fn noactivate(app: &AppHandle) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };
    if let Some(w) = app.get_webview_window("notch") {
        if let Ok(h) = w.hwnd() {
            unsafe {
                let hwnd =
                    windows::Win32::Foundation::HWND(h.0 as isize as *mut core::ffi::c_void);
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
                );
            }
        }
    }
}
#[cfg(not(windows))]
fn noactivate(_app: &AppHandle) {}

// ---------------- commands ----------------

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> state::Snapshot {
    let store = state.store.lock().unwrap();
    let cfg = state.cfg.lock().unwrap();
    store.snapshot(&cfg.lang, &resolved_lang(&cfg.lang), false)
}

#[tauri::command]
fn get_usage(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.usage.lock().unwrap().clone()
}

#[tauri::command]
fn refresh_usage(app: AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut u = st.usage.lock().unwrap();
        u.backoff_until = 0;
    }
    usage::request_refresh();
    codex::request_refresh();
}

#[tauri::command]
fn get_activity(state: tauri::State<AppState>) -> Vec<activity::Activity> {
    state.activity.lock().unwrap().clone()
}

#[tauri::command]
fn get_glyphs(state: tauri::State<AppState>) -> std::collections::HashMap<String, glyphs::Glyph> {
    state.glyphs.lock().unwrap().clone()
}

/// Collects the glyphs again and pushes them to the page (tray refresh, or the user just dropped in an override)
pub fn reload_glyphs(app: &AppHandle) {
    let m = glyphs::collect();
    let st = app.state::<AppState>();
    *st.glyphs.lock().unwrap() = m.clone();
    let _ = app.emit("glyphs", &m);
}

#[tauri::command]
fn open_data_dir() {
    let dir = config::config_path().parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let _ = std::fs::create_dir_all(glyphs::user_dir());
    let mut cmd = std::process::Command::new("explorer");
    cmd.arg(dir.as_os_str());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

#[tauri::command]
fn get_codex(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.codex.lock().unwrap().clone()
}

/// A click on a cell opens that provider's app, or its usage page when there is no app
#[tauri::command]
fn open_provider_page(provider: String) {
    // Process-name fragment to focus, executables to launch (under %LOCALAPPDATA%), site to fall back on.
    // Codex is a CLI with no window and no app, so it only ever opens its account page. Claude has
    // two shapes on Windows: the packaged build, which can be focused but has no path to launch
    // from, and the plain installer's, which has one.
    let (proc, exes, url): (&str, &[&str], &str) = match provider.as_str() {
        "codex" => ("", &[], "https://chatgpt.com/#settings/Account"),
        _ => (
            "claude",
            &[r"AnthropicClaude\claude.exe"],
            "https://claude.ai/settings/usage",
        ),
    };
    // Focus before launch: clicking a cell for an app that is already open should raise its window,
    // not leave a second instance behind.
    if !proc.is_empty() && focus::focus_app(proc) {
        return;
    }
    if let Some(local) = dirs::data_local_dir() {
        for rel in exes {
            let exe = local.join(rel);
            if exe.exists() {
                shell_open(&exe.to_string_lossy());
                return;
            }
        }
    }
    shell_open(url);
}

/// `start` with an empty title argument, detached and without a console window. Takes a URL or a path.
fn shell_open(target: &str) {
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", target]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
}

/// Card expansion state: Some(hot rectangles, in **physical pixels** relative to the window's
/// top-left as x,y,w,h) = expanded; None = collapsed. The page converts the rectangles with its
/// own devicePixelRatio before reporting them, so no scale conversion happens on this side —
/// WebView2's DPR and the window's scale_factor can disagree (see report_dpr).
static HOT: Mutex<Option<Vec<[f64; 4]>>> = Mutex::new(None);

/// The pill's own rectangle (physical px, window-relative), reported on every render. The window is
/// 340x460 but the pill only occupies a column on its right, and the rest has to let clicks through
/// to whatever is underneath — so the watchdog needs to know where the pill is even when collapsed.
static PILL: Mutex<Option<[f64; 4]>> = Mutex::new(None);

#[tauri::command]
fn report_pill(rect: [f64; 4]) {
    *PILL.lock().unwrap() = Some(rect);
}

#[tauri::command]
fn set_expanded(on: bool, rects: Option<Vec<[f64; 4]>>) {
    *HOT.lock().unwrap() = if on { Some(rects.unwrap_or_default()) } else { None };
}

/// The WebView zoom currently applied (1.0 = uncorrected)
static ZOOM: Mutex<f64> = Mutex::new(1.0);

pub fn applog(line: &str) {
    use std::io::Write;
    let log = config::config_path().with_file_name("run.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

/// Root cause: with two monitors (150 % / 200 %) WebView2 picked a devicePixelRatio of 2.0 while
/// the window was sized for the primary monitor's 1.5, so the page was 255 CSS px wide instead of
/// the designed 340 and every coordinate conversion was off (the watchdog misfired and the card
/// flashed away). Fix: the page reports its DPR, and when it differs from the primary monitor's
/// scale, set_zoom pulls the effective DPR back to that scale, restoring the 340 px width.
#[tauri::command]
fn report_dpr(app: AppHandle, dpr: f64, w: f64, h: f64) {
    let Some(win) = app.get_webview_window("notch") else { return };
    let want = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or_else(|| win.scale_factor().unwrap_or(1.0));
    let mut z = ZOOM.lock().unwrap();
    let base = if *z > 0.0 { dpr / *z } else { dpr };
    let target = if base > 0.0 { want / base } else { 1.0 };
    applog(&format!(
        "dpr report: dpr={dpr:.3} viewport={w:.0}x{h:.0} monitor_scale={want:.3} zoom_applied={:.3} -> target_zoom={target:.3}",
        *z
    ));
    // Oscillation guard: at most three corrections per process (if the DPR does not follow the zoom, stop chasing it)
    static APPLIED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if (dpr - want).abs() > 0.02
        && (target - *z).abs() > 0.01
        && (0.25..=4.0).contains(&target)
        && APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
    {
        match win.set_zoom(target) {
            Ok(()) => {
                *z = target;
                applog(&format!("dpr correction: set_zoom({target:.3}) ok"));
            }
            Err(e) => applog(&format!("dpr correction failed: {e}")),
        }
    }
}

/// One cursor poll, two jobs.
///
/// The first is click-through. The window is 340×460 but the notch only paints a column on its
/// right, and a transparent pixel still swallows the click meant for whatever is behind it — a link
/// two hundred pixels away from anything visible simply stopped working. So the window is
/// ignore_cursor_events by default and input is switched back on only while the cursor is actually
/// over the pill (or, once expanded, over the card as well). WebView2 sees no mouse events while
/// the window is click-through, which is exactly why this has to be driven from a system-cursor
/// poll rather than from DOM hover.
///
/// The second is collapsing. WebView2's mouseleave is unreliable inside a NOACTIVATE transparent
/// window — a cursor that leaves quickly often produces no WM_MOUSELEAVE, and the card stays up —
/// so pointer_left is emitted here once the cursor has missed the hot rectangles (pill, card, and
/// the gap between them) five polls running, and the page collapses after its 250 ms grace period.
fn start_pointer_watchdog(app: AppHandle) {
    std::thread::spawn(move || {
        let mut miss = 0u8;
        // None until the first poll, so the first decision is always applied
        let mut ignoring: Option<bool> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(60));
            let Some(w) = app.get_webview_window("notch") else { continue };
            let (Ok(pos), Ok(cur)) = (w.outer_position(), app.cursor_position()) else { continue };
            // Cursor relative to the window's top-left, in physical pixels; the reported rectangles are
            // physical too, so no scale conversion happens here
            let lx = cur.x - pos.x as f64;
            let ly = cur.y - pos.y as f64;
            const PAD: f64 = 10.0;
            let hit = |r: &[f64; 4]| {
                lx >= r[0] - PAD && ly >= r[1] - PAD && lx < r[0] + r[2] + PAD && ly < r[1] + r[3] + PAD
            };
            let in_window = w
                .outer_size()
                .map(|s| lx >= 0.0 && ly >= 0.0 && lx < s.width as f64 && ly < s.height as f64)
                .unwrap_or(true);

            let expanded = HOT.lock().unwrap().clone();
            let inside = match &expanded {
                Some(rects) => {
                    let mut i = in_window && rects.iter().any(|r| hit(r));
                    // The gap between pill and card counts as inside: use the bounding box of all of them
                    if !i && in_window && rects.len() > 1 {
                        let x0 = rects.iter().map(|r| r[0]).fold(f64::MAX, f64::min);
                        let y0 = rects.iter().map(|r| r[1]).fold(f64::MAX, f64::min);
                        let x1 = rects.iter().map(|r| r[0] + r[2]).fold(f64::MIN, f64::max);
                        let y1 = rects.iter().map(|r| r[1] + r[3]).fold(f64::MIN, f64::max);
                        i = lx >= x0 && ly >= y0 && lx < x1 && ly < y1;
                    }
                    i
                }
                // Collapsed: only the pill is worth taking the cursor for. No rectangle reported yet
                // (the page has not rendered) means stay click-through rather than block the desktop.
                None => in_window && PILL.lock().unwrap().as_ref().map(&hit).unwrap_or(false),
            };

            let want_ignore = !inside;
            if ignoring != Some(want_ignore) {
                let _ = w.set_ignore_cursor_events(want_ignore);
                ignoring = Some(want_ignore);
            }

            static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
                applog(&format!(
                    "watchdog: cursor_rel=({lx:.0},{ly:.0}) inside={inside} ignore={want_ignore} pill={:?} winpos=({},{})",
                    PILL.lock().unwrap(),
                    pos.x,
                    pos.y
                ));
            }

            if expanded.is_some() {
                if inside {
                    miss = 0;
                } else {
                    miss += 1;
                    if miss >= 5 {
                        miss = 0;
                        *HOT.lock().unwrap() = None;
                        let _ = app.emit("pointer_left", ());
                    }
                }
            } else {
                miss = 0;
            }
        }
    });
}

/// The running build, for the hover card — the answer to "is this the version I just installed?"
#[tauri::command]
fn app_version() -> &'static str {
    BUILD
}

/// Log channel for the page: JS writes key diagnostics into run.log (if invoke itself fails, the page reports on screen instead)
#[tauri::command]
fn log_js(msg: String) {
    applog(&format!("js: {}", msg.chars().take(600).collect::<String>()));
}

#[tauri::command]
fn open_usage_page() {
    shell_open("https://claude.ai/settings/usage");
}

#[tauri::command]
fn focus_session(app: AppHandle, id: String) -> bool {
    let ppid = {
        let st = app.state::<AppState>();
        let store = st.store.lock().unwrap();
        store.ppid_of(&id)
    };
    match ppid {
        Some(p) => focus::focus_terminal(p),
        None => focus::focus_claude_desktop(),
    }
}

#[tauri::command]
fn dismiss_session(app: AppHandle, id: String) {
    {
        let st = app.state::<AppState>();
        let mut store = st.store.lock().unwrap();
        store.dismiss(&id);
    }
    broadcast(&app);
}

#[tauri::command]
fn set_lang(app: AppHandle, lang: String) {
    apply_lang(&app, &lang);
}

/// Seen-clears-it: looking at a session acknowledges it (engine behaviour, unchanged)
#[cfg(windows)]
fn ack_scan(app: &AppHandle) -> bool {
    let need = {
        let st = app.state::<AppState>();
        let store = st.store.lock().unwrap();
        store.has_done()
    };
    if !need {
        return false;
    }
    let fg = focus::fg_pid();
    if fg == 0 {
        return false;
    }
    let maps = focus::proc_maps();
    let fg_name = maps.name.get(&fg).cloned().unwrap_or_default();
    let fg_is_claude_desktop = fg_name.contains("claude") && !fg_name.contains("codenotch");
    let st = app.state::<AppState>();
    let mut store = st.store.lock().unwrap();
    store.ack_done(|s| {
        if s.ppid == 0 {
            fg_is_claude_desktop
        } else {
            focus::pid_hits_chain(fg, &focus::chain_of(s.ppid, &maps.ppid), &maps)
        }
    })
}
#[cfg(not(windows))]
fn ack_scan(_app: &AppHandle) -> bool {
    false
}

// ---------------- main ----------------

#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
#[cfg(not(windows))]
fn attach_console() {}

fn report(r: Result<String, String>) {
    let msg = match r {
        Ok(m) => format!("OK: {m}"),
        Err(e) => format!("FAILED: {e}"),
    };
    println!("{msg}");
    let log = config::config_path().with_file_name("install.log");
    let _ = std::fs::write(log, &msg);
}

fn main() {
    attach_console();
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1) {
        match cmd.as_str() {
            "install-hooks" => {
                report(hooks_install::install());
                return;
            }
            "uninstall-hooks" => {
                report(hooks_install::uninstall());
                return;
            }
            "autostart" => {
                let r = match args.get(2).map(|s| s.as_str()) {
                    Some("on") => autostart::enable(),
                    Some("off") => autostart::disable(),
                    _ => Err("usage: codenotch.exe autostart on|off".into()),
                };
                report(r);
                return;
            }
            "doctor" => {
                let out = if args.get(2).map(|s| s.as_str()) == Some("deep") { diag::run() } else { doctor::run() };
                println!("{out}");
                let log = config::config_path().with_file_name("doctor.log");
                let _ = std::fs::write(log, &out);
                return;
            }
            _ => {}
        }
    }

    let cfg = config::load();
    let port = cfg.port;

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Launching a freshly built exe while the old one is still running lands here: the new
            // instance is turned away and what stays on screen is the old process. Say so loudly.
            applog(&format!("single instance: another launch was refused; the running instance is build={BUILD} — quit it from the tray first if you just rebuilt"));
            let _ = app.emit("notice", format!("Codenotch is already running ({BUILD}) — quit it from the tray before starting a new build"));
        }))
        .manage(AppState {
            store: Mutex::new(Default::default()),
            cfg: Mutex::new(cfg),
            usage: Mutex::new(usage::load_persisted()),
            codex: Mutex::new(codex::load_persisted()),
            glyphs: Mutex::new(Default::default()),
            activity: Mutex::new(Vec::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_usage,
            get_codex,
            get_glyphs,
            get_activity,
            open_data_dir,
            drag_begin,
            open_provider_page,
            refresh_usage,
            open_usage_page,
            set_expanded,
            report_pill,
            report_dpr,
            log_js,
            app_version,
            focus_session,
            dismiss_session,
            set_lang
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            place_notch(&handle);
            noactivate(&handle);
            if let Some(w) = handle.get_webview_window("notch") {
                // Click-through from the first frame: the watchdog switches input back on once the
                // cursor reaches the pill, and until then nothing behind the window is blocked.
                let _ = w.set_ignore_cursor_events(true);
                let _ = w.show();
            }
            tray::setup(&handle)?;
            server::start(handle.clone(), port);
            watcher::start(handle.clone());
            usage::start(handle.clone());
            update::start(handle.clone());
            // Codex hidden is reported as "absent", the state the UI already uses for a provider
            // that is not installed, so no cell is drawn and its poller never starts. Claude has no
            // switch: a notch with nothing in it would be a blank pill.
            let codex_hidden = {
                let st = handle.state::<AppState>();
                let cfg = st.cfg.lock().unwrap();
                cfg.hidden_providers.iter().any(|h| h.eq_ignore_ascii_case("codex"))
            };
            if codex_hidden {
                let absent = usage::UsageSnapshot { status: "absent".into(), ..Default::default() };
                *handle.state::<AppState>().codex.lock().unwrap() = absent.clone();
                let _ = handle.emit("codex", &absent);
            } else {
                codex::start(handle.clone());
            }
            activity::start(handle.clone());
            // Collecting glyphs may read icon resources out of a few executables; do it off the main thread and push when done
            let gh = handle.clone();
            std::thread::spawn(move || reload_glyphs(&gh));
            start_pointer_watchdog(handle.clone());
            // Seen-clears-it scan
            let acker = handle.clone();
            std::thread::spawn(move || {
                activity::lower_thread_priority();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if ack_scan(&acker) {
                        broadcast(&acker);
                    }
                }
            });
            // Stale session cleanup
            let sweeper = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let changed = {
                    let st = sweeper.state::<AppState>();
                    let mut s = st.store.lock().unwrap();
                    s.sweep()
                };
                if changed {
                    broadcast(&sweeper);
                }
            });
            // Persist the config (codenotch-hook reads the port from it)
            {
                let st = handle.state::<AppState>();
                let c = st.cfg.lock().unwrap();
                config::save(&c);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Codenotch failed to start");
}
