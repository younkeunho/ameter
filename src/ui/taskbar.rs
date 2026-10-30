//! Where the taskbar is and how it is scaled.

use crate::util::wide;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetWindowRect};

const DOCK_MARGIN: f32 = 6.0;

#[derive(Clone, Copy)]
pub struct Taskbar {
    pub hwnd: HWND,
    pub bounds: RECT,
    /// Left edge of the notification area: the meter docks just left of it.
    pub right_limit: i32,
    pub scale: f32,
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
    let tray = unsafe { FindWindowExW(taskbar, std::ptr::null_mut(), wide("TrayNotifyWnd").as_ptr(), std::ptr::null()) };
    let right_limit = match rect_of(tray) {
        Some(r) if r.right > r.left => r.left,
        _ => bounds.right - (220.0 * scale) as i32,
    };
    Some(Taskbar { hwnd: taskbar, bounds, right_limit, scale })
}

/// Position, in the taskbar's client coordinates, of a meter of `width` x `height` docked just
/// left of the notification area and centred vertically.
pub fn place(taskbar: &Taskbar, width: i32, height: i32) -> (i32, i32) {
    let margin = (DOCK_MARGIN * taskbar.scale) as i32;
    let b = taskbar.bounds;
    let mut at = POINT { x: (taskbar.right_limit - margin - width).max(b.left + margin), y: b.top + (b.bottom - b.top - height) / 2 };
    unsafe { ScreenToClient(taskbar.hwnd, &mut at) };
    (at.x, at.y)
}
