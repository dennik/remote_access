//! The remote desktop window on macOS: draws the remote screen with egui, forwards
//! input, and reconnects by itself when the computer restarts or the network drops.
//!
//! Windows uses the Win32 window in session.rs instead, because only a low-level
//! keyboard hook there can capture Win, Alt+Tab and Alt+F4.

use crate::framebuffer::Framebuffer;
use crate::geometry::{fit_rect, to_remote};
use crate::keymap;
use crate::net::{self, FingerprintChanged, NotAuthorized};
use crate::store;
use crate::transfer::{self, Listing, Transfers};
use anyhow::{anyhow, Result};
use eframe::egui;
use rd_common::clipboard;
use rd_common::proto::{read_msg, write_msg, CursorShape, MonitorInfo, MouseButton, ToAgent, ToViewer};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const QUALITIES: [(&str, u8); 3] = [("Fast (lower quality)", 50), ("Balanced", 75), ("Sharp (more bandwidth)", 90)];
/// Windows counts one wheel notch as 120.
const WHEEL_NOTCH: f32 = 120.0;
/// Trackpads scroll in points. This many points make one notch.
const POINTS_PER_NOTCH: f32 = 40.0;

struct UiState {
    status: String,
    connected: bool,
    monitors: Vec<MonitorInfo>,
    current_monitor: u32,
    cursor: CursorShape,
    quality: u8,
    /// Text received from the remote computer, so it isn't sent straight back.
    clipboard_echo: Option<String>,
}

/// State shared between the window and the network and clipboard threads.
struct Shared {
    framebuffer: Mutex<Framebuffer>,
    /// Set when the framebuffer changed and the texture needs uploading.
    dirty: AtomicBool,
    ui: Mutex<UiState>,
    tx: UnboundedSender<ToAgent>,
    clipboard_enabled: AtomicBool,
    ctx: Arc<OnceLock<egui::Context>>,
    transfers: Transfers,
}

impl Shared {
    fn send(&self, msg: ToAgent) {
        if debug() && !matches!(msg, ToAgent::Ack { .. }) {
            eprintln!("send {msg:?}");
        }
        let _ = self.tx.send(msg);
    }

    fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }
}

/// RD_DEBUG=1 prints the input sent to the remote computer, for troubleshooting.
fn debug() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("RD_DEBUG").is_some())
}

struct Session {
    name: String,
    shared: Arc<Shared>,
    texture: Option<egui::TextureHandle>,
    keys_down: HashSet<(u16, bool)>,
    buttons_down: Vec<egui::PointerButton>,
    /// Scroll not yet sent, in Windows wheel units.
    wheel: egui::Vec2,
    fullscreen: bool,
    focused: bool,
    /// The remote file browser, while open.
    picker: Option<Picker>,
    /// The last transfer result, shown until dismissed or replaced.
    notice: Option<(String, bool, Instant)>,
}

/// What the remote browser is choosing.
enum PickerMode {
    /// A folder to put these local files in.
    Upload(Vec<PathBuf>),
    /// A file to bring back.
    Download,
}

struct Picker {
    mode: PickerMode,
    /// The listing being waited for. Older replies are ignored.
    request: u32,
    listing: Option<Listing>,
}

pub fn run(addr: String, fingerprint: String, name: String) -> Result<()> {
    let key = store::load_or_create_key()?;
    let (tx, rx) = unbounded_channel();
    let ctx: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    let repaint_ctx = ctx.clone();
    let transfers = Transfers::new(
        tx.clone(),
        Arc::new(move || {
            if let Some(ctx) = repaint_ctx.get() {
                ctx.request_repaint();
            }
        }),
    );
    let shared = Arc::new(Shared {
        framebuffer: Mutex::new(Framebuffer::default()),
        dirty: AtomicBool::new(false),
        ui: Mutex::new(UiState {
            status: "Connecting…".to_string(),
            connected: false,
            monitors: Vec::new(),
            current_monitor: 0,
            cursor: CursorShape::Arrow,
            quality: 75,
            clipboard_echo: None,
        }),
        tx,
        clipboard_enabled: AtomicBool::new(true),
        ctx,
        transfers,
    });

    let net_shared = shared.clone();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Runtime::new() else { return };
        runtime.block_on(network_loop(net_shared, addr, fingerprint, key, rx));
    });
    let clip_shared = shared.clone();
    std::thread::spawn(move || clipboard_loop(clip_shared));

    let title = format!("{name} - Remote Access");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]).with_title(&title),
        // Command+C, X and V must reach the remote computer as keys.
        clipboard_shortcuts: false,
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let _ = shared.ctx.set(cc.egui_ctx.clone());
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            Ok(Box::new(Session {
                name,
                shared,
                texture: None,
                keys_down: HashSet::new(),
                buttons_down: Vec::new(),
                wheel: egui::Vec2::ZERO,
                fullscreen: false,
                focused: true,
                picker: None,
                notice: None,
            }))
        }),
    )
    .map_err(|e| anyhow!("couldn't open the session window: {e}"))
}

// ------------------------------------------------------------------ network

async fn network_loop(
    shared: Arc<Shared>,
    addr: String,
    fingerprint: String,
    key: [u8; 32],
    mut rx: UnboundedReceiver<ToAgent>,
) {
    loop {
        let reason = match net::connect(&addr, Some(&fingerprint), &key).await {
            Ok(stream) => {
                set_status(&shared, true, "");
                match run_connection(&shared, stream, &mut rx).await {
                    Ok(()) => "The connection closed.".to_string(),
                    Err(e) => format!("Connection lost: {e}"),
                }
            }
            // Retrying can't fix these, and the identity failure should not be retried.
            Err(e) if e.is::<FingerprintChanged>() || e.is::<NotAuthorized>() => {
                set_status(&shared, false, &format!("Not connected: {e}"));
                return;
            }
            Err(e) => format!("Can't reach the computer: {e}"),
        };
        shared.transfers.on_disconnect();
        set_status(&shared, false, &format!("{reason}\nReconnecting…"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Input typed while disconnected must not run on the machine once it's back.
        while rx.try_recv().is_ok() {}
    }
}

fn set_status(shared: &Shared, connected: bool, status: &str) {
    {
        let mut ui = shared.ui.lock().unwrap();
        ui.connected = connected;
        ui.status = status.to_string();
    }
    shared.repaint();
}

async fn run_connection<S>(shared: &Arc<Shared>, stream: S, rx: &mut UnboundedReceiver<ToAgent>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // The agent starts each connection with defaults, so restore the user's choices.
    let (quality, monitor) = {
        let ui = shared.ui.lock().unwrap();
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
            if shared.transfers.on_message(&msg) {
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
                    shared.framebuffer.lock().unwrap().apply(width, height, &tiles, &decoded)?;
                    shared.dirty.store(true, Ordering::SeqCst);
                    shared.repaint();
                    shared.send(ToAgent::Ack { seq });
                }
                ToViewer::Monitors { monitors, current } => {
                    let mut ui = shared.ui.lock().unwrap();
                    // Keeps the user's selection if it's still valid after a reconnect.
                    if ui.current_monitor as usize >= monitors.len() || ui.monitors.is_empty() {
                        ui.current_monitor = current;
                    }
                    ui.monitors = monitors;
                    drop(ui);
                    shared.repaint();
                }
                ToViewer::Cursor(shape) => {
                    shared.ui.lock().unwrap().cursor = shape;
                    shared.repaint();
                }
                ToViewer::Clipboard(text) => {
                    if shared.clipboard_enabled.load(Ordering::SeqCst) {
                        shared.ui.lock().unwrap().clipboard_echo = Some(text.clone());
                        clipboard::set_text(&text);
                    }
                }
                // File messages were handled by `transfers` above.
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

fn clipboard_loop(shared: Arc<Shared>) {
    let mut seen = clipboard::sequence_number();
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let now = clipboard::sequence_number();
        if now == seen {
            continue;
        }
        seen = now;
        if !shared.clipboard_enabled.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(text) = clipboard::get_text() {
            let echo = shared.ui.lock().unwrap().clipboard_echo.take();
            if echo.as_deref() != Some(text.as_str()) {
                shared.send(ToAgent::Clipboard(text));
            }
        }
    }
}

// ------------------------------------------------------------------ window

fn cursor_icon(shape: CursorShape) -> egui::CursorIcon {
    use egui::CursorIcon as C;
    match shape {
        CursorShape::Arrow => C::Default,
        CursorShape::IBeam => C::Text,
        CursorShape::Hand => C::PointingHand,
        CursorShape::Wait => C::Wait,
        CursorShape::AppStarting => C::Progress,
        CursorShape::Cross => C::Crosshair,
        CursorShape::SizeAll => C::Move,
        CursorShape::SizeNs => C::ResizeVertical,
        CursorShape::SizeWe => C::ResizeHorizontal,
        CursorShape::SizeNesw => C::ResizeNeSw,
        CursorShape::SizeNwse => C::ResizeNwSe,
        CursorShape::No => C::NotAllowed,
        CursorShape::Hidden => C::None,
    }
}

fn mouse_button(button: egui::PointerButton) -> MouseButton {
    match button {
        egui::PointerButton::Primary => MouseButton::Left,
        egui::PointerButton::Secondary => MouseButton::Right,
        egui::PointerButton::Middle => MouseButton::Middle,
        egui::PointerButton::Extra1 => MouseButton::X1,
        egui::PointerButton::Extra2 => MouseButton::X2,
    }
}

impl Session {
    fn send(&self, msg: ToAgent) {
        self.shared.send(msg);
    }

    fn key(&mut self, code: (u16, bool), down: bool) {
        if down {
            self.keys_down.insert(code);
        } else {
            self.keys_down.remove(&code);
        }
        self.send(ToAgent::Key { scan: code.0, extended: code.1, down });
    }

    fn tap(&self, code: (u16, bool)) {
        self.send(ToAgent::Key { scan: code.0, extended: code.1, down: true });
        self.send(ToAgent::Key { scan: code.0, extended: code.1, down: false });
    }

    fn release_all(&mut self) {
        for (scan, extended) in std::mem::take(&mut self.keys_down) {
            self.send(ToAgent::Key { scan, extended, down: false });
        }
        for button in std::mem::take(&mut self.buttons_down) {
            self.send(ToAgent::MouseButton { button: mouse_button(button), down: false });
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    fn upload_frame(&mut self, ctx: &egui::Context) {
        if !self.shared.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let image = {
            let fb = self.shared.framebuffer.lock().unwrap();
            if fb.width == 0 || fb.height == 0 {
                return;
            }
            let pixels = fb.pixels.chunks_exact(4).map(|p| egui::Color32::from_rgb(p[2], p[1], p[0])).collect();
            egui::ColorImage::new([fb.width as usize, fb.height as usize], pixels)
        };
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("remote", image, egui::TextureOptions::LINEAR)),
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Session", |ui| {
                if ui.button("Send Ctrl+Alt+Del").clicked() {
                    self.send(ToAgent::CtrlAltDel);
                }
                if ui.button("Send Windows key").clicked() {
                    self.tap(keymap::WIN);
                }
                let files_ok = self.shared.transfers.snapshot().supported;
                if ui.add_enabled(files_ok, egui::Button::new("Send files…")).clicked() {
                    if let Some(paths) = rfd::FileDialog::new().set_title("Send files").pick_files() {
                        self.open_upload(paths);
                    }
                }
                if ui.add_enabled(files_ok, egui::Button::new("Get a file…")).clicked() {
                    self.open_picker(PickerMode::Download);
                }
                if ui.button("Refresh screen").clicked() {
                    self.send(ToAgent::RequestFullFrame);
                }
                if ui.button("Full screen (Ctrl+Option+Return)").clicked() {
                    self.toggle_fullscreen(&ctx);
                }
                let mut share = self.shared.clipboard_enabled.load(Ordering::SeqCst);
                if ui.checkbox(&mut share, "Share clipboard").changed() {
                    self.shared.clipboard_enabled.store(share, Ordering::SeqCst);
                }
            });
            let (monitors, current, quality) = {
                let ui = self.shared.ui.lock().unwrap();
                (ui.monitors.clone(), ui.current_monitor, ui.quality)
            };
            ui.menu_button("Monitor", |ui| {
                if monitors.is_empty() {
                    ui.weak("Not connected");
                }
                for (i, m) in monitors.iter().enumerate() {
                    let primary = if m.primary { ", primary" } else { "" };
                    let label = format!("Display {} ({}x{}{primary})", i + 1, m.width, m.height);
                    if ui.radio(i as u32 == current, label).clicked() {
                        self.shared.ui.lock().unwrap().current_monitor = i as u32;
                        self.send(ToAgent::SelectMonitor(i as u32));
                    }
                }
            });
            ui.menu_button("Quality", |ui| {
                for (label, value) in QUALITIES {
                    if ui.radio(value == quality, label).clicked() {
                        self.shared.ui.lock().unwrap().quality = value;
                        self.send(ToAgent::SetQuality(value));
                    }
                }
            });
        });
    }

    fn open_picker(&mut self, mode: PickerMode) {
        // Keys and buttons held now would stay down on the remote computer.
        self.release_all();
        let request = self.shared.transfers.list_dir("");
        self.picker = Some(Picker { mode, request, listing: None });
    }

    fn open_upload(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.open_picker(PickerMode::Upload(paths));
    }

    fn notify(&mut self, text: String, ok: bool) {
        self.notice = Some((text, ok, Instant::now()));
    }

    /// Dropped files, results, the progress bar and the remote browser.
    fn file_transfer_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let state = self.shared.transfers.snapshot();

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            if !state.supported {
                self.notify(
                    "This computer's agent can't receive files. Update the agent to the latest version.".to_string(),
                    false,
                );
            } else if self.picker.is_none() {
                self.open_upload(dropped);
            }
        }

        for finished in self.shared.transfers.take_finished() {
            let verb = if finished.upload { "Sent" } else { "Saved" };
            match finished.result {
                Ok(path) => self.notify(format!("{verb} {} to {path}", finished.name), true),
                Err(e) => self.notify(format!("{} failed: {e}", finished.name), false),
            }
        }
        for listing in self.shared.transfers.take_listings() {
            if let Some(picker) = &mut self.picker {
                if picker.request == listing.request {
                    picker.listing = Some(listing);
                }
            }
        }
        if !state.supported && self.picker.is_some() {
            self.picker = None;
            self.notify("The connection dropped, so the file browser closed.".to_string(), false);
        }

        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering && state.supported {
            egui::Area::new(egui::Id::unique("drop hint")).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0)).show(&ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.heading(format!("Drop to send to {}", self.name));
                });
            });
        }

        if state.current.is_some() || self.notice.is_some() {
            egui::Panel::bottom("transfers").show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(current) = &state.current {
                        let verb = if current.upload { "Sending" } else { "Getting" };
                        let fraction = current.total.filter(|t| *t > 0).map(|t| current.done as f32 / t as f32);
                        ui.label(format!("{verb} {}", current.name));
                        let bar = egui::ProgressBar::new(fraction.unwrap_or(0.0)).desired_width(220.0);
                        ui.add(if fraction.is_some() { bar.show_percentage() } else { bar.animate(true) });
                        if state.queued > 0 {
                            ui.weak(format!("{} more waiting", state.queued));
                        }
                        if ui.button("Cancel").clicked() {
                            self.shared.transfers.cancel(current.id);
                        }
                        ui.separator();
                    }
                    let mut dismiss = false;
                    if let Some((text, ok, at)) = &self.notice {
                        let colour = if *ok { egui::Color32::from_rgb(60, 170, 90) } else { egui::Color32::from_rgb(220, 90, 80) };
                        ui.colored_label(colour, text);
                        // Successes fade after a while; failures stay until dismissed.
                        dismiss = ui.small_button("✕").clicked() || (*ok && at.elapsed() > Duration::from_secs(8));
                    }
                    if dismiss {
                        self.notice = None;
                    }
                });
            });
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        self.picker_window(&ctx);
    }

    fn picker_window(&mut self, ctx: &egui::Context) {
        let Some(picker) = &self.picker else { return };
        let title = match &picker.mode {
            PickerMode::Upload(files) if files.len() == 1 => {
                format!("Where on {} should {} go?", self.name, transfer::display_name(&files[0].to_string_lossy()))
            }
            PickerMode::Upload(files) => format!("Where on {} should these {} files go?", self.name, files.len()),
            PickerMode::Download => format!("Choose a file on {}", self.name),
        };
        let upload = matches!(picker.mode, PickerMode::Upload(_));
        let listing = picker.listing.clone();

        let mut navigate = None::<String>;
        let mut choose_file = None::<(String, String)>;
        let mut put_here = false;
        let mut close = false;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_size([520.0, 420.0])
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let Some(listing) = &listing else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading…");
                    });
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    return;
                };
                ui.horizontal(|ui| {
                    if let Some(parent) = &listing.parent {
                        if ui.button("⬆ Up").clicked() {
                            navigate = Some(parent.clone());
                        }
                    }
                    let here = if listing.path.is_empty() { "Places".to_string() } else { listing.path.clone() };
                    ui.monospace(here);
                });
                if let Some(error) = &listing.error {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 80), error);
                }
                ui.separator();
                egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, false]).show(ui, |ui| {
                    for entry in &listing.entries {
                        if entry.is_dir {
                            if ui.selectable_label(false, format!("📁 {}", entry.name)).clicked() {
                                navigate = Some(entry.path.clone());
                            }
                        } else if upload {
                            ui.add_enabled(false, egui::Button::selectable(false, format!("    {}", entry.name)));
                        } else if ui
                            .selectable_label(false, format!("    {}   ({})", entry.name, human_size(entry.size)))
                            .clicked()
                        {
                            choose_file = Some((entry.path.clone(), entry.name.clone()));
                        }
                    }
                    if listing.entries.is_empty() && listing.error.is_none() {
                        ui.weak("This folder is empty.");
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if upload {
                        // The starting places are a list, not a folder.
                        let in_folder = !listing.path.is_empty() && listing.error.is_none();
                        if ui.add_enabled(in_folder, egui::Button::new("Put files here")).clicked() {
                            put_here = true;
                        }
                    } else {
                        ui.weak("Click a file to save it on this Mac.");
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if let Some(path) = navigate {
            let request = self.shared.transfers.list_dir(&path);
            if let Some(picker) = &mut self.picker {
                picker.request = request;
                picker.listing = None;
            }
        }
        if put_here {
            if let Some(Picker { mode: PickerMode::Upload(files), listing: Some(listing), .. }) = self.picker.take() {
                if let Err(e) = self.shared.transfers.upload(files, &listing.path) {
                    self.notify(e, false);
                }
            }
        }
        if let Some((remote, name)) = choose_file {
            let downloads = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads"));
            let mut dialog = rfd::FileDialog::new().set_title("Save file").set_file_name(&name);
            if let Some(dir) = downloads {
                dialog = dialog.set_directory(dir);
            }
            if let Some(local) = dialog.save_file() {
                self.shared.transfers.download(&remote, local);
                self.picker = None;
            }
        }
        if close {
            self.picker = None;
        }
    }

    /// Forwards this frame's keyboard and mouse input to the remote computer.
    fn forward_input(&mut self, ctx: &egui::Context, area: egui::Rect, layer: egui::LayerId, image: (i32, i32, i32, i32)) {
        let events = ctx.input(|i| i.events.clone());
        // A press belongs to the remote screen only if no menu or popup is drawn over it there.
        let on_screen = |pos: egui::Pos2| area.contains(pos) && ctx.layer_id_at(pos).is_none_or(|l| l == layer);
        let remote = |pos: egui::Pos2| {
            to_remote((pos.x - area.min.x) as i32, (pos.y - area.min.y) as i32, image)
        };
        for event in events {
            match event {
                egui::Event::WindowFocused(focused) => {
                    self.focused = focused;
                    if focused {
                        self.send(ToAgent::RequestFullFrame);
                    } else {
                        self.release_all();
                    }
                }
                // Nothing else is forwarded while another window has focus.
                // Nothing else is forwarded while the file browser is open either.
                _ if !self.focused || self.picker.is_some() => {}
                egui::Event::Key { key, physical_key, pressed, repeat, .. } => {
                    let key = physical_key.unwrap_or(key);
                    let Some(code) = keymap::scan_code(key) else { continue };
                    let held = |c| self.keys_down.contains(&c);
                    if pressed && key == egui::Key::Enter && held(keymap::CTRL) && held(keymap::ALT) {
                        self.toggle_fullscreen(ctx);
                        continue;
                    }
                    let command_held = ctx.input(|i| i.modifiers.mac_cmd);
                    if key == egui::Key::CapsLock {
                        // macOS reports Caps Lock as held while it's on, so toggle on each change.
                        self.tap(code);
                    } else if pressed && command_held && !keymap::is_modifier(key) {
                        // No release will be reported for this key while Command is held.
                        self.tap(code);
                    } else if pressed || !repeat {
                        self.key(code, pressed);
                    }
                }
                egui::Event::PointerMoved(pos) if area.contains(pos) || !self.buttons_down.is_empty() => {
                    if let Some((x, y)) = remote(pos) {
                        self.send(ToAgent::MouseMove { x, y });
                    }
                }
                egui::Event::PointerButton { pos, button, pressed, .. } => {
                    if debug() {
                        eprintln!("pointer {button:?} pressed={pressed} at {pos:?}, on screen: {}", on_screen(pos));
                    }
                    if pressed && !on_screen(pos) {
                        continue;
                    }
                    if pressed {
                        if !self.buttons_down.contains(&button) {
                            self.buttons_down.push(button);
                        }
                    } else if let Some(i) = self.buttons_down.iter().position(|b| *b == button) {
                        self.buttons_down.remove(i);
                    } else {
                        continue;
                    }
                    if let Some((x, y)) = remote(pos) {
                        self.send(ToAgent::MouseMove { x, y });
                    }
                    self.send(ToAgent::MouseButton { button: mouse_button(button), down: pressed });
                }
                egui::Event::MouseWheel { unit, delta, .. } => {
                    if !ctx.pointer_hover_pos().is_some_and(on_screen) {
                        continue;
                    }
                    let scale = match unit {
                        egui::MouseWheelUnit::Point => WHEEL_NOTCH / POINTS_PER_NOTCH,
                        egui::MouseWheelUnit::Line => WHEEL_NOTCH,
                        egui::MouseWheelUnit::Page => WHEEL_NOTCH * 3.0,
                    };
                    // Windows counts a positive horizontal delta as scrolling right.
                    self.wheel += egui::vec2(-delta.x, delta.y) * scale;
                    for (horizontal, amount) in [(false, &mut self.wheel.y), (true, &mut self.wheel.x)] {
                        let whole = amount.trunc().clamp(i16::MIN as f32, i16::MAX as f32);
                        if whole != 0.0 {
                            *amount -= whole;
                            self.shared.send(ToAgent::Wheel { horizontal, delta: whole as i16 });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for Session {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.upload_frame(&ctx);

        let (connected, status, cursor) = {
            let state = self.shared.ui.lock().unwrap();
            (state.connected, state.status.clone(), state.cursor)
        };
        let state = if connected {
            "Connected"
        } else if status.contains("Reconnecting") {
            "Reconnecting"
        } else {
            "Not connected"
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{} - {state} - Remote Access", self.name)));

        if !self.fullscreen {
            egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        }
        self.file_transfer_ui(ui);
        let panel = egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ui, |ui| {
            let area = ui.max_rect();
            let image = match &self.texture {
                Some(texture) => {
                    let [w, h] = texture.size();
                    fit_rect(area.width() as i32, area.height() as i32, w as i32, h as i32)
                }
                None => (0, 0, 0, 0),
            };
            if let Some(texture) = &self.texture {
                let (x, y, w, h) = image;
                let rect = egui::Rect::from_min_size(area.min + egui::vec2(x as f32, y as f32), egui::vec2(w as f32, h as f32));
                ui.painter().image(texture.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
            if !connected {
                let text_rect = area.shrink(24.0);
                ui.painter().text(
                    text_rect.min,
                    egui::Align2::LEFT_TOP,
                    &status,
                    egui::FontId::proportional(16.0),
                    egui::Color32::WHITE,
                );
            }
            let hovered = ctx.pointer_hover_pos().is_some_and(|p| area.contains(p));
            if hovered && connected {
                ctx.set_cursor_icon(cursor_icon(cursor));
            }
            (area, ui.layer_id(), image)
        });
        let (area, layer, image) = panel.inner;
        // Always called, even unfocused, so it sees the window getting focus back.
        self.forward_input(&ctx, area, layer, image);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.release_all();
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} bytes") } else { format!("{value:.1} {}", UNITS[unit]) }
}
