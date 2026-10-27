//! Where the taskbar is, how it is scaled, and whether a fullscreen app covers it.

use crate::util::wide;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{APPBARDATA, SHAppBarMessage};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetClassNameW, GetForegroundWindow, GetWindowRect};

const ABM_GETSTATE: u32 = 4;
const ABS_AUTOHIDE: usize = 1;
const DOCK_MARGIN: f32 = 6.0;

#[derive(Clone, Copy)]
pub struct Taskbar {
    pub bounds: RECT,
    /// Left edge of the notification area: the meter docks just left of it.
    pub right_limit: i32,
    pub scale: f32,
    /// Horizontal and always visible; otherwise the meter floats above the clock corner.
    pub dockable: bool,
}

fn rect_of(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    (!hwnd.is_null() && unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some(rect)
}

pub fn locate() -> Option<Taskbar> {
    let taskbar = unsafe { FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null()) };
    let bounds = rect_of(taskbar)?;
    let dpi = unsafe { GetDpiForWindow(taskbar) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };

    let mut data: APPBARDATA = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<APPBARDATA>() as u32;
    let auto_hide = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) } & ABS_AUTOHIDE != 0;
    let horizontal = bounds.right - bounds.left > bounds.bottom - bounds.top;

    let tray = unsafe { FindWindowExW(taskbar, std::ptr::null_mut(), wide("TrayNotifyWnd").as_ptr(), std::ptr::null()) };
    let right_limit = match rect_of(tray) {
        Some(r) if r.right > r.left => r.left,
        _ => bounds.right - (220.0 * scale) as i32,
    };
    Some(Taskbar { bounds, right_limit, scale, dockable: horizontal && !auto_hide })
}

/// Top-left corner for a meter of `width` x `height`.
pub fn place(taskbar: Option<&Taskbar>, width: i32, height: i32, scale: f32) -> (i32, i32) {
    if let Some(t) = taskbar.filter(|t| t.dockable) {
        let margin = (DOCK_MARGIN * scale) as i32;
        let x = (t.right_limit - margin - width).max(t.bounds.left + margin);
        let y = t.bounds.top + (t.bounds.bottom - t.bounds.top - height) / 2;
        return (x, y);
    }
    let work = monitor_info(unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }).rcWork;
    (work.right - width - (12.0 * scale) as i32, work.bottom - height - (8.0 * scale) as i32)
}

fn monitor_info(monitor: windows_sys::Win32::Graphics::Gdi::HMONITOR) -> MONITORINFO {
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<MONITORINFO>() as u32;
    unsafe { GetMonitorInfoW(monitor, &mut info) };
    info
}

/// A window covering the whole monitor the taskbar is on (games, video, slides).
pub fn fullscreen_app_active(taskbar: Option<&Taskbar>) -> bool {
    let Some(taskbar) = taskbar else { return false };
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return false;
    }
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(foreground, class.as_mut_ptr(), class.len() as i32) }.max(0) as usize;
    let class = String::from_utf16_lossy(&class[..len]);
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return false;
    }
    let Some(rect) = rect_of(foreground) else { return false };
    let screen = monitor_info(unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) }).rcMonitor;
    let b = taskbar.bounds;
    let overlaps = b.left < screen.right && screen.left < b.right && b.top < screen.bottom && screen.top < b.bottom;
    overlaps && rect.left <= screen.left && rect.top <= screen.top && rect.right >= screen.right && rect.bottom >= screen.bottom
}
