//! Start at Windows sign-in through the per-user Run key.

use crate::util::wide;
use std::path::Path;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE: &str = "ameter";

pub fn is_enabled() -> bool {
    let mut key: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_QUERY_VALUE, &mut key) } != ERROR_SUCCESS {
        return false;
    }
    let found = unsafe {
        RegQueryValueExW(key, wide(VALUE).as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    } == ERROR_SUCCESS;
    unsafe { RegCloseKey(key) };
    found
}

pub fn set(enabled: bool, widget: &Path) -> Result<(), String> {
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("레지스트리 열기 실패 ({status})"));
    }
    let status = if enabled {
        let data = wide(&format!("\"{}\"", widget.display()));
        unsafe { RegSetValueExW(key, wide(VALUE).as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32) }
    } else {
        match unsafe { RegDeleteValueW(key, wide(VALUE).as_ptr()) } {
            windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            other => other,
        }
    };
    unsafe { RegCloseKey(key) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(format!("레지스트리 쓰기 실패 ({status})")) }
}
