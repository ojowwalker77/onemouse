//! Notification-area icon: shows the connection status as its tooltip and
//! offers "Open log" and "Quit". Runs its own window and message loop.

use std::ffi::OsStr;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_BYCOMMAND,
    MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SW_SHOWNORMAL, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_ENDSESSION, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use crate::client::Role;
use crate::{WindowsHost, log};

use onemouse_protocol::Main;

const CALLBACK: u32 = WM_APP + 1;
const CMD_OPEN_LOG: usize = 1;
const CMD_QUIT: usize = 2;
const CMD_MAIN_PC: usize = 3;
const CMD_MAIN_MAC: usize = 4;

static HWND_TRAY: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
static STATUS: Mutex<String> = Mutex::new(String::new());
static HOOKS: OnceLock<Hooks> = OnceLock::new();
static ROLE: Mutex<Option<Arc<Role<WindowsHost>>>> = Mutex::new(None);

struct Hooks {
    log_path: PathBuf,
    /// Releases everything held; then the process exits.
    on_quit: Box<dyn Fn() + Send + Sync>,
}

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Shows the icon. `on_quit` runs on "Quit" and at logoff/shutdown.
pub fn start(log_path: PathBuf, on_quit: impl Fn() + Send + Sync + 'static) {
    let _ = HOOKS.set(Hooks {
        log_path,
        on_quit: Box::new(on_quit),
    });
    let spawned = thread::Builder::new()
        .name("onemouse-tray".into())
        .spawn(|| {
            if let Err(e) = message_loop() {
                log!("tray icon unavailable: {e}");
            }
        });
    if let Err(e) = spawned {
        log!("can't start the tray icon: {e}");
    }
}

/// The role switch ("keyboard & mouse are on") acts on this; set once at
/// startup so the menu can offer it.
pub fn set_role(role: Arc<Role<WindowsHost>>) {
    *ROLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(role);
}

/// Updates the tooltip (at most 127 characters are shown).
pub fn set_status(status: &str) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = format!("onemouse: {status}");
    let hwnd = HWND_TRAY.load(Ordering::Acquire);
    if !hwnd.is_null() {
        notify(hwnd, NIM_MODIFY);
    }
}

fn notify(hwnd: HWND, action: u32) {
    // SAFETY: a zeroed NOTIFYICONDATAW with cbSize set is valid; strings are
    // copied into the struct.
    unsafe {
        let mut data: NOTIFYICONDATAW = mem::zeroed();
        data.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = CALLBACK;
        data.hIcon = LoadIconW(ptr::null_mut(), IDI_APPLICATION);
        let status = STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let status = if status.is_empty() {
            "onemouse".to_owned()
        } else {
            status
        };
        let tip: Vec<u16> = status.encode_utf16().take(data.szTip.len() - 1).collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(action, &data);
    }
}

fn quit(hwnd: HWND) -> ! {
    if let Some(hooks) = HOOKS.get() {
        (hooks.on_quit)();
    }
    notify(hwnd, NIM_DELETE);
    log!("quit from the tray");
    std::process::exit(0)
}

fn show_menu(hwnd: HWND) {
    // SAFETY: standard popup-menu dance; the menu is destroyed afterwards.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let status = wide(&*STATUS.lock().unwrap_or_else(|e| e.into_inner()));
        AppendMenuW(
            menu,
            MF_STRING | 0x2, /* MF_GRAYED */
            0,
            status.as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_STRING | 0x2, /* MF_DISABLED */
            0,
            wide("Keyboard and mouse are on:").as_ptr(),
        );
        let main = ROLE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|role| role.get());
        AppendMenuW(menu, MF_STRING, CMD_MAIN_PC, wide("This PC").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_MAIN_MAC, wide("This Mac").as_ptr());
        if main.is_some() {
            let check = |id: usize, on: bool| {
                CheckMenuItem(
                    menu,
                    id as u32,
                    MF_BYCOMMAND | if on { MF_CHECKED } else { MF_UNCHECKED },
                );
            };
            check(CMD_MAIN_PC, main == Some(Main::Client));
            check(CMD_MAIN_MAC, main == Some(Main::Server));
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, CMD_OPEN_LOG, wide("Open log").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Quit onemouse").as_ptr());
        let mut pos = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pos);
        // Required so the menu closes when clicking elsewhere.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pos.x,
            pos.y,
            0,
            hwnd,
            ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd {
            CMD_OPEN_LOG => {
                if let Some(hooks) = HOOKS.get() {
                    ShellExecuteW(
                        ptr::null_mut(),
                        wide("open").as_ptr(),
                        wide(&hooks.log_path).as_ptr(),
                        ptr::null(),
                        ptr::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            CMD_QUIT => quit(hwnd),
            CMD_MAIN_PC | CMD_MAIN_MAC => {
                let main = if cmd == CMD_MAIN_PC {
                    Main::Client
                } else {
                    Main::Server
                };
                if let Some(role) = ROLE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                    role.request(main);
                    log!("keyboard and mouse are on {main:?}");
                }
            }
            _ => {}
        }
    }
}

fn message_loop() -> std::io::Result<()> {
    static TASKBAR_CREATED: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            CALLBACK => {
                let event = (lp & 0xFFFF) as u32;
                if matches!(event, WM_RBUTTONUP | WM_LBUTTONUP | WM_CONTEXTMENU) {
                    show_menu(hwnd);
                }
                0
            }
            // Logoff or shutdown: never leave keys held down.
            WM_ENDSESSION if wp != 0 => {
                if let Some(hooks) = HOOKS.get() {
                    (hooks.on_quit)();
                }
                0
            }
            // Explorer restarted: the icon is gone, add it again.
            _ if msg != 0 && msg as usize == TASKBAR_CREATED.load(Ordering::Relaxed) as usize => {
                notify(hwnd, NIM_ADD);
                0
            }
            // SAFETY: forwarding the arguments we were given.
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    let class_name = wide("onemouse-tray");
    // SAFETY: standard window class registration and creation; the class name
    // buffer outlives both calls, and the window lives as long as the thread.
    unsafe {
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        TASKBAR_CREATED.store(taskbar_created as usize as *mut _, Ordering::Relaxed);
        let instance = GetModuleHandleW(ptr::null());
        let mut class: WNDCLASSW = mem::zeroed();
        class.lpfnWndProc = Some(wndproc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A hidden top-level window (not message-only) so it gets
        // WM_ENDSESSION and the TaskbarCreated broadcast.
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
        HWND_TRAY.store(hwnd, Ordering::Release);
        notify(hwnd, NIM_ADD);
        let mut msg: MSG = mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
