//! Monitor enumeration (physical pixels, virtual-desktop coordinates) and
//! `WM_DISPLAYCHANGE` notifications.

use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use onemouse_protocol::Display;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MONITORINFOF_PRIMARY, MSG,
    RegisterClassW, TranslateMessage, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_SETTINGCHANGE, WNDCLASSW,
    WS_OVERLAPPED,
};

use crate::log;

/// Makes the process Per-Monitor-V2 DPI aware, so every coordinate we see or
/// inject is in physical pixels. Call first thing in `main`.
pub fn enable_dpi_awareness() {
    // SAFETY: plain Win32 call with a predefined context value.
    if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } == 0 {
        // Fails if already set (e.g. by a manifest), which is fine as long as
        // it's V2; anything else would report scaled coordinates.
        log!(
            "SetProcessDpiAwarenessContext failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Every monitor, sorted by position. IDs are a hash of the device name
/// (`\\.\DISPLAY1`), stable across re-enumeration.
pub fn displays() -> Vec<Display> {
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows_sys::core::BOOL {
        // SAFETY: `data` is the `&mut Vec<Display>` passed below, alive for
        // the duration of EnumDisplayMonitors.
        let out = unsafe { &mut *(data as *mut Vec<Display>) };
        if let Some(display) = describe(monitor) {
            out.push(display);
        }
        1
    }

    let mut out: Vec<Display> = Vec::new();
    // SAFETY: the callback only touches `out` through `data`.
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(callback),
            &mut out as *mut Vec<Display> as LPARAM,
        );
    }
    out.sort_by_key(|d| (d.x, d.y, d.id));
    out
}

fn describe(monitor: HMONITOR) -> Option<Display> {
    // SAFETY: zeroed MONITORINFOEXW with cbSize set is the documented input.
    let mut info: MONITORINFOEXW = unsafe { mem::zeroed() };
    info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) } == 0 {
        return None;
    }
    let rect = info.monitorInfo.rcMonitor;

    let (mut dpi_x, mut dpi_y) = (96, 96);
    // SAFETY: out-pointers to locals.
    let hr = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let scale = if hr >= 0 { dpi_x as f32 / 96.0 } else { 1.0 };

    let name_len = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    Some(Display {
        id: fnv1a(&info.szDevice[..name_len]),
        x: rect.left,
        y: rect.top,
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
        scale,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

fn fnv1a(name: &[u16]) -> u32 {
    name.iter().fold(0x811c_9dc5, |hash, &c| {
        (hash ^ c as u32).wrapping_mul(0x0100_0193)
    })
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Bumped on every display-related window message. See [`watch`].
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Starts a thread with a hidden top-level window that bumps [`generation`]
/// on `WM_DISPLAYCHANGE` (resolution, arrangement, monitor plugged in or out)
/// and on DPI/settings changes. Message-only windows don't get those
/// broadcasts, hence a real (never shown) window.
pub fn watch() {
    let spawned = thread::Builder::new()
        .name("onemouse-displays".into())
        .spawn(|| {
            if let Err(e) = message_loop() {
                log!("display watcher stopped: {e}; falling back to polling");
            }
        });
    if let Err(e) = spawned {
        log!("can't start display watcher: {e}; falling back to polling");
    }
}

fn message_loop() -> std::io::Result<()> {
    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if matches!(msg, WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE) {
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarding the arguments we were given.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    let class_name: Vec<u16> = "onemouse-display-watcher\0".encode_utf16().collect();
    // SAFETY: standard window class registration and creation; the class name
    // buffer outlives both calls, and the window lives as long as the thread.
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let mut class: WNDCLASSW = mem::zeroed();
        class.lpfnWndProc = Some(wndproc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut msg: MSG = mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_at_least_one_display() {
        enable_dpi_awareness();
        let displays = displays();
        // CI runners have a (virtual) display; a service session might not.
        if displays.is_empty() {
            return;
        }
        assert_eq!(displays.iter().filter(|d| d.primary).count(), 1);
        for d in &displays {
            assert!(d.width > 0 && d.height > 0, "{d:?}");
            assert!(d.scale >= 1.0, "{d:?}");
        }
        let primary = displays.iter().find(|d| d.primary).unwrap();
        assert_eq!((primary.x, primary.y), (0, 0));
    }

    #[test]
    fn ids_are_stable_hashes() {
        let a: Vec<u16> = r"\\.\DISPLAY1".encode_utf16().collect();
        let b: Vec<u16> = r"\\.\DISPLAY2".encode_utf16().collect();
        assert_eq!(fnv1a(&a), fnv1a(&a));
        assert_ne!(fnv1a(&a), fnv1a(&b));
    }
}
