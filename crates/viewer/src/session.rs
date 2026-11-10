//! The remote desktop window: draws the remote screen, forwards input, and
//! reconnects by itself when the computer restarts or the network drops.

use crate::framebuffer::Framebuffer;
use crate::geometry::{fit_rect, to_remote};
use crate::net::{self, FingerprintChanged, NotAuthorized};
use crate::store;
use crate::transfer::{display_name, Transfers};
use anyhow::Result;
use rd_common::clipboard;
use rd_common::proto::{read_msg, write_msg, CursorShape, DirEntry, MonitorInfo, MouseButton, ToAgent, ToViewer};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, ReleaseCapture, SetCapture, SetFocus, VK_NUMLOCK, VK_RETURN};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_REFRESH_UI: u32 = WM_APP + 1;
const WM_APP_TOGGLE_FULLSCREEN: u32 = WM_APP + 2;
const WM_APP_TRANSFER: u32 = WM_APP + 3;
const TIMER_CLEAR_NOTE: usize = 1;

const ID_CAD: usize = 100;
const ID_FULLSCREEN: usize = 101;
const ID_REFRESH: usize = 102;
const ID_CLIPBOARD: usize = 103;
const ID_SEND_FILES: usize = 104;
const ID_GET_FILE: usize = 105;
const ID_CANCEL_TRANSFER: usize = 106;
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
    /// The last finished transfer, shown in the title for a while.
    transfer_note: Option<String>,
}

struct App {
    name: String,
    framebuffer: Mutex<Framebuffer>,
    ui: Mutex<UiState>,
    tx: UnboundedSender<ToAgent>,
    hwnd: AtomicIsize,
    clipboard_enabled: AtomicBool,
    transfers: Transfers,
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
    post_to_window(WM_APP_REFRESH_UI);
}

fn post_to_window(msg: u32) {
    let Some(app) = APP.get() else { return };
    let window = app.hwnd.load(Ordering::SeqCst);
    if window != 0 {
        let _ = unsafe { PostMessageW(Some(HWND(window as *mut _)), msg, WPARAM(0), LPARAM(0)) };
    }
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
            transfer_note: None,
        }),
        transfers: Transfers::new(tx.clone(), Arc::new(|| post_to_window(WM_APP_TRANSFER))),
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
                let result = run_connection(stream, &mut rx).await;
                app().transfers.on_disconnect();
                match result {
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
    write_msg(&mut writer, &ToAgent::SelectMonitor(rd_common::proto::FEATURE_PROBE)).await?;

    let write_side = async {
        while let Some(msg) = rx.recv().await {
            write_msg(&mut writer, &msg).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let read_side = async {
        loop {
            let msg = read_msg::<_, ToViewer>(&mut reader).await?;
            if app().transfers.on_message(&msg) {
                continue;
            }
            match msg {
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
                _ => {}
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
    DragAcceptFiles(window, true);
    Ok(())
}

unsafe fn build_menu() -> Result<HMENU> {
    let bar = CreateMenu()?;
    let session = CreatePopupMenu()?;
    AppendMenuW(session, MF_STRING, ID_CAD, w!("Send Ctrl+Alt+Del"))?;
    AppendMenuW(session, MF_STRING, ID_REFRESH, w!("Refresh screen"))?;
    AppendMenuW(session, MF_STRING, ID_FULLSCREEN, w!("Full screen\tCtrl+Alt+Enter"))?;
    AppendMenuW(session, MF_STRING | MF_CHECKED, ID_CLIPBOARD, w!("Share clipboard"))?;
    AppendMenuW(session, MF_SEPARATOR, 0, None)?;
    AppendMenuW(session, MF_STRING, ID_SEND_FILES, w!("Send files…	(or drop them on the window)"))?;
    AppendMenuW(session, MF_STRING, ID_GET_FILE, w!("Get a file…"))?;
    AppendMenuW(session, MF_STRING, ID_CANCEL_TRANSFER, w!("Cancel file transfer"))?;
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

unsafe fn refresh_title(window: HWND) {
    let (connected, status, note) = {
        let ui = app().ui.lock().unwrap();
        (ui.connected, ui.status.clone(), ui.transfer_note.clone())
    };
    let state = if connected {
        "Connected"
    } else if status.contains("Reconnecting") {
        "Reconnecting"
    } else {
        "Not connected"
    };
    let transfer = app().transfers.snapshot();
    let activity = match (&transfer.current, note) {
        (Some(p), _) => {
            let verb = if p.upload { "Sending" } else { "Getting" };
            let percent = match p.total {
                Some(0) => 100,
                Some(total) => p.done * 100 / total,
                None => 0,
            };
            let more = if transfer.queued > 0 { format!(", {} more waiting", transfer.queued) } else { String::new() };
            format!(" - {verb} {} {percent}%{more}", p.name)
        }
        (None, Some(note)) => format!(" - {note}"),
        (None, None) => String::new(),
    };
    let title = wide(&format!("{} - {state}{activity} - Remote Access", app().name));
    let _ = SetWindowTextW(window, PCWSTR(title.as_ptr()));
}

unsafe fn refresh_ui(window: HWND) {
    let (monitors, current) = {
        let ui = app().ui.lock().unwrap();
        (ui.monitors.clone(), ui.current_monitor)
    };
    refresh_title(window);

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
        ID_SEND_FILES => {
            let files = open_files_dialog(window);
            if !files.is_empty() {
                send_files(window, files);
            }
        }
        ID_GET_FILE => get_file(window),
        ID_CANCEL_TRANSFER => {
            if let Some(current) = app().transfers.snapshot().current {
                app().transfers.cancel(current.id);
            }
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
        WM_APP_TRANSFER => {
            transfer_update(window);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_CLEAR_NOTE => {
            let _ = KillTimer(Some(window), TIMER_CLEAR_NOTE);
            app().ui.lock().unwrap().transfer_note = None;
            refresh_title(window);
            LRESULT(0)
        }
        WM_DROPFILES => {
            let files = dropped_files(HDROP(wparam.0 as *mut _));
            if !files.is_empty() {
                send_files(window, files);
            }
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

// ------------------------------------------------------------------ file transfer

fn message_box(window: HWND, text: &str, warning: bool) {
    let text = wide(text);
    let caption = wide(&format!("{} - Remote Access", app().name));
    let icon = if warning { MB_ICONWARNING } else { MB_ICONINFORMATION };
    unsafe { MessageBoxW(Some(window), PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), MB_OK | icon) };
}

/// Runs on the window thread after the transfer engine changes.
unsafe fn transfer_update(window: HWND) {
    deliver_listings();
    let mut failures = Vec::new();
    let mut note = None;
    for done in app().transfers.take_finished() {
        match done.result {
            Ok(path) if done.upload => note = Some(format!("Sent {} to {path}", done.name)),
            Ok(path) => note = Some(format!("Saved {} to {path}", done.name)),
            Err(e) if e == "cancelled" => note = Some(format!("Cancelled {}", done.name)),
            Err(e) => {
                let verb = if done.upload { "send" } else { "get" };
                failures.push(format!("Couldn't {verb} {}: {e}", done.name));
            }
        }
    }
    if let Some(note) = note {
        app().ui.lock().unwrap().transfer_note = Some(note);
        SetTimer(Some(window), TIMER_CLEAR_NOTE, 15_000, None);
    }
    refresh_title(window);
    if !failures.is_empty() {
        message_box(window, &failures.join("\n"), true);
    }
}

fn transfer_available(window: HWND) -> bool {
    if app().transfers.snapshot().supported {
        return true;
    }
    let connected = app().ui.lock().unwrap().connected;
    let text = if connected {
        "This computer's agent doesn't support file transfer yet. Update it with the latest rdagent and try again."
    } else {
        "Files can be sent once the computer is connected."
    };
    message_box(window, text, true);
    false
}

unsafe fn dropped_files(drop: HDROP) -> Vec<PathBuf> {
    let count = DragQueryFileW(drop, u32::MAX, None);
    let mut files = Vec::new();
    for i in 0..count {
        let len = DragQueryFileW(drop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        let copied = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
        files.push(PathBuf::from(String::from_utf16_lossy(&buf[..copied])));
    }
    DragFinish(drop);
    files
}

/// Asks where on the remote computer the files go, then queues them.
unsafe fn send_files(window: HWND, files: Vec<PathBuf>) {
    if !transfer_available(window) {
        return;
    }
    let what = match files.as_slice() {
        [one] => display_name(&one.to_string_lossy()),
        many => format!("{} files", many.len()),
    };
    let prompt = format!("Choose where to put {what} on {}.", app().name);
    let Some(dir) = browse_remote(window, Pick::Folder, &prompt) else { return };
    if let Err(e) = app().transfers.upload(files, &dir) {
        message_box(window, &e, true);
    }
}

unsafe fn get_file(window: HWND) {
    if !transfer_available(window) {
        return;
    }
    let prompt = format!("Choose a file to copy from {} to this computer.", app().name);
    let Some(remote) = browse_remote(window, Pick::File, &prompt) else { return };
    if let Some(local) = save_file_dialog(window, &display_name(&remote)) {
        app().transfers.download(&remote, local);
    }
}

unsafe fn open_files_dialog(window: HWND) -> Vec<PathBuf> {
    let mut buf = vec![0u16; 64 * 1024];
    let title = wide("Send files");
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: window,
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_ALLOWMULTISELECT | OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if !GetOpenFileNameW(&mut ofn).as_bool() {
        return Vec::new();
    }
    // One file: its full path. Several: the folder, then each name, all null separated.
    let parts: Vec<String> = buf
        .split(|&c| c == 0)
        .take_while(|part| !part.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    match parts.as_slice() {
        [] => Vec::new(),
        [single] => vec![PathBuf::from(single)],
        [dir, names @ ..] => names.iter().map(|name| PathBuf::from(dir).join(name)).collect(),
    }
}

unsafe fn save_file_dialog(window: HWND, name: &str) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32 * 1024];
    for (slot, c) in buf.iter_mut().zip(name.encode_utf16()) {
        *slot = c;
    }
    let title = wide(&format!("Save {name} from {}", app().name));
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: window,
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if !GetSaveFileNameW(&mut ofn).as_bool() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

// ------------------------------------------------------------------ remote folder browser

#[derive(Clone, Copy, PartialEq)]
enum Pick {
    Folder,
    File,
}

const ID_BROWSER_LIST: usize = 10;
const ID_BROWSER_UP: usize = 11;
/// IDOK and IDCANCEL, so Enter and Esc work through IsDialogMessage.
const ID_BROWSER_OK: usize = 1;
const ID_BROWSER_CANCEL: usize = 2;

struct Browser {
    pick: Pick,
    list: isize,
    label: isize,
    up: isize,
    request: u32,
    path: String,
    parent: Option<String>,
    /// Entries shown in the list, in list order.
    shown: Vec<DirEntry>,
    chosen: Option<String>,
    done: bool,
}

static BROWSER: Mutex<Option<Browser>> = Mutex::new(None);

fn handle(value: isize) -> HWND {
    HWND(value as *mut _)
}

/// A modal window listing the remote computer's folders. Returns the chosen folder or file.
unsafe fn browse_remote(owner: HWND, pick: Pick, prompt: &str) -> Option<String> {
    let module = GetModuleHandleW(None).ok()?;
    let instance = HINSTANCE(module.0);
    let class = w!("RemoteAccessBrowser");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(browser_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut _),
        ..Default::default()
    };
    RegisterClassW(&wc);

    let scale = GetDpiForWindow(owner).max(96) as f32 / 96.0;
    let px = |v: i32| (v as f32 * scale) as i32;
    let (width, height) = (px(560), px(520));
    let mut owner_rect = RECT::default();
    let _ = GetWindowRect(owner, &mut owner_rect);
    let x = owner_rect.left + ((owner_rect.right - owner_rect.left) - width) / 2;
    let y = owner_rect.top + ((owner_rect.bottom - owner_rect.top) - height) / 2;
    let title = wide(match pick {
        Pick::Folder => "Send files to…",
        Pick::File => "Get a file",
    });
    let window = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        class,
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        x.max(0),
        y.max(0),
        width,
        height,
        Some(owner),
        None,
        Some(instance),
        None,
    )
    .ok()?;

    let mut client = RECT::default();
    let _ = GetClientRect(window, &mut client);
    let (cw, ch) = (client.right, client.bottom);
    let font = CreateFontW(
        -px(15),
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        w!("Segoe UI"),
    );
    let child = |class: PCWSTR, text: &str, style: WINDOW_STYLE, id: usize, rect: (i32, i32, i32, i32)| {
        let text = wide(text);
        let control = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            Some(window),
            Some(HMENU(id as *mut _)),
            Some(instance),
            None,
        )
        .unwrap_or_default();
        SendMessageW(control, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        control
    };

    let m = px(12);
    let button_h = px(30);
    let label_h = px(22);
    child(w!("STATIC"), prompt, WINDOW_STYLE(0), 0, (m, m, cw - 2 * m, label_h));
    let row = m + label_h + px(6);
    let up = child(w!("BUTTON"), "Up", WS_TABSTOP, ID_BROWSER_UP, (m, row, px(70), button_h));
    let label = child(
        w!("STATIC"),
        "",
        WINDOW_STYLE(0x8000 | 0x200), // SS_PATHELLIPSIS | SS_CENTERIMAGE,
        0,
        (m + px(80), row, cw - 2 * m - px(80), button_h),
    );
    let list_top = row + button_h + px(8);
    let list = child(
        w!("LISTBOX"),
        "",
        WS_BORDER | WS_VSCROLL | WS_TABSTOP | WINDOW_STYLE((LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32),
        ID_BROWSER_LIST,
        (m, list_top, cw - 2 * m, ch - list_top - button_h - 2 * m),
    );
    let ok_label = match pick {
        Pick::Folder => "Put files here",
        Pick::File => "Get file",
    };
    let bw = px(130);
    child(
        w!("BUTTON"),
        ok_label,
        WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        ID_BROWSER_OK,
        (cw - m - 2 * bw - px(8), ch - m - button_h, bw, button_h),
    );
    child(w!("BUTTON"), "Cancel", WS_TABSTOP, ID_BROWSER_CANCEL, (cw - m - bw, ch - m - button_h, bw, button_h));

    *BROWSER.lock().unwrap() = Some(Browser {
        pick,
        list: list.0 as isize,
        label: label.0 as isize,
        up: up.0 as isize,
        request: 0,
        path: String::new(),
        parent: None,
        shown: Vec::new(),
        chosen: None,
        done: false,
    });
    browser_navigate("");

    let _ = EnableWindow(owner, false);
    let _ = ShowWindow(window, SW_SHOW);
    let _ = SetFocus(Some(list));
    let mut msg = MSG::default();
    while !BROWSER.lock().unwrap().as_ref().is_none_or(|b| b.done) {
        if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Leave the quit for the main loop.
            PostQuitMessage(msg.wParam.0 as i32);
            break;
        }
        // Enter in the list opens the selected entry rather than pressing the default button.
        if msg.message == WM_KEYDOWN && msg.wParam.0 == VK_RETURN.0 as usize && GetFocus() == list {
            browser_open_selection();
            continue;
        }
        if !IsDialogMessageW(window, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let _ = EnableWindow(owner, true);
    let chosen = BROWSER.lock().unwrap().take().and_then(|b| b.chosen);
    let _ = DestroyWindow(window);
    let _ = DeleteObject(font.into());
    let _ = SetForegroundWindow(owner);
    chosen
}

unsafe fn browser_navigate(path: &str) {
    let request = app().transfers.list_dir(path);
    let mut guard = BROWSER.lock().unwrap();
    let Some(b) = guard.as_mut() else { return };
    b.request = request;
    let text = wide("Loading…");
    let _ = SetWindowTextW(handle(b.label), PCWSTR(text.as_ptr()));
}

/// Hands folder listings to the browser, if it's waiting for one.
unsafe fn deliver_listings() {
    for listing in app().transfers.take_listings() {
        let mut guard = BROWSER.lock().unwrap();
        let Some(b) = guard.as_mut() else { continue };
        if listing.request != b.request {
            continue;
        }
        b.path = listing.path;
        b.parent = listing.parent;
        let pick = b.pick;
        b.shown = listing.entries.into_iter().filter(|e| e.is_dir || pick == Pick::File).collect();

        let list = handle(b.list);
        SendMessageW(list, LB_RESETCONTENT, None, None);
        for entry in &b.shown {
            let text = if entry.is_dir {
                format!("\u{1F4C1}  {}", entry.name)
            } else {
                format!("        {}   ({})", entry.name, human_size(entry.size))
            };
            let text = wide(&text);
            SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(text.as_ptr() as isize)));
        }
        let place = if b.path.is_empty() { format!("{} (choose a place)", app().name) } else { b.path.clone() };
        let label = match listing.error {
            Some(e) => format!("{place} - {e}"),
            None if b.shown.is_empty() && !b.path.is_empty() => format!("{place} (empty)"),
            None => place,
        };
        let label = wide(&label);
        let _ = SetWindowTextW(handle(b.label), PCWSTR(label.as_ptr()));
        let _ = EnableWindow(handle(b.up), b.parent.is_some());
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn browser_selection() -> Option<DirEntry> {
    let guard = BROWSER.lock().unwrap();
    let b = guard.as_ref()?;
    let index = unsafe { SendMessageW(handle(b.list), LB_GETCURSEL, None, None) }.0;
    usize::try_from(index).ok().and_then(|i| b.shown.get(i).cloned())
}

fn browser_finish(chosen: Option<String>) {
    if let Some(b) = BROWSER.lock().unwrap().as_mut() {
        b.chosen = chosen;
        b.done = true;
    }
}

/// Double-click or Enter: folders open, files are picked when choosing a file.
unsafe fn browser_open_selection() {
    match browser_selection() {
        Some(entry) if entry.is_dir => browser_navigate(&entry.path),
        Some(entry) => browser_finish(Some(entry.path)),
        None => {}
    }
}

/// The OK button. Folders: the folder being shown, or the selected one at the
/// starting places. Files: the selected file, or open the selected folder.
unsafe fn browser_ok() {
    let (pick, path) = {
        let guard = BROWSER.lock().unwrap();
        let Some(b) = guard.as_ref() else { return };
        (b.pick, b.path.clone())
    };
    match (pick, browser_selection()) {
        (Pick::Folder, _) if !path.is_empty() => browser_finish(Some(path)),
        (Pick::Folder, Some(entry)) => browser_finish(Some(entry.path)),
        (Pick::File, Some(entry)) if !entry.is_dir => browser_finish(Some(entry.path)),
        (Pick::File, Some(entry)) => browser_navigate(&entry.path),
        _ => {}
    }
}

unsafe extern "system" fn browser_proc(window: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            match id {
                ID_BROWSER_OK => browser_ok(),
                ID_BROWSER_CANCEL => browser_finish(None),
                ID_BROWSER_UP => {
                    let parent = BROWSER.lock().unwrap().as_ref().and_then(|b| b.parent.clone());
                    if let Some(parent) = parent {
                        browser_navigate(&parent);
                    }
                }
                ID_BROWSER_LIST if code == LBN_DBLCLK => browser_open_selection(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            browser_finish(None);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}
