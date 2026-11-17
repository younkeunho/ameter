//! ameter.exe: the taskbar meter. Top line Codex (CO) and Higgsfield credits (HF), bottom line
//! Claude (CC); the mini style puts Codex, Claude and Higgsfield on one line. Right-click switches
//! items on and off; double-click refreshes.
//!
//! Two windows: a hidden top-level host (timer, notification icon, menu, Explorer restarts) and
//! the meter itself, a layered child of the taskbar. As part of the taskbar the meter moves,
//! hides and rises with it: opening Start lifts the taskbar above every topmost window, which a
//! separate topmost window could only chase a frame late.

mod bar;
mod gdip;
mod taskbar;

use crate::claude_setup;
use crate::claude_web::{ClaudeWeb, WebStatus};
use crate::codex::Codex;
use crate::fmt;
use crate::higgsfield::Higgsfield;
use crate::model::{self, ProviderState, WindowKind};
use crate::paths;
use crate::poller::Poller;
use crate::util::{self, JsonExt, wide};
use bar::{Cell, Fonts, Label, Layout, Row, color};
use gdip::{Canvas, Gdiplus};
use serde_json::json;
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const TIMER_TICK: usize = 1;

const CODEX_STALE_MS: i64 = 5 * 60 * 1000;
const HIGGSFIELD_STALE_MS: i64 = 5 * 60 * 1000;
/// claude.ai is read every 2 minutes.
const CLAUDE_WEB_STALE_MS: i64 = 10 * 60 * 1000;
/// Statusline values are only fresh while Claude Code is in use.
const CLAUDE_STATUSLINE_STALE_MS: i64 = 30 * 60 * 1000;

const MENU_REFRESH: usize = 1;
const MENU_CODEX: usize = 2;
const MENU_CLAUDE_WEEKLY: usize = 3;
const MENU_CLAUDE_FABLE: usize = 4;
const MENU_HIGGSFIELD: usize = 5;
const MENU_MINI: usize = 6;
const MENU_EXIT: usize = 9;

/// What the bar shows; toggled from the menu and kept in display.json.
#[derive(Clone, Copy)]
struct Display {
    codex: bool,
    claude_weekly: bool,
    claude_fable: bool,
    higgsfield: bool,
    /// Mini style: one line of Codex, Claude and Higgsfield, each led by a dot in its color, without
    /// countdowns; only a provider's second window keeps its label.
    mini: bool,
}

impl Display {
    fn claude(&self) -> bool {
        self.claude_weekly || self.claude_fable
    }

    fn load() -> Display {
        let json = util::read_json_object(&paths::display_settings()).unwrap_or_default();
        Self::from_json(&json)
    }

    fn from_json(json: &serde_json::Map<String, serde_json::Value>) -> Display {
        let get = |key: &str| json.bool_of(key).unwrap_or(true);
        Display {
            codex: get("codex"),
            claude_weekly: get("claudeWeekly"),
            claude_fable: get("claudeFable"),
            higgsfield: get("higgsfield"),
            mini: json.bool_of("mini").unwrap_or(false),
        }
    }

    fn to_json(self) -> serde_json::Value {
        json!({
            "codex": self.codex,
            "claudeWeekly": self.claude_weekly,
            "claudeFable": self.claude_fable,
            "higgsfield": self.higgsfield,
            "mini": self.mini,
        })
    }

    fn save(&self) {
        if let Err(e) = util::write_json(&paths::display_settings(), &self.to_json()) {
            util::log(&format!("display settings save failed: {e}"));
        }
    }
}

/// Written by the poller threads, read by the UI thread after WM_APP_UPDATE.
#[derive(Default)]
struct Shared {
    codex: ProviderState,
    higgsfield: ProviderState,
    /// None until the first claude.ai read finishes.
    web: Option<WebStatus>,
}

struct Pollers {
    codex: Poller,
    claude_web: Poller,
    higgsfield: Poller,
}

struct App {
    hwnd: HWND,
    shared: Arc<Mutex<Shared>>,
    pollers: Pollers,
    display: Display,
    claude: ProviderState,
    claude_stamp: Option<SystemTime>,
    claude_installed: bool,
    installed_checked_ms: i64,
    measuring: Canvas,
    fonts: Fonts,
    rows: Vec<Row>,
    layout: Layout,
    /// Where the meter was last drawn, in taskbar coordinates; None until drawn.
    drawn_at: Option<(i32, i32)>,
    hidden: bool,
    tray_icon: HICON,
    tray_tip: String,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// The host window.
static HWND_VALUE: AtomicIsize = AtomicIsize::new(0);
/// The meter window, 0 while there is none. Cleared when it is destroyed, by ameter or because
/// Explorer (its parent) went away.
static METER: AtomicIsize = AtomicIsize::new(0);

const HOST_CLASS: &str = "ameter.host";
const METER_CLASS: &str = "ameter.meter";

/// Runs `f` on the app unless it is already borrowed (a nested message during a call).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok().and_then(|mut app| app.as_mut().map(f)))
}

fn post(message: u32) {
    let hwnd = HWND_VALUE.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, message, 0, 0) };
    }
}

/// DPI awareness (per-monitor v2) and the Windows 10 compatibility that layered child windows
/// need come from the manifest build.rs embeds.
pub fn run() {
    let _instance = match single_instance() {
        Some(handle) => handle,
        None => return,
    };
    util::set_log_name("ameter");
    let _ = std::fs::create_dir_all(paths::data_dir());
    util::log(&format!("ameter {} started", crate::VERSION));

    let Some(_gdiplus) = Gdiplus::start() else {
        util::log("GDI+ failed to start");
        return;
    };
    let Some(hwnd) = create_host() else {
        util::log("window creation failed");
        return;
    };
    HWND_VALUE.store(hwnd as isize, Ordering::SeqCst);

    let mut codex = model::load_codex();
    if codex.updated_at_ms == 0 {
        codex.status = Some("확인 중…".into());
    }
    let mut higgsfield = model::load_higgsfield();
    if higgsfield.credits.is_none() {
        higgsfield.status = Some("확인 중…".into());
    }
    let shared = Arc::new(Mutex::new(Shared { codex: codex.clone(), higgsfield: higgsfield.clone(), web: None }));

    let pollers = {
        let s = shared.clone();
        let codex_poller = Poller::start(Codex::new(codex), move |state| {
            if let Ok(mut shared) = s.lock() {
                shared.codex = state;
            }
            post(WM_APP_UPDATE);
        });
        let s = shared.clone();
        let web_poller = Poller::start(ClaudeWeb::default(), move |status| {
            if let Ok(mut shared) = s.lock() {
                shared.web = Some(status);
            }
            post(WM_APP_UPDATE);
        });
        let s = shared.clone();
        let higgsfield_poller = Poller::start(Higgsfield::new(higgsfield), move |state| {
            if let Ok(mut shared) = s.lock() {
                shared.higgsfield = state;
            }
            post(WM_APP_UPDATE);
        });
        Pollers { codex: codex_poller, claude_web: web_poller, higgsfield: higgsfield_poller }
    };

    let scale = taskbar::locate().map(|t| t.scale).unwrap_or(1.0);
    let mut app = App {
        hwnd,
        shared,
        pollers,
        display: Display::load(),
        claude: model::load_claude(),
        claude_stamp: modified(&paths::claude_cache()),
        claude_installed: claude_setup::is_installed(),
        installed_checked_ms: util::now_ms(),
        measuring: Canvas::owned(1, 1).expect("GDI+ canvas"),
        fonts: Fonts::new(scale),
        rows: Vec::new(),
        layout: Layout::default(),
        drawn_at: None,
        hidden: true,
        tray_icon: tray_icon(),
        tray_tip: String::new(),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
    };
    app.apply_display();
    app.tray(NIM_ADD);
    app.sync(true);
    APP.with(|cell| *cell.borrow_mut() = Some(app));

    unsafe { SetTimer(hwnd, TIMER_TICK, 1000, None) };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.with(|cell| cell.borrow_mut().take());
    util::log("ameter stopped");
}

/// One meter per data directory.
fn single_instance() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    let key = paths::data_dir().to_string_lossy().to_lowercase();
    let hash = key.encode_utf16().fold(0x811C_9DC5u32, |h, c| (h ^ c as u32).wrapping_mul(0x0100_0193));
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide(&format!("Local\\ameter.widget.{hash:08x}")).as_ptr()) };
    (!handle.is_null() && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS).then_some(handle)
}

fn register_class(name: &str, proc: WNDPROC, style: WNDCLASS_STYLES) -> bool {
    let class = wide(name);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style,
        lpfnWndProc: proc,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        hIcon: std::ptr::null_mut(),
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    unsafe { RegisterClassExW(&wc) != 0 }
}

/// The never-shown top-level window that owns the timer, the notification icon and the menu.
fn create_host() -> Option<HWND> {
    if !register_class(HOST_CLASS, Some(window_proc), 0) || !register_class(METER_CLASS, Some(meter_proc), CS_DBLCLKS) {
        return None;
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(HOST_CLASS).as_ptr(),
            wide("ameter").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    (!hwnd.is_null()).then_some(hwnd)
}

/// The meter: a hidden layered child of `taskbar`, drawn with UpdateLayeredWindow.
fn create_meter(taskbar: HWND) -> HWND {
    unsafe {
        CreateWindowExW(
            // No WM_PARENTNOTIFY: Explorer never waits on ameter.
            WS_EX_LAYERED | WS_EX_NOPARENTNOTIFY,
            wide(METER_CLASS).as_ptr(),
            wide("ameter").as_ptr(),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            taskbar,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    }
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

unsafe extern "system" fn meter_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_RBUTTONUP => {
            show_menu(HWND_VALUE.load(Ordering::SeqCst) as HWND);
            return 0;
        }
        WM_LBUTTONDBLCLK => {
            with_app(App::refresh_all);
            return 0;
        }
        WM_NCDESTROY => {
            let _ = METER.compare_exchange(hwnd as isize, 0, Ordering::SeqCst, Ordering::SeqCst);
            // Explorer took the meter down with the taskbar: build a new one once it is back.
            post(WM_APP_UPDATE);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam == TIMER_TICK => {
            with_app(App::tick);
            return 0;
        }
        WM_APP_UPDATE => {
            with_app(App::on_update);
            return 0;
        }
        WM_APP_TRAY => {
            match lparam as u32 {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONDBLCLK => {
                    with_app(App::refresh_all);
                }
                _ => {}
            }
            return 0;
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED => {
            with_app(|app| app.sync(true));
        }
        WM_DESTROY => {
            with_app(App::shutdown);
            unsafe { PostQuitMessage(0) };
            return 0;
        }
        _ => {
            // Explorer restarted: the notification area is new and empty.
            if with_app(|app| msg == app.taskbar_created && msg != 0) == Some(true) {
                with_app(|app| {
                    app.tray(NIM_ADD);
                    app.sync(true);
                });
                return 0;
            }
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The context menu, owned by the host window.
fn show_menu(hwnd: HWND) {
    let Some(display) = with_app(|app| app.display) else {
        return;
    };
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let command = unsafe {
        let menu = CreatePopupMenu();
        let claude = CreatePopupMenu();
        AppendMenuW(claude, MF_STRING | check(display.claude_weekly), MENU_CLAUDE_WEEKLY, wide("주간 (W)").as_ptr());
        AppendMenuW(claude, MF_STRING | check(display.claude_fable), MENU_CLAUDE_FABLE, wide("페이블 (F)").as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_REFRESH, wide("새로고침").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING | check(display.codex), MENU_CODEX, wide("코덱스").as_ptr());
        AppendMenuW(menu, MF_POPUP | check(display.claude()), claude as usize, wide("클로드").as_ptr());
        AppendMenuW(menu, MF_STRING | check(display.higgsfield), MENU_HIGGSFIELD, wide("힉스필드").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING | check(display.mini), MENU_MINI, wide("미니 스타일").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, wide("종료").as_ptr());

        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        // Take the foreground so the menu closes on an outside click.
        SetForegroundWindow(hwnd);
        let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_RIGHTALIGN | TPM_BOTTOMALIGN;
        let command = TrackPopupMenu(menu, flags, cursor.x, cursor.y, 0, hwnd, std::ptr::null()) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu); // destroys the submenu too
        command
    };
    with_app(|app| app.on_command(command));
}

/// The application icon embedded by build.rs, at the notification area's size.
fn tray_icon() -> HICON {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
    let dpi = unsafe { GetDpiForSystem() };
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // Resource id 1 (MAKEINTRESOURCE).
    unsafe { LoadImageW(instance, 1 as windows_sys::core::PCWSTR, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as HICON }
}

impl App {
    fn tick(&mut self) {
        let stamp = modified(&paths::claude_cache());
        if stamp != self.claude_stamp {
            self.claude_stamp = stamp;
            self.claude = model::load_claude();
        }
        let now = util::now_ms();
        if now - self.installed_checked_ms > 60_000 {
            self.installed_checked_ms = now;
            self.claude_installed = claude_setup::is_installed();
        }
        self.sync(false);
    }

    fn on_update(&mut self) {
        // claude.ai results land in the shared Claude cache.
        self.claude_stamp = modified(&paths::claude_cache());
        self.claude = model::load_claude();
        self.sync(false);
    }

    fn refresh_all(&mut self) {
        self.pollers.codex.refresh_now();
        self.pollers.claude_web.refresh_now();
        self.pollers.higgsfield.refresh_now();
    }

    fn on_command(&mut self, command: usize) {
        match command {
            MENU_REFRESH => return self.refresh_all(),
            MENU_EXIT => {
                // Closed from the message loop, outside this borrow, so WM_DESTROY can shut down.
                unsafe { PostMessageW(self.hwnd, WM_CLOSE, 0, 0) };
                return;
            }
            MENU_CODEX => self.display.codex = !self.display.codex,
            MENU_CLAUDE_WEEKLY => self.display.claude_weekly = !self.display.claude_weekly,
            MENU_CLAUDE_FABLE => self.display.claude_fable = !self.display.claude_fable,
            MENU_HIGGSFIELD => self.display.higgsfield = !self.display.higgsfield,
            MENU_MINI => self.display.mini = !self.display.mini,
            _ => return,
        }
        self.display.save();
        self.apply_display();
        self.sync(false);
    }

    /// Hidden providers are not polled.
    fn apply_display(&self) {
        self.pollers.codex.set_paused(!self.display.codex);
        self.pollers.claude_web.set_paused(!self.display.claude());
        self.pollers.higgsfield.set_paused(!self.display.higgsfield);
    }

    fn shutdown(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER_TICK) };
        let meter = METER.load(Ordering::SeqCst) as HWND;
        if !meter.is_null() {
            unsafe { DestroyWindow(meter) };
        }
        self.tray(NIM_DELETE);
        if !self.tray_icon.is_null() {
            unsafe { DestroyIcon(self.tray_icon) };
        }
    }

    fn tray(&mut self, action: u32) {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.tray_icon;
        for (slot, unit) in data.szTip.iter_mut().zip(self.tray_tip.encode_utf16().take(127)) {
            *slot = unit;
        }
        unsafe { Shell_NotifyIconW(action, &data) };
    }

    fn build_rows(&self) -> (Vec<Row>, String) {
        let now_ms = util::now_ms();
        let now_sec = now_ms / 1000;
        let shared = self.shared.lock().map(|s| (s.codex.clone(), s.higgsfield.clone(), s.web.clone()));
        let (codex, higgsfield, web) = shared.unwrap_or_default();
        let claude_stale = if web.as_ref().is_some_and(|w| w.available) { CLAUDE_WEB_STALE_MS } else { CLAUDE_STATUSLINE_STALE_MS };
        let claude = claude_view(&self.claude, web.as_ref(), self.claude_installed, now_ms, claude_stale);

        let d = self.display;
        let mut rows = Vec::new();
        if d.mini {
            let mut providers = Vec::new();
            if d.codex {
                providers.push((color::CODEX, windows(&codex, now_sec, now_ms, CODEX_STALE_MS, true, true)));
            }
            if d.claude() {
                providers.push((color::CLAUDE, windows(&claude, now_sec, now_ms, claude_stale, d.claude_weekly, d.claude_fable)));
            }
            if d.higgsfield {
                providers.push((color::HIGGSFIELD, vec![credits_cell(Label::None, &higgsfield, now_sec, now_ms)]));
            }
            let line = mini_line(providers);
            if !line.cells.is_empty() {
                rows.push(line);
            }
        } else {
            let mut top = Row { name: d.codex.then_some(Label::Name("CO")), name_color: color::CODEX, cells: Vec::new() };
            if d.codex {
                top.cells.extend(windows(&codex, now_sec, now_ms, CODEX_STALE_MS, true, true));
            }
            if d.higgsfield {
                top.cells.push(credits_cell(Label::Name("HF"), &higgsfield, now_sec, now_ms));
            }
            if !top.cells.is_empty() {
                rows.push(top);
            }
            if d.claude() {
                let mut bottom = Row { name: Some(Label::Name("CC")), name_color: color::CLAUDE, cells: Vec::new() };
                bottom.cells.extend(windows(&claude, now_sec, now_ms, claude_stale, d.claude_weekly, d.claude_fable));
                rows.push(bottom);
            }
        }

        let tip = [
            fmt::terminal_line("CO", &codex, now_sec, false),
            fmt::terminal_line("HF", &higgsfield, now_sec, false),
            fmt::terminal_line("CC", &claude, now_sec, false),
        ]
        .join("\n");
        (rows, tip)
    }

    /// The meter inside the current taskbar, created on first use and again after Explorer restarts.
    fn meter(&mut self, taskbar: HWND) -> Option<HWND> {
        let mut meter = METER.load(Ordering::SeqCst) as HWND;
        if !meter.is_null() && unsafe { GetParent(meter) } != taskbar {
            unsafe { DestroyWindow(meter) };
            meter = std::ptr::null_mut();
        }
        if meter.is_null() {
            meter = create_meter(taskbar);
            if meter.is_null() {
                return None;
            }
            METER.store(meter as isize, Ordering::SeqCst);
            self.hidden = true;
            self.drawn_at = None;
        }
        Some(meter)
    }

    /// Placement, redraw on change, and the meter's place above the taskbar's own content.
    fn sync(&mut self, force: bool) {
        let (rows, tip) = self.build_rows();
        if tip != self.tray_tip {
            self.tray_tip = tip;
            self.tray(NIM_MODIFY);
        }

        let Some(taskbar) = taskbar::locate() else {
            self.rows = rows;
            return;
        };
        let Some(meter) = self.meter(taskbar.hwnd) else {
            self.rows = rows;
            return;
        };
        // Hidden while everything is switched off.
        let hidden = rows.is_empty();
        if hidden {
            if !self.hidden {
                self.hidden = true;
                unsafe { ShowWindow(meter, SW_HIDE) };
            }
            self.rows = rows;
            return;
        }

        let mut redraw = force;
        if (taskbar.scale - self.fonts.scale).abs() > f32::EPSILON {
            self.fonts = Fonts::new(taskbar.scale);
            redraw = true;
        }
        if redraw || rows != self.rows || self.drawn_at.is_none() {
            self.layout = bar::measure(&rows, &self.measuring, &self.fonts);
            self.rows = rows;
            redraw = true;
        }
        let at = taskbar::place(&taskbar, self.layout.width, self.layout.height);
        if redraw || self.drawn_at != Some(at) {
            self.present(meter, at);
            self.drawn_at = Some(at);
        }
        if self.hidden {
            self.hidden = false;
            unsafe { ShowWindow(meter, SW_SHOWNA) };
        }
        // The taskbar's XAML content is a sibling window covering the whole bar: stay above it.
        if unsafe { GetWindow(taskbar.hwnd, GW_CHILD) } != meter {
            unsafe { SetWindowPos(meter, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
        }
    }

    /// Moves the meter to `(x, y)` in taskbar coordinates and paints premultiplied ARGB straight
    /// into a DIB section for UpdateLayeredWindow.
    fn present(&self, meter: HWND, (x, y): (i32, i32)) {
        let (width, height) = (self.layout.width.max(1), self.layout.height.max(1));
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let memory = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height; // top-down
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if !dib.is_null() && !bits.is_null() {
                let previous = SelectObject(memory, dib);
                if let Some(canvas) = Canvas::on_pixels(width, height, bits as *mut u8) {
                    bar::paint(&self.rows, &self.layout, &canvas, &self.fonts);
                    canvas.flush();
                }
                // A child's position is in its parent's coordinates, which UpdateLayeredWindow does
                // not take: move first, then hand over the pixels at the new size.
                SetWindowPos(meter, std::ptr::null_mut(), x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW);
                let source = POINT { x: 0, y: 0 };
                let size = SIZE { cx: width, cy: height };
                let blend =
                    BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
                UpdateLayeredWindow(meter, screen, std::ptr::null(), &size, memory, &source, 0, &blend, ULW_ALPHA);
                SelectObject(memory, previous);
                DeleteObject(dib);
            }
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
    }
}

/// Claude's cached percentages are only useful while fresh. A failed web read must not leave
/// an old percentage on the meter indefinitely, but a recent statusline update is still usable.
/// Keep the cache intact so a successful refresh restores the values without losing history.
fn claude_view(cached: &ProviderState, web: Option<&WebStatus>, installed: bool, now_ms: i64, stale_ms: i64) -> ProviderState {
    let mut state = cached.clone();
    let stale = state.updated_at_ms <= 0 || now_ms.saturating_sub(state.updated_at_ms) > stale_ms;
    state.failing = stale || web.is_some_and(|w| w.available && !w.ok);
    if stale {
        state.windows.clear();
    }
    if state.windows.is_empty() {
        let web_source = web.is_none_or(|w| w.available);
        state.status = Some(match web {
            Some(w) if w.available && !w.ok => w.status.clone().unwrap_or_else(|| "연결 실패".into()),
            _ if !cached.windows.is_empty() => "갱신 필요".into(),
            Some(w) if w.ok => "데이터 없음".into(),
            _ if web_source => "확인 중…".into(),
            _ if installed => "대기 중".into(),
            _ => "연결 필요".into(),
        });
    }
    state
}

/// Mini style: one line of providers in the given order, without countdowns. A dot in the
/// provider's color takes the place of its first value's label; a second window of the same
/// provider keeps its label, so the two stay apart.
fn mini_line(providers: Vec<(u32, Vec<Cell>)>) -> Row {
    let mut cells = Vec::new();
    for (dot_color, group) in providers {
        for (i, mut cell) in group.into_iter().enumerate() {
            cell.reset.clear();
            if i == 0 {
                // Fable-only mode still needs an F; otherwise it looks like the weekly bucket.
                if cell.label != Label::Plain("F".into()) {
                    cell.label = Label::Dot;
                }
                cell.label_color = dot_color;
            }
            cells.push(cell);
        }
    }
    Row { name: None, name_color: 0, cells }
}

/// "W 94% 6d0h" per window, or one status cell while there are no values.
fn windows(state: &ProviderState, now_sec: i64, now_ms: i64, stale_ms: i64, timed: bool, fable: bool) -> Vec<Cell> {
    if !timed && !fable {
        return Vec::new();
    }
    if state.windows.is_empty() {
        return vec![Cell::status(state.status.as_deref().unwrap_or("—"))];
    }
    let dim = state.failing || state.updated_at_ms == 0 || now_ms - state.updated_at_ms > stale_ms;
    let mut cells = Vec::new();
    for w in fmt::ordered(&state.windows) {
        if (w.kind == WindowKind::Timed && !timed) || (w.kind == WindowKind::Fable && !fable) {
            continue;
        }
        let mut cell = Cell { label: Label::Plain(fmt::label(w)), ..Default::default() };
        match fmt::current_used(state, w, now_sec) {
            None => {
                cell.value = "—".into();
                cell.value_color = color::MUTED;
            }
            Some(used) => {
                cell.value = fmt::left(used);
                cell.reset = fmt::countdown(w, now_sec).unwrap_or_default();
                cell.value_color = color::for_severity(fmt::severity(used));
            }
        }
        if dim {
            cell.value_color = color::dimmed(cell.value_color);
        }
        cells.push(cell);
    }
    if cells.is_empty() { vec![Cell::status("데이터 없음")] } else { cells }
}

/// "HF 7,747 11d22h": remaining credits and the time until the monthly refill.
fn credits_cell(label: Label, state: &ProviderState, now_sec: i64, now_ms: i64) -> Cell {
    let mut cell = Cell { label, label_color: color::HIGGSFIELD, ..Default::default() };
    match state.credits {
        None => {
            cell.value = state.status.clone().unwrap_or_else(|| "—".into());
            cell.value_color = color::MUTED;
        }
        Some(credits) => {
            let dim = state.failing || now_ms - state.updated_at_ms > HIGGSFIELD_STALE_MS;
            cell.value = fmt::credits(credits);
            cell.value_color = if dim { color::dimmed(color::VALUE) } else { color::VALUE };
            cell.reset = state.renews_at.filter(|r| *r > now_sec).map(|r| fmt::short(r - now_sec)).unwrap_or_default();
        }
    }
    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_cache(updated_at_ms: i64) -> ProviderState {
        ProviderState {
            windows: vec![model::UsageWindow {
                duration_mins: Some(model::WEEK_MINS),
                used_percent: Some(78.0),
                resets_at: None,
                ..Default::default()
            }],
            updated_at_ms,
            ..Default::default()
        }
    }

    #[test]
    fn claude_stale_cache_shows_login_failure_in_both_styles_and_tooltip() {
        let now = 10_000_000;
        let cached = claude_cache(now - CLAUDE_WEB_STALE_MS - 1);
        let web = WebStatus { available: true, ok: false, status: Some("로그인 필요".into()) };
        let view = claude_view(&cached, Some(&web), true, now, CLAUDE_WEB_STALE_MS);
        assert!(view.windows.is_empty());
        assert!(view.failing);
        assert_eq!(view.status.as_deref(), Some("로그인 필요"));
        assert_eq!(cached.windows[0].used_percent, Some(78.0));
        let cells = windows(&view, now / 1000, now, CLAUDE_WEB_STALE_MS, true, true);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].status.as_deref(), Some("로그인 필요"));
        let mini = mini_line(vec![(color::CLAUDE, cells)]);
        assert_eq!(mini.cells[0].status.as_deref(), Some("로그인 필요"));
        assert_eq!(fmt::terminal_line("CC", &view, now / 1000, false), "CC  로그인 필요");
    }

    #[test]
    fn claude_recent_cache_survives_web_failure_until_the_freshness_boundary() {
        let now = 10_000_000;
        let web = WebStatus { available: true, ok: false, status: Some("연결 실패".into()) };
        for age in [0, CLAUDE_WEB_STALE_MS] {
            let cached = claude_cache(now - age);
            let view = claude_view(&cached, Some(&web), true, now, CLAUDE_WEB_STALE_MS);
            assert_eq!(view.windows, cached.windows);
            assert!(view.failing);
            let cells = windows(&view, now / 1000, now, CLAUDE_WEB_STALE_MS, true, true);
            assert_eq!(cells[0].value, "22%");
            assert!(cells[0].status.is_none());
        }
    }

    #[test]
    fn claude_unknown_or_stale_timestamp_never_displays_a_percentage() {
        let now = 10_000_000;
        let web = WebStatus { available: true, ok: true, status: None };
        for updated in [0, now - CLAUDE_WEB_STALE_MS - 1] {
            let view = claude_view(&claude_cache(updated), Some(&web), true, now, CLAUDE_WEB_STALE_MS);
            assert!(view.windows.is_empty());
            assert_eq!(view.status.as_deref(), Some("갱신 필요"));
        }
        let cached = claude_cache(now - CLAUDE_STATUSLINE_STALE_MS - 1);
        let view = claude_view(&cached, None, true, now, CLAUDE_STATUSLINE_STALE_MS);
        assert!(view.windows.is_empty());
        assert_eq!(view.status.as_deref(), Some("갱신 필요"));
    }

    #[test]
    fn claude_new_observation_recovers_after_login_failure() {
        let now = 10_000_000;
        let web = WebStatus { available: true, ok: true, status: None };
        let mut cached = claude_cache(now);
        cached.windows[0].used_percent = Some(85.0);
        let view = claude_view(&cached, Some(&web), true, now, CLAUDE_WEB_STALE_MS);
        assert_eq!(view.windows, cached.windows);
        assert!(!view.failing);
        assert!(view.status.is_none());
        let cells = windows(&view, now / 1000, now, CLAUDE_WEB_STALE_MS, true, true);
        assert_eq!(cells[0].value, "15%");
    }

    #[test]
    fn claude_without_cached_windows_explains_the_source_state() {
        let cached = ProviderState::default();
        let unavailable = WebStatus::default();
        let failed = WebStatus { available: true, ok: false, status: None };
        for (web, installed, expected) in [
            (None, false, "확인 중…"),
            (Some(&unavailable), true, "대기 중"),
            (Some(&unavailable), false, "연결 필요"),
            (Some(&failed), true, "연결 실패"),
        ] {
            let view = claude_view(&cached, web, installed, 10_000_000, CLAUDE_WEB_STALE_MS);
            assert!(view.windows.is_empty());
            assert_eq!(view.status.as_deref(), Some(expected));
        }
    }

    #[test]
    fn weekly_and_fable_toggles_select_independent_values_even_with_the_same_duration() {
        let now = 10_000_000;
        let mut state = claude_cache(now);
        state.windows.push(model::UsageWindow {
            kind: WindowKind::Fable,
            duration_mins: Some(model::WEEK_MINS),
            used_percent: Some(32.0),
            resets_at: None,
        });
        for (weekly, fable, labels, values) in [
            (true, true, vec!["W", "F"], vec!["22%", "68%"]),
            (true, false, vec!["W"], vec!["22%"]),
            (false, true, vec!["F"], vec!["68%"]),
            (false, false, vec![], vec![]),
        ] {
            let cells = windows(&state, now / 1000, now, CLAUDE_WEB_STALE_MS, weekly, fable);
            assert_eq!(
                cells.iter().map(|c| c.label.clone()).collect::<Vec<_>>(),
                labels.iter().map(|s| Label::Plain((*s).into())).collect::<Vec<_>>()
            );
            assert_eq!(cells.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(), values);
        }
        let cells = windows(&state, now / 1000, now, CLAUDE_WEB_STALE_MS, false, true);
        let mini = mini_line(vec![(color::CLAUDE, cells)]);
        assert_eq!(mini.cells[0].label, Label::Plain("F".into()));
        assert_eq!(mini.cells[0].value, "68%");
        assert_eq!(mini.cells[0].label_color, color::CLAUDE);
    }

    #[test]
    fn fable_not_reported_by_the_server_is_not_shown_as_unused() {
        let now = 10_000_000;
        let cells = windows(&claude_cache(now), now / 1000, now, CLAUDE_WEB_STALE_MS, false, true);
        assert_eq!(cells[0].status.as_deref(), Some("데이터 없음"));
        assert!(cells[0].value.is_empty());
    }

    #[test]
    fn claude_toggle_preferences_round_trip_without_the_removed_five_hour_setting() {
        for (weekly, fable) in [(false, false), (false, true), (true, false), (true, true)] {
            let input = json!({"claudeWeekly":weekly,"claudeFable":fable,"claudeFiveHour":!fable,"mini":true});
            let display = Display::from_json(input.as_object().unwrap());
            assert_eq!(display.claude(), weekly || fable);
            let saved = display.to_json();
            assert!(saved.get("claudeFiveHour").is_none());
            let loaded = Display::from_json(saved.as_object().unwrap());
            assert_eq!((loaded.claude_weekly, loaded.claude_fable, loaded.mini), (weekly, fable, true));
        }
    }

    fn window(label: &str) -> Cell {
        Cell { label: Label::Plain(label.into()), value: "90%".into(), reset: "3d6h".into(), ..Default::default() }
    }

    #[test]
    fn mini_line_keeps_provider_order_with_a_dot_before_each() {
        let credits = Cell { value: "7,113".into(), reset: "10d2h".into(), ..Default::default() };
        let line = mini_line(vec![
            (color::CODEX, vec![window("W")]),
            (color::CLAUDE, vec![Cell::status("연결 필요")]),
            (color::HIGGSFIELD, vec![credits]),
        ]);
        assert_eq!(line.name, None);
        let marks = line.cells.iter().map(|c| (c.label.clone(), c.label_color)).collect::<Vec<_>>();
        assert_eq!(marks, [(Label::Dot, color::CODEX), (Label::Dot, color::CLAUDE), (Label::Dot, color::HIGGSFIELD)]);
        assert_eq!(line.cells[1].status.as_deref(), Some("연결 필요"));
        assert!(line.cells.iter().all(|c| c.reset.is_empty()));
    }

    #[test]
    fn mini_line_labels_only_a_second_window_of_the_same_provider() {
        let line = mini_line(vec![(color::CLAUDE, vec![window("W"), window("F")])]);
        let labels = line.cells.iter().map(|c| c.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels, [Label::Dot, Label::Plain("F".into())]);
        assert_eq!(line.cells[1].label_color, color::LABEL);
    }

    #[test]
    fn mini_line_without_providers_is_empty() {
        assert!(mini_line(Vec::new()).cells.is_empty());
        assert!(mini_line(vec![(color::CLAUDE, Vec::new())]).cells.is_empty());
    }
}
