//! The remote desktop window: draws the remote screen, forwards input, and
//! reconnects by itself when the computer restarts or the network drops.

use crate::framebuffer::Framebuffer;
use crate::geometry::{fit_rect, to_remote};
use crate::net::{self, FingerprintChanged, NotAuthorized};
use crate::store;
use anyhow::Result;
use rd_common::clipboard;
use rd_common::proto::{read_msg, write_msg, CursorShape, MonitorInfo, MouseButton, ToAgent, ToViewer};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_NUMLOCK, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_REFRESH_UI: u32 = WM_APP + 1;
const WM_APP_TOGGLE_FULLSCREEN: u32 = WM_APP + 2;

const ID_CAD: usize = 100;
const ID_FULLSCREEN: usize = 101;
const ID_REFRESH: usize = 102;
const ID_CLIPBOARD: usize = 103;
const ID_QUALITY_BASE: usize = 110;
const ID_MONITOR_BASE: usize = 200;
const QUALITIES: [(&str, u8); 3] = [("Fast (lower quality)", 50), ("Balanced", 75), ("Sharp (more bandwidth)", 90)];
const SCAN_CTRL: u16 = 0x1D;
const SCAN_ALT: u16 = 0x38;

struct UiState {
    status: String,
    connected: bool,
    monitors: Vec<MonitorInfo>,
    current_monitor: u32,
    cursor: CursorShape,
    quality: u8,
    /// Text received from the remote computer, so it isn't sent straight back.
    clipboard_echo: Option<String>,
    keys_down: HashSet<(u16, bool)>,
    fullscreen_restore: Option<(WINDOWPLACEMENT, i32)>,
}

struct App {
    name: String,
    framebuffer: Mutex<Framebuffer>,
    ui: Mutex<UiState>,
    tx: UnboundedSender<ToAgent>,
    hwnd: AtomicIsize,
    clipboard_enabled: AtomicBool,
}

static APP: OnceLock<App> = OnceLock::new();

fn app() -> &'static App {
    APP.get().expect("initialised before the window is created")
}

fn send(msg: ToAgent) {
    let _ = app().tx.send(msg);
}

fn hwnd() -> HWND {
    HWND(app().hwnd.load(Ordering::SeqCst) as *mut _)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn request_repaint() {
    let _ = unsafe { InvalidateRect(Some(hwnd()), None, false) };
}

fn request_ui_refresh() {
    let _ = unsafe { PostMessageW(Some(hwnd()), WM_APP_REFRESH_UI, WPARAM(0), LPARAM(0)) };
}

pub fn run(addr: String, fingerprint: String, name: String) -> Result<()> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let key = store::load_or_create_key()?;
    let (tx, rx) = unbounded_channel();
    let _ = APP.set(App {
        name,
        framebuffer: Mutex::new(Framebuffer::default()),
        ui: Mutex::new(UiState {
            status: "Connecting…".to_string(),
            connected: false,
            monitors: Vec::new(),
            current_monitor: 0,
            cursor: CursorShape::Arrow,
            quality: 75,
            clipboard_echo: None,
            keys_down: HashSet::new(),
            fullscreen_restore: None,
        }),
        tx,
        hwnd: AtomicIsize::new(0),
        clipboard_enabled: AtomicBool::new(true),
    });

    unsafe { create_window()? };

    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Runtime::new() else { return };
        runtime.block_on(network_loop(addr, fingerprint, key, rx));
    });
    std::thread::spawn(clipboard_loop);

    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)?;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
    Ok(())
}

// ------------------------------------------------------------------ network

async fn network_loop(addr: String, fingerprint: String, key: [u8; 32], mut rx: UnboundedReceiver<ToAgent>) {
    loop {
        let reason = match net::connect(&addr, Some(&fingerprint), &key).await {
            Ok(stream) => {
                set_status(true, "");
                match run_connection(stream, &mut rx).await {
                    Ok(()) => "The connection closed.".to_string(),
                    Err(e) => format!("Connection lost: {e}"),
                }
            }
            // Retrying can't fix these, and the identity failure should not be retried.
            Err(e) if e.is::<FingerprintChanged>() || e.is::<NotAuthorized>() => {
                set_status(false, &format!("Not connected: {e}"));
                return;
            }
            Err(e) => format!("Can't reach the computer: {e}"),
        };
        set_status(false, &format!("{reason}\nReconnecting…"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Input typed while disconnected must not run on the machine once it's back.
        while rx.try_recv().is_ok() {}
    }
}

fn set_status(connected: bool, status: &str) {
    {
        let mut ui = app().ui.lock().unwrap();
        ui.connected = connected;
        ui.status = status.to_string();
    }
    request_repaint();
    request_ui_refresh();
}

async fn run_connection<S>(stream: S, rx: &mut UnboundedReceiver<ToAgent>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // The agent starts each connection with defaults, so restore the user's choices.
    let (quality, monitor) = {
        let ui = app().ui.lock().unwrap();
        (ui.quality, ui.current_monitor)
    };
    write_msg(&mut writer, &ToAgent::SetQuality(quality)).await?;
    write_msg(&mut writer, &ToAgent::SelectMonitor(monitor)).await?;

    let write_side = async {
        while let Some(msg) = rx.recv().await {
            write_msg(&mut writer, &msg).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let read_side = async {
        loop {
            match read_msg::<_, ToViewer>(&mut reader).await? {
                ToViewer::Frame { seq, width, height, tiles } => {
                    // Off the async threads: a whole 4K frame takes a noticeable time to decode.
                    let (decoded, tiles) = tokio::task::spawn_blocking(move || {
                        let decoded = Framebuffer::decode_tiles(&tiles);
                        (decoded, tiles)
                    })
                    .await?;
                    let decoded = decoded?;
                    app().framebuffer.lock().unwrap().apply(width, height, &tiles, &decoded)?;
                    request_repaint();
                    send(ToAgent::Ack { seq });
                }
                ToViewer::Monitors { monitors, current } => {
                    let mut ui = app().ui.lock().unwrap();
                    // Keeps the user's selection if it's still valid after a reconnect.
                    if ui.current_monitor as usize >= monitors.len() || ui.monitors.is_empty() {
                        ui.current_monitor = current;
                    }
                    ui.monitors = monitors;
                    drop(ui);
                    request_ui_refresh();
                }
                ToViewer::Cursor(shape) => app().ui.lock().unwrap().cursor = shape,
                ToViewer::Clipboard(text) => {
                    if app().clipboard_enabled.load(Ordering::SeqCst) {
                        app().ui.lock().unwrap().clipboard_echo = Some(text.clone());
                        clipboard::set_text(&text);
                    }
                }
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = write_side => r,
        r = read_side => r,
    }
}

fn clipboard_loop() {
    let mut seen = clipboard::sequence_number();
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let now = clipboard::sequence_number();
        if now == seen {
            continue;
        }
        seen = now;
        if !app().clipboard_enabled.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(text) = clipboard::get_text() {
            let echo = app().ui.lock().unwrap().clipboard_echo.take();
            if echo.as_deref() != Some(text.as_str()) {
                send(ToAgent::Clipboard(text));
            }
        }
    }
}

// ------------------------------------------------------------------ window

unsafe fn create_window() -> Result<()> {
    let module = GetModuleHandleW(None)?;
    let instance = HINSTANCE(module.0);
    let class = w!("RemoteAccessSession");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    RegisterClassW(&wc);

    let title = wide(&format!("{} - Remote Access", app().name));
    let menu = build_menu()?;
    MENU.store(menu.0 as isize, Ordering::SeqCst);
    let window = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        1280,
        800,
        None,
        Some(menu),
        Some(instance),
        None,
    )?;
    app().hwnd.store(window.0 as isize, Ordering::SeqCst);
    Ok(())
}

unsafe fn build_menu() -> Result<HMENU> {
    let bar = CreateMenu()?;
    let session = CreatePopupMenu()?;
    AppendMenuW(session, MF_STRING, ID_CAD, w!("Send Ctrl+Alt+Del"))?;
    AppendMenuW(session, MF_STRING, ID_REFRESH, w!("Refresh screen"))?;
    AppendMenuW(session, MF_STRING, ID_FULLSCREEN, w!("Full screen\tCtrl+Alt+Enter"))?;
    AppendMenuW(session, MF_STRING | MF_CHECKED, ID_CLIPBOARD, w!("Share clipboard"))?;
    AppendMenuW(bar, MF_POPUP, session.0 as usize, w!("Session"))?;

    let monitors = CreatePopupMenu()?;
    AppendMenuW(bar, MF_POPUP, monitors.0 as usize, w!("Monitor"))?;

    let quality = CreatePopupMenu()?;
    for (i, (label, value)) in QUALITIES.iter().enumerate() {
        let label = wide(label);
        let flags = if *value == 75 { MF_STRING | MF_CHECKED } else { MF_STRING };
        AppendMenuW(quality, flags, ID_QUALITY_BASE + i, PCWSTR(label.as_ptr()))?;
    }
    AppendMenuW(bar, MF_POPUP, quality.0 as usize, w!("Quality"))?;
    Ok(bar)
}

unsafe fn refresh_ui(window: HWND) {
    let (monitors, current, connected, status) = {
        let ui = app().ui.lock().unwrap();
        (ui.monitors.clone(), ui.current_monitor, ui.connected, ui.status.clone())
    };
    let state = if connected {
        "Connected"
    } else if status.contains("Reconnecting") {
        "Reconnecting"
    } else {
        "Not connected"
    };
    let title = wide(&format!("{} - {state} - Remote Access", app().name));
    let _ = SetWindowTextW(window, PCWSTR(title.as_ptr()));

    let submenu = GetSubMenu(stored_menu(), 1);
    if submenu.is_invalid() {
        return;
    }
    while GetMenuItemCount(Some(submenu)) > 0 {
        let _ = DeleteMenu(submenu, 0, MF_BYPOSITION);
    }
    for (i, m) in monitors.iter().enumerate() {
        let primary = if m.primary { ", primary" } else { "" };
        let label = wide(&format!("Display {} ({}x{}{primary})", i + 1, m.width, m.height));
        let flags = if i as u32 == current { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(submenu, flags, ID_MONITOR_BASE + i, PCWSTR(label.as_ptr()));
    }
    let _ = DrawMenuBar(window);
}

/// The menu bar is removed from the window in full screen, so its handle is kept here.
static MENU: AtomicIsize = AtomicIsize::new(0);

fn stored_menu() -> HMENU {
    HMENU(MENU.load(Ordering::SeqCst) as *mut _)
}

unsafe fn toggle_fullscreen(window: HWND) {
    let mut ui = app().ui.lock().unwrap();
    let menu = stored_menu();
    if let Some((placement, style)) = ui.fullscreen_restore.take() {
        SetWindowLongW(window, GWL_STYLE, style);
        let _ = SetMenu(window, Some(menu));
        let _ = SetWindowPlacement(window, &placement);
        let _ = SetWindowPos(
            window,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    } else {
        let mut placement = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(window, &mut placement);
        let style = GetWindowLongW(window, GWL_STYLE);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut info);
        ui.fullscreen_restore = Some((placement, style));
        SetWindowLongW(window, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW.0 as i32));
        let _ = SetMenu(window, None);
        let r = info.rcMonitor;
        let _ = SetWindowPos(
            window,
            Some(HWND_TOP),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_FRAMECHANGED,
        );
    }
}

fn client_size(window: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    let _ = unsafe { GetClientRect(window, &mut r) };
    (r.right, r.bottom)
}

fn image_rect(window: HWND) -> (i32, i32, i32, i32) {
    let (cw, ch) = client_size(window);
    let fb = app().framebuffer.lock().unwrap();
    fit_rect(cw, ch, fb.width as i32, fb.height as i32)
}

unsafe fn paint(window: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(window, &mut ps);
    let (cw, ch) = client_size(window);
    let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);

    let (x, y, w, h) = {
        let fb = app().framebuffer.lock().unwrap();
        let rect = fit_rect(cw, ch, fb.width as i32, fb.height as i32);
        let (x, y, w, h) = rect;
        if w > 0 && !fb.pixels.is_empty() {
            let mut info = BITMAPINFO::default();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: fb.width as i32,
                biHeight: -(fb.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            SetStretchBltMode(dc, HALFTONE);
            let _ = SetBrushOrgEx(dc, 0, 0, None);
            StretchDIBits(
                dc,
                x,
                y,
                w,
                h,
                0,
                0,
                fb.width as i32,
                fb.height as i32,
                Some(fb.pixels.as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
        rect
    };

    // Black bars around the image.
    for bar in [
        RECT { left: 0, top: 0, right: cw, bottom: y },
        RECT { left: 0, top: y + h, right: cw, bottom: ch },
        RECT { left: 0, top: y, right: x, bottom: y + h },
        RECT { left: x + w, top: y, right: cw, bottom: y + h },
    ] {
        FillRect(dc, &bar, black);
    }

    let (connected, status) = {
        let ui = app().ui.lock().unwrap();
        (ui.connected, ui.status.clone())
    };
    if !connected {
        let mut text: Vec<u16> = status.encode_utf16().collect();
        let mut box_rect = RECT { left: 24, top: 24, right: cw - 24, bottom: 180 };
        FillRect(dc, &box_rect, black);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
        DrawTextW(dc, &mut text, &mut box_rect, DT_LEFT | DT_WORDBREAK);
    }
    let _ = EndPaint(window, &ps);
}

unsafe fn handle_command(window: HWND, id: usize) {
    match id {
        ID_CAD => send(ToAgent::CtrlAltDel),
        ID_REFRESH => send(ToAgent::RequestFullFrame),
        ID_FULLSCREEN => toggle_fullscreen(window),
        ID_CLIPBOARD => {
            let enabled = !app().clipboard_enabled.fetch_not(Ordering::SeqCst);
            let menu = stored_menu();
            let state = if enabled { MF_CHECKED } else { MF_UNCHECKED };
            CheckMenuItem(menu, ID_CLIPBOARD as u32, (MF_BYCOMMAND | state).0);
        }
        id if (ID_QUALITY_BASE..ID_QUALITY_BASE + QUALITIES.len()).contains(&id) => {
            let index = id - ID_QUALITY_BASE;
            let value = QUALITIES[index].1;
            app().ui.lock().unwrap().quality = value;
            send(ToAgent::SetQuality(value));
            let first = ID_QUALITY_BASE as u32;
            let _ = CheckMenuRadioItem(
                stored_menu(),
                first,
                first + QUALITIES.len() as u32 - 1,
                id as u32,
                MF_BYCOMMAND.0,
            );
        }
        id if id >= ID_MONITOR_BASE => {
            let index = (id - ID_MONITOR_BASE) as u32;
            app().ui.lock().unwrap().current_monitor = index;
            send(ToAgent::SelectMonitor(index));
            refresh_ui(window);
        }
        _ => {}
    }
}

fn send_mouse_position(window: HWND, lparam: LPARAM) {
    let x = (lparam.0 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
    if let Some((rx, ry)) = to_remote(x, y, image_rect(window)) {
        send(ToAgent::MouseMove { x: rx, y: ry });
    }
}

fn release_all_keys() {
    let keys: Vec<_> = app().ui.lock().unwrap().keys_down.drain().collect();
    for (scan, extended) in keys {
        send(ToAgent::Key { scan, extended, down: false });
    }
}

fn load_cursor(shape: CursorShape) -> Option<HCURSOR> {
    let id = match shape {
        CursorShape::Arrow => IDC_ARROW,
        CursorShape::IBeam => IDC_IBEAM,
        CursorShape::Hand => IDC_HAND,
        CursorShape::Wait => IDC_WAIT,
        CursorShape::AppStarting => IDC_APPSTARTING,
        CursorShape::Cross => IDC_CROSS,
        CursorShape::SizeAll => IDC_SIZEALL,
        CursorShape::SizeNs => IDC_SIZENS,
        CursorShape::SizeWe => IDC_SIZEWE,
        CursorShape::SizeNesw => IDC_SIZENESW,
        CursorShape::SizeNwse => IDC_SIZENWSE,
        CursorShape::No => IDC_NO,
        CursorShape::Hidden => return None,
    };
    unsafe { LoadCursorW(None, id) }.ok()
}

unsafe extern "system" fn window_proc(window: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let button = |b: MouseButton, down: bool| {
        send_mouse_position(window, lparam);
        send(ToAgent::MouseButton { button: b, down });
        // Keeps drags going when the pointer leaves the window.
        if down {
            SetCapture(window);
        } else {
            let _ = ReleaseCapture();
        }
        LRESULT(0)
    };
    let xbutton = || if (wparam.0 >> 16) & 0xFFFF == 1 { MouseButton::X1 } else { MouseButton::X2 };

    match msg {
        WM_PAINT => {
            paint(window);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_APP_REFRESH_UI => {
            refresh_ui(window);
            LRESULT(0)
        }
        WM_APP_TOGGLE_FULLSCREEN => {
            toggle_fullscreen(window);
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_command(window, wparam.0 & 0xFFFF);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            send_mouse_position(window, lparam);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => button(MouseButton::Left, true),
        WM_LBUTTONUP => button(MouseButton::Left, false),
        WM_RBUTTONDOWN => button(MouseButton::Right, true),
        WM_RBUTTONUP => button(MouseButton::Right, false),
        WM_MBUTTONDOWN => button(MouseButton::Middle, true),
        WM_MBUTTONUP => button(MouseButton::Middle, false),
        WM_XBUTTONDOWN => {
            button(xbutton(), true);
            LRESULT(1)
        }
        WM_XBUTTONUP => {
            button(xbutton(), false);
            LRESULT(1)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            send(ToAgent::Wheel { horizontal: msg == WM_MOUSEHWHEEL, delta });
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let shape = app().ui.lock().unwrap().cursor;
            SetCursor(load_cursor(shape));
            LRESULT(1)
        }
        WM_KILLFOCUS => {
            release_all_keys();
            LRESULT(0)
        }
        WM_SETFOCUS => {
            send(ToAgent::RequestFullFrame);
            LRESULT(0)
        }
        WM_DESTROY => {
            release_all_keys();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}

/// While this window has focus, every key goes to the remote computer, including
/// Win, Alt+Tab and Alt+F4. Ctrl+Alt+Del can't be intercepted, hence the menu item.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && GetForegroundWindow() == hwnd() {
        let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let vk = key.vkCode as u16;
        let scan = key.scanCode as u16;
        // The hook marks NumLock extended, but injecting it that way sends Pause.
        let extended = key.flags.0 & LLKHF_EXTENDED.0 != 0 && vk != VK_NUMLOCK.0;

        let mut ui = app().ui.lock().unwrap();
        // Swallowed keys don't update GetAsyncKeyState, so use the tracked set.
        let held = |scan: u16| ui.keys_down.contains(&(scan, false)) || ui.keys_down.contains(&(scan, true));
        if down && vk == VK_RETURN.0 && held(SCAN_CTRL) && held(SCAN_ALT) {
            drop(ui);
            let _ = PostMessageW(Some(hwnd()), WM_APP_TOGGLE_FULLSCREEN, WPARAM(0), LPARAM(0));
            return LRESULT(1);
        }
        if down {
            ui.keys_down.insert((scan, extended));
        } else {
            ui.keys_down.remove(&(scan, extended));
        }
        drop(ui);
        send(ToAgent::Key { scan, extended, down });
        return LRESULT(1);
    }
    CallNextHookEx(None, code, wparam, lparam)
}
