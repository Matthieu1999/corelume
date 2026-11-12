#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sampler;
mod win;

use sampler::{Info, Sample, Sampler};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, State, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent, Wry,
};
use tauri_plugin_window_state::StateFlags;
use win::tasks;

/// 2 minutes at 1 Hz, kept while the window is closed so reopening shows it at once.
const HISTORY: usize = 120;
/// Desktop widgets, in the order they are laid out.
const WIDGETS: [&str; 5] = ["cpu", "gpu", "memory", "disks", "network"];
/// Their window labels; widget positions come from the settings, not the window-state plugin.
const WIDGET_LABELS: [&str; 5] = ["widget-cpu", "widget-gpu", "widget-memory", "widget-disks", "widget-network"];
/// Logical pixels: widget size, space between widgets, space kept from the screen edges.
const WIDGET_SIZE: (f64, f64) = (240.0, 128.0);
const WIDGET_GAP: f64 = 12.0;
const SCREEN_MARGIN: f64 = 16.0;
const THEMES: [&str; 3] = ["system", "light", "dark"];
/// One column, one row, or rows of two or three.
const LAYOUTS: [&str; 4] = ["column", "row", "grid2", "grid3"];

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    /// "C" or "F".
    unit: String,
    /// "system", "light" or "dark".
    theme: String,
    /// Widget id → shown.
    widgets: BTreeMap<String, bool>,
    /// Screen the widgets sit on, by its Windows name ("\\.\DISPLAY2"); "" or an unplugged
    /// screen means the main one.
    screen: String,
    /// Cell of a 5×5 grid over that screen where the widgets sit, 0 to 4 each.
    row: u8,
    col: u8,
    layout: String,
    /// Where a short last row goes in a grid layout: "left" or "right".
    align: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            unit: "C".into(),
            theme: "system".into(),
            widgets: BTreeMap::new(),
            screen: String::new(),
            row: 0,
            col: 4,
            layout: "column".into(),
            align: "left".into(),
        }
    }
}

impl Settings {
    /// The file and the page are outside input: keep only values Corelume knows.
    fn clean(mut self) -> Self {
        if self.unit != "F" {
            self.unit = "C".into();
        }
        if !THEMES.contains(&self.theme.as_str()) {
            self.theme = "system".into();
        }
        if !LAYOUTS.contains(&self.layout.as_str()) {
            self.layout = "column".into();
        }
        if self.align != "right" {
            self.align = "left".into();
        }
        self.row = self.row.min(4);
        self.col = self.col.min(4);
        self.widgets.retain(|id, _| WIDGETS.contains(&id.as_str()));
        self
    }
}

/// Top-left corners of `n` widgets, in logical pixels from the work area's top-left. The
/// block sits in cell (row, col) of a 5×5 grid over the work area and lines up like the
/// cell: left column flush left, right column flush right, centre column centred (same for
/// rows). It may spill past its cell, toward the middle of the screen, but stays on screen.
/// A short last row starts under the first column, or ends under the last with `right`.
fn widget_spots(n: usize, row: u8, col: u8, layout: &str, right: bool, area: (f64, f64)) -> Vec<(f64, f64)> {
    let (w, h) = WIDGET_SIZE;
    let cols = match layout {
        "row" => n,
        "grid2" => 2,
        "grid3" => 3,
        _ => 1,
    }
    .clamp(1, n.max(1));
    let rows = n.div_ceil(cols);
    let block = (cols as f64 * (w + WIDGET_GAP) - WIDGET_GAP, rows as f64 * (h + WIDGET_GAP) - WIDGET_GAP);
    let start = |index: u8, total: f64, size: f64| {
        let cell = total / 5.0;
        let at = cell * index as f64 + SCREEN_MARGIN + (cell - 2.0 * SCREEN_MARGIN - size) * index as f64 / 4.0;
        at.clamp(SCREEN_MARGIN, (total - size - SCREEN_MARGIN).max(SCREEN_MARGIN))
    };
    let (x0, y0) = (start(col, area.0, block.0), start(row, area.1, block.1));
    let shift = if right { cols - (n - rows.saturating_sub(1) * cols) } else { 0 };
    (0..n)
        .map(|i| {
            let (r, c) = (i / cols, i % cols + if i / cols == rows - 1 { shift } else { 0 });
            (x0 + c as f64 * (w + WIDGET_GAP), y0 + r as f64 * (h + WIDGET_GAP))
        })
        .collect()
}

#[derive(Serialize)]
struct Screen {
    /// Windows name, stored in the settings.
    name: String,
    /// "Screen 2 · 2560 × 1440 (main)", with the number Windows shows in Display settings.
    label: String,
}

/// "\\.\DISPLAY2" → 2, the number Windows' Display settings "Identify" shows.
fn screen_number(name: &str) -> Option<u32> {
    name.strip_prefix(r"\\.\DISPLAY")?.parse().ok()
}

/// The screen a widget layout uses: the named one if it is plugged in, else the main one.
fn widget_screen(app: &AppHandle, name: &str) -> Option<tauri::Monitor> {
    let all = app.available_monitors().unwrap_or_default();
    all.into_iter().find(|m| !name.is_empty() && m.name().is_some_and(|n| n == name)).or_else(|| app.primary_monitor().ok().flatten())
}

#[tauri::command]
fn list_screens(app: AppHandle) -> Vec<Screen> {
    let main = app.primary_monitor().ok().flatten().and_then(|m| m.name().cloned());
    let mut all: Vec<_> = app.available_monitors().unwrap_or_default().into_iter().filter(|m| m.name().is_some()).collect();
    all.sort_by_key(|m| m.name().and_then(|n| screen_number(n)));
    all.iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let name = m.name()?.clone();
            let number = screen_number(&name).unwrap_or(i as u32 + 1);
            let main = if main.as_ref() == Some(&name) { " (main)" } else { "" };
            let label = format!("Screen {number} · {} × {}{main}", m.size().width, m.size().height);
            Some(Screen { name, label })
        })
        .collect()
}

struct Shared {
    info: Info,
    history: Mutex<VecDeque<Sample>>,
    /// Page open in the main window, "" when it is closed: it picks the sample's detail.
    page: Mutex<String>,
    /// Page a widget asked for before the main window existed.
    pending_page: Mutex<Option<String>>,
    settings: Mutex<Settings>,
}

struct TrayAutostart(CheckMenuItem<Wry>);

#[derive(Serialize)]
struct Autostart {
    enabled: bool,
    /// Creating or deleting the elevated logon task needs admin.
    allowed: bool,
}

#[tauri::command]
fn get_info(shared: State<Shared>) -> Info {
    shared.info.clone()
}

#[tauri::command]
fn get_history(shared: State<Shared>) -> Vec<Sample> {
    shared.history.lock().map(|h| h.iter().cloned().collect()).unwrap_or_default()
}

#[tauri::command]
fn set_page(shared: State<Shared>, page: String) {
    if let Ok(mut p) = shared.page.lock() {
        *p = page;
    }
}

// Commands that may create a window are async: on Windows, building a WebView inside a
// synchronous command deadlocks (documented on tauri::WebviewWindowBuilder::new).

/// A widget click: show the main window on that component's page.
#[tauri::command]
async fn open_page(app: AppHandle, shared: State<'_, Shared>, page: String) -> Result<(), String> {
    if app.get_webview_window("main").is_some() {
        let _ = app.emit_to("main", "navigate", &page);
    } else if let Ok(mut p) = shared.pending_page.lock() {
        *p = Some(page);
    }
    show_main(&app);
    Ok(())
}

#[tauri::command]
fn take_page(shared: State<Shared>) -> Option<String> {
    shared.pending_page.lock().ok()?.take()
}

#[tauri::command]
fn get_settings(shared: State<Shared>) -> Settings {
    shared.settings.lock().map(|s| s.clone()).unwrap_or_default()
}

#[tauri::command]
async fn set_settings(app: AppHandle, shared: State<'_, Shared>, settings: Settings) -> Result<(), String> {
    let settings = settings.clean();
    if let Some(path) = settings_file(&app) {
        let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| format!("could not save the settings: {e}"))?;
    }
    apply_widgets(&app, &settings);
    apply_theme(&app, &settings.theme);
    if let Ok(mut s) = shared.settings.lock() {
        *s = settings.clone();
    }
    let _ = app.emit("settings", settings);
    Ok(())
}

#[tauri::command]
fn get_autostart() -> Autostart {
    Autostart { enabled: tasks::exists(tasks::LOGON_TASK), allowed: tasks::is_elevated() }
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Autostart {
    toggle_autostart(enabled);
    let state = get_autostart();
    let _ = app.state::<TrayAutostart>().0.set_checked(state.enabled);
    state
}

fn toggle_autostart(enabled: bool) {
    if enabled {
        let _ = tasks::create(tasks::LOGON_TASK);
    } else {
        tasks::remove(tasks::LOGON_TASK);
    }
}

fn run_sampler(app: AppHandle, mut sampler: Sampler) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let shared = app.state::<Shared>();
        let page = shared.page.lock().map(|p| p.clone()).unwrap_or_default();
        let sample = sampler.tick(&page);
        if let Ok(mut h) = shared.history.lock() {
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back(Sample { detail: None, ..sample.clone() });
        }
        // No window open means no listener: the event is dropped at no cost.
        let _ = app.emit("sample", sample);
    }
}

fn config_file(app: &AppHandle, name: &str) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(name))
}

fn settings_file(app: &AppHandle) -> Option<PathBuf> {
    config_file(app, "settings.json")
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_file(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|json| serde_json::from_str::<Settings>(&json).ok())
        .unwrap_or_default()
        .clean()
}

/// Remembers whether the window was open, so a logon start restores the last state.
/// Only the user's × marks it closed: Quit or a Windows shutdown keep "open".
fn window_marker(app: &AppHandle) -> Option<PathBuf> {
    config_file(app, "window-open")
}

fn set_window_open(app: &AppHandle, open: bool) {
    if let Some(path) = window_marker(app) {
        let _ = if open { std::fs::write(path, b"1") } else { std::fs::remove_file(path) };
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // No window yet, or it was closed and its WebView freed: build one from the config.
    // The window-state plugin puts it back on its last screen, position and size.
    if let Some(cfg) = app.config().app.windows.first() {
        if WebviewWindowBuilder::from_config(app, cfg).and_then(|b| b.build()).is_ok() {
            set_window_open(app, true);
            if let Some(shared) = app.try_state::<Shared>() {
                let theme = shared.settings.lock().map(|s| s.theme.clone()).unwrap_or_default();
                apply_theme(app, &theme);
            }
        }
    }
}

/// The title bar follows the chosen theme. "system" lets Windows decide, and the page then
/// reads the system choice through prefers-color-scheme.
fn apply_theme(app: &AppHandle, theme: &str) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_theme(match theme {
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            _ => None,
        });
    }
}

/// Opens the widgets the settings ask for, closes the others, and lays them out on the
/// chosen screen's work area (the screen minus the taskbar).
fn apply_widgets(app: &AppHandle, settings: &Settings) {
    let shown: Vec<&str> = WIDGETS.into_iter().filter(|id| settings.widgets.get(*id) == Some(&true)).collect();
    for (id, label) in WIDGETS.iter().zip(WIDGET_LABELS) {
        if !shown.contains(id) {
            if let Some(w) = app.get_webview_window(label) {
                let _ = w.destroy();
            }
        }
    }
    let Some(monitor) = widget_screen(app, &settings.screen) else { return };
    let (scale, area) = (monitor.scale_factor(), monitor.work_area());
    let size = (area.size.width as f64 / scale, area.size.height as f64 / scale);
    let spots = widget_spots(shown.len(), settings.row, settings.col, &settings.layout, settings.align == "right", size);
    for (id, (x, y)) in shown.into_iter().zip(spots) {
        let spot = PhysicalPosition::new(area.position.x + (x * scale).round() as i32, area.position.y + (y * scale).round() as i32);
        match app.get_webview_window(&format!("widget-{id}")) {
            Some(w) => {
                let _ = w.set_position(spot);
            }
            None => {
                if let Err(e) = build_widget(app, id, spot, scale) {
                    eprintln!("widget {id}: {e}");
                }
            }
        }
    }
}

fn build_widget(app: &AppHandle, id: &str, spot: PhysicalPosition<i32>, scale: f64) -> tauri::Result<()> {
    let (w, h) = WIDGET_SIZE;
    let window = WebviewWindowBuilder::new(app, format!("widget-{id}"), WebviewUrl::App(format!("widget.html#{id}").into()))
        .title(format!("Corelume widget: {id}"))
        .inner_size(w, h)
        .position(spot.x as f64 / scale, spot.y as f64 / scale)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .skip_taskbar(true)
        .always_on_bottom(true)
        // Never takes focus, not even on a click (WS_EX_NOACTIVATE), so it never comes forward.
        .focused(false)
        .focusable(false)
        .build()?;
    // The builder's logical position can land a few pixels off on mixed-DPI screens.
    window.set_position(spot)?;
    pin_to_desktop(&window);
    Ok(())
}

/// Owned by the desktop window (Progman), a widget comes back up with the desktop on
/// Win+D ("show desktop") instead of being hidden with every other window. Then it goes to
/// the bottom of the window stack: a new window starts on top, and always-on-bottom only
/// applies on the next move.
fn pin_to_desktop(window: &WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GWLP_HWNDPARENT, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos,
    };
    let Ok(hwnd) = window.hwnd() else { return };
    let class: Vec<u16> = "Progman".encode_utf16().chain([0]).collect();
    let progman = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    unsafe {
        if !progman.is_null() {
            SetWindowLongPtrW(hwnd.0, GWLP_HWNDPARENT, progman as isize);
        }
        SetWindowPos(hwnd.0, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Corelume", true, None::<&str>)?;
    // Creating or deleting the elevated logon task needs admin, which Corelume has when
    // it was started by its tasks.
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start with Windows",
        tasks::is_elevated(),
        tasks::exists(tasks::LOGON_TASK),
        None::<&str>,
    )?;
    app.manage(TrayAutostart(autostart.clone()));
    let quit = MenuItem::with_id(app, "quit", "Quit Corelume", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &autostart, &PredefinedMenuItem::separator(app)?, &quit])?;
    TrayIconBuilder::new()
        .icon(app.default_window_icon().expect("bundle icon missing").clone())
        .tooltip("Corelume")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, e| match e.id.as_ref() {
            "open" => show_main(app),
            "autostart" => {
                toggle_autostart(autostart.is_checked().unwrap_or(false));
                // Show what Task Scheduler really holds, whatever happened.
                let _ = autostart.set_checked(tasks::exists(tasks::LOGON_TASK));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    // WebView2 lets environment variables replace the browser it starts and the folder it
    // uses (Microsoft docs, CreateCoreWebView2EnvironmentWithOptions). Any program running
    // as the user can set them, and Corelume runs elevated: drop them before any WebView.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().to_ascii_uppercase().starts_with("WEBVIEW2_") {
            // SAFETY: first thing in main, before any other thread exists.
            unsafe { std::env::remove_var(&key) };
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);

    // Called elevated by the installer and uninstaller.
    if has("--install-tasks") {
        let result = tasks::create(tasks::OPEN_TASK).and_then(|_| tasks::create(tasks::LOGON_TASK));
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    if has("--remove-tasks") {
        tasks::remove(tasks::LOGON_TASK);
        tasks::remove(tasks::OPEN_TASK);
        return;
    }
    // A plain launch (Start menu) without admin hands over to the elevated "open" task.
    let autostart = has(tasks::AUTOSTART_ARG);
    if !autostart && !has(tasks::FROM_TASK_ARG) && !tasks::is_elevated() && tasks::exists(tasks::OPEN_TASK) && tasks::run(tasks::OPEN_TASK) {
        return;
    }

    tauri::Builder::default()
        // First, so a second copy hands over before anything else starts.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_main(app)))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
                .with_denylist(&WIDGET_LABELS)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_info,
            get_history,
            set_page,
            open_page,
            take_page,
            get_settings,
            set_settings,
            get_autostart,
            set_autostart,
            list_screens
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    set_window_open(window.app_handle(), false);
                    let _ = window.state::<Shared>().page.lock().map(|mut p| p.clear());
                } else {
                    // Widgets come and go with the settings, not with Alt+F4.
                    api.prevent_close();
                }
            }
        })
        .setup(move |app| {
            let (sampler, mut info) = Sampler::new()?;
            info.version = app.package_info().version.to_string();
            info.elevated = tasks::is_elevated();
            let settings = load_settings(app.handle());
            app.manage(Shared {
                info,
                history: Mutex::new(VecDeque::with_capacity(HISTORY)),
                page: Mutex::default(),
                pending_page: Mutex::default(),
                settings: Mutex::new(settings.clone()),
            });
            build_tray(app)?;
            let handle = app.handle().clone();
            std::thread::Builder::new().name("sampler".into()).spawn(move || run_sampler(handle, sampler))?;
            apply_widgets(app.handle(), &settings);
            // At logon, reopen only if the window was open last time; any other start opens it.
            let reopen = window_marker(app.handle()).is_some_and(|p| p.exists());
            if !autostart || reopen {
                show_main(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start Corelume")
        .run(|_, event| {
            // Closing the window frees its WebView; the app lives on in the tray until Quit.
            if let RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_outside_are_cleaned() {
        let json = r#"{"unit":"K","theme":"neon","widgets":{"cpu":true,"evil":true},"row":9,"layout":"x","align":"up","locked":true}"#;
        let s = serde_json::from_str::<Settings>(json).unwrap().clean();
        assert_eq!((s.unit.as_str(), s.theme.as_str(), s.layout.as_str(), s.align.as_str()), ("C", "system", "column", "left"));
        assert_eq!(s.widgets.keys().collect::<Vec<_>>(), ["cpu"]);
        assert_eq!((s.row, s.col), (4, 4), "row clamped, missing col takes its default");
    }

    #[test]
    fn screens_keep_the_windows_number() {
        assert_eq!(screen_number(r"\\.\DISPLAY2"), Some(2));
        assert_eq!(screen_number(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(screen_number("Generic PnP Monitor"), None);
    }

    #[test]
    fn widgets_line_up_with_their_grid_cell() {
        let area = (1920.0, 1040.0);
        // Top-right, one column: flush with the right edge, stacked down.
        let s = widget_spots(5, 0, 4, "column", false, area);
        assert_eq!(s[0], (1920.0 - 16.0 - 240.0, 16.0));
        assert_eq!(s[4], (1664.0, 16.0 + 4.0 * 140.0));
        // Bottom-left, one row: flush with the bottom edge, going right.
        let s = widget_spots(5, 4, 0, "row", false, area);
        assert_eq!(s[0], (16.0, 1040.0 - 16.0 - 128.0));
        assert_eq!(s[1].0, 16.0 + 252.0);
        // Centre, rows of two: the 492 x 408 block is centred on the screen.
        let s = widget_spots(5, 2, 2, "grid2", false, area);
        assert_eq!(s[0], (960.0 - 246.0, 520.0 - 204.0));
        assert_eq!(s[2], (714.0, 316.0 + 140.0));
        // A row wider than the screen still starts on screen.
        let s = widget_spots(5, 0, 4, "row", false, (1000.0, 600.0));
        assert_eq!(s[0].0, 16.0);
        assert!(widget_spots(0, 0, 0, "grid3", true, area).is_empty());
        // Five in rows of two: the fifth sits under the left column, or the right one.
        assert_eq!(widget_spots(5, 0, 0, "grid2", false, area)[4], (16.0, 16.0 + 2.0 * 140.0));
        assert_eq!(widget_spots(5, 0, 0, "grid2", true, area)[4], (16.0 + 252.0, 16.0 + 2.0 * 140.0));
        // Rows of three: two left over end under the second and third columns; a full row moves nothing.
        let s = widget_spots(5, 0, 0, "grid3", true, area);
        assert_eq!((s[3].0, s[4].0), (16.0 + 252.0, 16.0 + 504.0));
        assert_eq!(widget_spots(4, 0, 0, "grid2", true, area)[3].0, 16.0 + 252.0);
    }
}
