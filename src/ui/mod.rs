//! ameter.exe: the taskbar meter. Top line Codex (CO) and Higgsfield credits (HF), bottom line
//! Claude (CC). Right-click switches items on and off; double-click refreshes.

mod bar;
mod gdip;
mod taskbar;

use crate::claude_setup;
use crate::claude_web::{ClaudeWeb, WebStatus};
use crate::codex::Codex;
use crate::fmt;
use crate::higgsfield::Higgsfield;
use crate::model::{self, FIVE_HOUR_MINS, ProviderState, WEEK_MINS};
use crate::paths;
use crate::poller::Poller;
use crate::util::{self, JsonExt, wide};
use bar::{Cell, Fonts, Layout, Row, color};
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
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
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
const MENU_CLAUDE_FIVE_HOUR: usize = 4;
const MENU_HIGGSFIELD: usize = 5;
const MENU_EXIT: usize = 9;

/// What the bar shows; toggled from the menu and kept in display.json.
#[derive(Clone, Copy)]
struct Display {
    codex: bool,
    claude_weekly: bool,
    claude_five_hour: bool,
    higgsfield: bool,
}

impl Display {
    fn claude(&self) -> bool {
        self.claude_weekly || self.claude_five_hour
    }

    fn load() -> Display {
        let json = util::read_json_object(&paths::display_settings()).unwrap_or_default();
        let get = |key: &str| json.bool_of(key).unwrap_or(true);
        Display {
            codex: get("codex"),
            claude_weekly: get("claudeWeekly"),
            claude_five_hour: get("claudeFiveHour"),
            higgsfield: get("higgsfield"),
        }
    }

    fn save(&self) {
        let json = json!({
            "codex": self.codex,
            "claudeWeekly": self.claude_weekly,
            "claudeFiveHour": self.claude_five_hour,
            "higgsfield": self.higgsfield,
        });
        if let Err(e) = util::write_json(&paths::display_settings(), &json) {
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
    drawn_at: Option<(i32, i32)>,
    hidden: bool,
    menu_open: bool,
    tray_icon: HICON,
    tray_tip: String,
    taskbar_created: u32,
    hook: HWINEVENTHOOK,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

static HWND_VALUE: AtomicIsize = AtomicIsize::new(0);

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

pub fn run() {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
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
    let Some(hwnd) = create_window() else {
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
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
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
        menu_open: false,
        tray_icon: make_icon(),
        tray_tip: String::new(),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        hook,
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

fn create_window() -> Option<HWND> {
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = wide("ameter.meter");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return None;
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class.as_ptr(),
            wide("ameter").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    (!hwnd.is_null()).then_some(hwnd)
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    with_app(|app| app.sync(false));
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_TIMER if wparam == TIMER_TICK => {
            with_app(App::tick);
            return 0;
        }
        WM_APP_UPDATE => {
            with_app(App::on_update);
            return 0;
        }
        WM_RBUTTONUP => {
            show_menu(hwnd);
            return 0;
        }
        WM_LBUTTONDBLCLK => {
            with_app(App::refresh_all);
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

fn show_menu(hwnd: HWND) {
    let Some(display) = with_app(|app| {
        app.menu_open = true;
        app.display
    }) else {
        return;
    };
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let command = unsafe {
        let menu = CreatePopupMenu();
        let claude = CreatePopupMenu();
        AppendMenuW(claude, MF_STRING | check(display.claude_weekly), MENU_CLAUDE_WEEKLY, wide("주간").as_ptr());
        AppendMenuW(claude, MF_STRING | check(display.claude_five_hour), MENU_CLAUDE_FIVE_HOUR, wide("5시간").as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_REFRESH, wide("새로고침").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING | check(display.codex), MENU_CODEX, wide("코덱스").as_ptr());
        AppendMenuW(menu, MF_POPUP | check(display.claude()), claude as usize, wide("클로드").as_ptr());
        AppendMenuW(menu, MF_STRING | check(display.higgsfield), MENU_HIGGSFIELD, wide("힉스필드").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, wide("종료").as_ptr());

        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        // The meter never activates; take the foreground so the menu closes on an outside click.
        SetForegroundWindow(hwnd);
        let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_RIGHTALIGN | TPM_BOTTOMALIGN;
        let command = TrackPopupMenu(menu, flags, cursor.x, cursor.y, 0, hwnd, std::ptr::null()) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu); // destroys the submenu too
        command
    };
    with_app(|app| {
        app.menu_open = false;
        app.on_command(command);
    });
}

fn make_icon() -> HICON {
    let Some(canvas) = Canvas::owned(32, 32) else { return std::ptr::null_mut() };
    let tile = windows_sys::Win32::Graphics::GdiPlus::RectF { X: 1.0, Y: 1.0, Width: 30.0, Height: 30.0 };
    canvas.rounded_rect(tile, 7.0, 0xFF_26_26_26, 0);
    canvas.fill_rect(6.0, 8.0, 20.0, 6.0, color::CODEX);
    canvas.fill_rect(6.0, 18.0, 13.0, 6.0, color::CLAUDE);
    canvas.to_icon()
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
                unsafe { DestroyWindow(self.hwnd) };
                return;
            }
            MENU_CODEX => self.display.codex = !self.display.codex,
            MENU_CLAUDE_WEEKLY => self.display.claude_weekly = !self.display.claude_weekly,
            MENU_CLAUDE_FIVE_HOUR => self.display.claude_five_hour = !self.display.claude_five_hour,
            MENU_HIGGSFIELD => self.display.higgsfield = !self.display.higgsfield,
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
        if !self.hook.is_null() {
            unsafe { UnhookWinEvent(self.hook) };
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

    /// Claude as shown: the cache, with a status explaining whichever source is missing.
    fn claude_view(&self, web: Option<&WebStatus>) -> ProviderState {
        let mut state = self.claude.clone();
        if state.windows.is_empty() {
            let web_source = web.is_none_or(|w| w.available);
            state.status = Some(match web {
                Some(w) if w.available && !w.ok => w.status.clone().unwrap_or_else(|| "연결 실패".into()),
                _ if web_source => "확인 중…".into(),
                _ if self.claude_installed => "대기 중".into(),
                _ => "연결 필요".into(),
            });
        }
        state
    }

    fn build_rows(&self) -> (Vec<Row>, String) {
        let now_ms = util::now_ms();
        let now_sec = now_ms / 1000;
        let shared = self.shared.lock().map(|s| (s.codex.clone(), s.higgsfield.clone(), s.web.clone()));
        let (codex, higgsfield, web) = shared.unwrap_or_default();
        let claude = self.claude_view(web.as_ref());
        let claude_stale = if web.as_ref().is_some_and(|w| w.available) { CLAUDE_WEB_STALE_MS } else { CLAUDE_STATUSLINE_STALE_MS };

        let d = self.display;
        let mut rows = Vec::new();
        let mut top = Row { name: d.codex.then(|| "CO".into()), name_color: color::CODEX, cells: Vec::new() };
        if d.codex {
            add_windows(&mut top, &codex, now_sec, now_ms, CODEX_STALE_MS, true, true);
        }
        if d.higgsfield {
            top.cells.push(credits_cell(&higgsfield, now_sec, now_ms));
        }
        if !top.cells.is_empty() {
            rows.push(top);
        }
        if d.claude() {
            let mut bottom = Row { name: Some("CC".into()), name_color: color::CLAUDE, cells: Vec::new() };
            add_windows(&mut bottom, &claude, now_sec, now_ms, claude_stale, d.claude_five_hour, d.claude_weekly);
            rows.push(bottom);
        }

        let tip = [
            fmt::terminal_line("CO", &codex, now_sec, false),
            fmt::terminal_line("HF", &higgsfield, now_sec, false),
            fmt::terminal_line("CC", &claude, now_sec, false),
        ]
        .join("\n");
        (rows, tip)
    }

    /// Placement, fullscreen hiding, redraw on change, and the topmost z-order.
    fn sync(&mut self, force: bool) {
        let (rows, tip) = self.build_rows();
        if tip != self.tray_tip {
            self.tray_tip = tip;
            self.tray(NIM_MODIFY);
        }

        let taskbar = taskbar::locate();
        // Hidden while everything is switched off, or under a fullscreen app.
        let hidden = rows.is_empty() || taskbar::fullscreen_app_active(taskbar.as_ref());
        if hidden != self.hidden {
            self.hidden = hidden;
            unsafe { ShowWindow(self.hwnd, if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE }) };
        }
        if hidden {
            self.rows = rows;
            return;
        }

        let scale = taskbar.as_ref().map(|t| t.scale).unwrap_or(1.0);
        let mut redraw = force;
        if (scale - self.fonts.scale).abs() > f32::EPSILON {
            self.fonts = Fonts::new(scale);
            redraw = true;
        }
        if redraw || rows != self.rows || self.drawn_at.is_none() {
            self.layout = bar::measure(&rows, &self.measuring, &self.fonts);
            self.rows = rows;
            redraw = true;
        }
        let at = taskbar::place(taskbar.as_ref(), self.layout.width, self.layout.height, scale);
        if redraw || self.drawn_at != Some(at) {
            self.present(at);
            self.drawn_at = Some(at);
        }
        if !self.menu_open {
            unsafe {
                SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
            }
        }
    }

    /// Paints premultiplied ARGB straight into a DIB section and hands it to UpdateLayeredWindow.
    fn present(&self, (x, y): (i32, i32)) {
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
                let destination = POINT { x, y };
                let source = POINT { x: 0, y: 0 };
                let size = SIZE { cx: width, cy: height };
                let blend =
                    BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
                UpdateLayeredWindow(self.hwnd, screen, &destination, &size, memory, &source, 0, &blend, ULW_ALPHA);
                SelectObject(memory, previous);
                DeleteObject(dib);
            }
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
    }
}

/// "W 94% 6d0h" per window, or one status cell while there are no values.
fn add_windows(row: &mut Row, state: &ProviderState, now_sec: i64, now_ms: i64, stale_ms: i64, five_hour: bool, weekly: bool) {
    if state.windows.is_empty() {
        row.cells.push(Cell::status(state.status.as_deref().unwrap_or("—")));
        return;
    }
    let dim = state.failing || state.updated_at_ms == 0 || now_ms - state.updated_at_ms > stale_ms;
    for w in fmt::ordered(&state.windows) {
        if (w.duration_mins == Some(FIVE_HOUR_MINS) && !five_hour) || (w.duration_mins == Some(WEEK_MINS) && !weekly) {
            continue;
        }
        let mut cell = Cell { label: fmt::label(w.duration_mins), ..Default::default() };
        match w.used_percent.filter(|_| !fmt::expired(w, now_sec)) {
            None => {
                cell.value = "—".into();
                cell.value_color = color::MUTED;
            }
            Some(used) => {
                cell.value = fmt::left(used);
                cell.reset = w.resets_at.map(|r| fmt::short(r - now_sec)).unwrap_or_default();
                cell.value_color = color::for_severity(fmt::severity(used));
            }
        }
        if dim {
            cell.value_color = color::dimmed(cell.value_color);
        }
        row.cells.push(cell);
    }
}

/// "HF 7,747 11d22h": remaining credits and the time until the monthly refill.
fn credits_cell(state: &ProviderState, now_sec: i64, now_ms: i64) -> Cell {
    let mut cell = Cell { label: "HF".into(), label_color: color::HIGGSFIELD, label_bold: true, ..Default::default() };
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
