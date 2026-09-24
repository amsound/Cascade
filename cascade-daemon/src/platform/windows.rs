//! Windows desktop integration: a hidden top-level window that carries both the session
//! shutdown handshake and the tray icon.
//!
//! ONE WINDOW, TWO JOBS, and the first is why the second is cheap.
//!
//! Session shutdown: `WM_QUERYENDSESSION` and `WM_ENDSESSION` are broadcast to every
//! top-level window of a process, shown or not. Without a window Cascade never hears a log
//! off, restart or shutdown, and is terminated with whatever was still inside the config
//! save debounce. The window must be genuinely top-level — a message-only (`HWND_MESSAGE`)
//! window is excluded from that broadcast, which is the trap in this design.
//!
//! Tray icon: `Shell_NotifyIconW` needs a window to deliver its callback message to, and
//! this one is already pumping messages.
//!
//! WHAT THIS STILL CANNOT CATCH: Task Manager's End task calls `TerminateProcess`, which no
//! handler intercepts. That is Windows' force-quit and it is uncatchable by design. The
//! tray's Exit item is the graceful path a person should use.

use std::sync::atomic::{AtomicU16, AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, MessageBoxW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu,
    TranslateMessage, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST,
    MB_YESNO, MF_SEPARATOR, MF_STRING, MSG, SW_SHOWNORMAL, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_LBUTTONUP,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

/// Tray callback. `WM_APP` and above are free for application use.
const WM_TRAY: u32 = WM_APP + 1;
/// Menu command ids.
const ID_OPEN: usize = 1;
const ID_EXIT: usize = 2;
/// The tray icon's id within this window. Any constant will do; it only has to be stable
/// between the NIM_ADD and the NIM_DELETE.
const TRAY_UID: u32 = 1;
/// Resource id of the application icon, as linked by build.rs.
const ICON_RESOURCE: u16 = 1;

/// The web UI's port, so the menu can open the right URL rather than assuming 8080.
static API_PORT: AtomicU16 = AtomicU16::new(0);
/// The pump's window, so `stop()` can post to it from another thread. isize because HWND is
/// not itself atomic-friendly; 0 means "not created yet".
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
/// "TaskbarCreated", registered at startup: Explorer broadcasts it when it restarts, and an
/// app that ignores it silently loses its tray icon for the rest of the session.
static TASKBAR_CREATED: AtomicU16 = AtomicU16::new(0);
/// Set by `stop()` — which the shutdown path calls once the config is flushed and exclusive
/// access released — so a session-end handler can wait for that work before returning.
static SHUTDOWN_DONE: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
static SHUTDOWN_DONE_CV: std::sync::Condvar = std::sync::Condvar::new();
/// How long a session-end handler waits for the shutdown work. Windows starts offering to
/// end an application that takes about five seconds to answer, so this stays inside that.
const SESSION_END_WAIT: std::time::Duration = std::time::Duration::from_secs(4);

/// Start the window and tray on a dedicated thread. Returns once the thread is running;
/// failures are logged and leave the daemon running headless rather than stopping it.
pub fn start(api_port: u16) {
    API_PORT.store(api_port, Ordering::Relaxed);
    if let Err(e) = std::thread::Builder::new()
        .name("cascade-tray".into())
        .spawn(pump)
    {
        tracing::warn!("tray: could not start the message pump ({e}) — running without a \
                        tray icon, and the session-shutdown handshake is not installed");
    }
}

/// Ask the pump to close down. Best-effort: if the window never came up there is nothing to
/// tell, and the thread is not joined — the process is on its way out either way.
///
/// Called at the end of the shutdown work, so it also releases a session-end handler waiting
/// on that work (see `WM_ENDSESSION`).
pub fn stop() {
    *SHUTDOWN_DONE.lock().unwrap_or_else(|e| e.into_inner()) = true;
    SHUTDOWN_DONE_CV.notify_all();
    let raw = HWND_RAW.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: HWND_RAW is only ever set to a window this module created, and cleared in
        // WM_DESTROY. PostMessageW to a stale HWND fails harmlessly rather than misbehaving.
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn pump() {
    // SAFETY: a conventional Win32 window-class registration, window creation and message
    // loop. Every pointer handed to the API is either null, a 'static wide string literal,
    // or a stack value that outlives the call.
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            tracing::warn!("tray: GetModuleHandleW failed — no tray icon");
            return;
        };
        let class = w!("CascadeTrayWindow");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            tracing::warn!("tray: RegisterClassW failed — no tray icon");
            return;
        }

        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")) as u16, Ordering::Relaxed);

        // Top-level (no parent) so the session-end broadcast reaches it, WS_EX_TOOLWINDOW so
        // it stays out of Alt-Tab and the taskbar, and never shown — no ShowWindow call.
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("Cascade"),
            WS_OVERLAPPED,
            0, 0, 0, 0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("tray: CreateWindowExW failed ({e}) — no tray icon");
                return;
            }
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);

        add_icon(hwnd);
        tracing::info!("Tray icon active; session shutdown handshake installed");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Add (or re-add) the notification-area icon.
unsafe fn add_icon(hwnd: HWND) {
    let hicon = GetModuleHandleW(None)
        .ok()
        .and_then(|h| LoadIconW(Some(h.into()), PCWSTR(ICON_RESOURCE as *const u16)).ok());

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_MESSAGE | NIF_TIP | if hicon.is_some() { NIF_ICON } else { Default::default() },
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    if let Some(i) = hicon {
        nid.hIcon = i;
    }
    // szTip is a fixed 128-unit buffer; the text is far shorter, and the struct starts
    // zeroed, so it stays NUL-terminated.
    for (i, c) in "Cascade".encode_utf16().enumerate() {
        nid.szTip[i] = c;
    }
    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        tracing::warn!("tray: Shell_NotifyIcon(NIM_ADD) failed — no tray icon (the session \
                        shutdown handshake is still installed)");
    }
}

unsafe fn remove_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// Right-click menu: Open config, Exit.
unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Open config"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit Cascade"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required before TrackPopupMenu on a tray menu: without it the menu does not dismiss
    // when the user clicks elsewhere, and is left on screen.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
    let _ = DestroyMenu(menu);
}

/// Open the web UI. Loopback explicitly: `bind` may be 0.0.0.0, which is not an address to
/// browse to, and the port comes from config rather than being assumed.
unsafe fn open_config(hwnd: HWND) {
    let port = API_PORT.load(Ordering::Relaxed);
    let url: Vec<u16> = format!("http://127.0.0.1:{port}/\0").encode_utf16().collect();
    ShellExecuteW(
        Some(hwnd),
        w!("open"),
        PCWSTR(url.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Confirm before stopping. A tray Exit is one click away from a live broadcast, so it asks
/// once, says what is actually lost, and defaults to No.
unsafe fn confirm_exit(hwnd: HWND) -> bool {
    let _ = SetForegroundWindow(hwnd);
    MessageBoxW(
        Some(hwnd),
        w!("Quit Cascade?\n\nAll audio being sent to and received from your remotes will stop."),
        w!("Cascade"),
        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST,
    ) == IDYES
}

/// Show text in a dialog. The daemon links as a GUI-subsystem binary and therefore has NO
/// stdout — `--selftest` printing to it goes nowhere, which is exactly what happens if you
/// run it from a Windows terminal and see nothing at all. This is the visible channel.
pub fn show_report(title: &str, body: &str) {
    let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let b: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(None, PCWSTR(b.as_ptr()), PCWSTR(t.as_ptr()),
                    MB_SETFOREGROUND | MB_TOPMOST);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed) as u32;
    if taskbar_created != 0 && msg == taskbar_created {
        add_icon(hwnd);
        return LRESULT(0);
    }

    match msg {
        // Windows is ASKING whether the session may end. Answer yes, and do nothing else:
        // another application can still refuse, in which case the session carries on — and
        // so must the audio.
        WM_QUERYENDSESSION => LRESULT(1),
        // The outcome. wParam TRUE: the session really is ending, and the process may be
        // terminated at any moment once this returns — so start the clean shutdown and do
        // not return until it has flushed the config and released exclusive access, or
        // SESSION_END_WAIT has passed. wParam FALSE: the end was cancelled; nothing to do.
        WM_ENDSESSION => {
            if wp.0 != 0 {
                crate::lifecycle::request(crate::lifecycle::Exit::Quit);
                let done = SHUTDOWN_DONE.lock().unwrap_or_else(|e| e.into_inner());
                let _ = SHUTDOWN_DONE_CV.wait_timeout_while(done, SESSION_END_WAIT, |d| !*d);
            }
            LRESULT(0)
        }
        // Posted by stop() when the daemon is going down on its own, and sent by anything
        // that asks this window to close. No confirmation here: this is not the tray item,
        // it is the process already on its way out.
        WM_CLOSE => {
            remove_icon(hwnd);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            HWND_RAW.store(0, Ordering::Relaxed);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_TRAY => {
            match lp.0 as u32 {
                WM_RBUTTONUP | WM_LBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wp.0 & 0xFFFF) as usize {
                ID_OPEN => open_config(hwnd),
                ID_EXIT => {
                    if confirm_exit(hwnd) {
                        crate::lifecycle::request(crate::lifecycle::Exit::Quit);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
