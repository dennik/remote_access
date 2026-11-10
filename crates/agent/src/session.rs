//! One viewer session in the console session: streams the screen out and applies
//! input coming in. Runs over the TLS stream in console mode, or the service's
//! named pipe in normal operation.

use crate::capture::{Capturer, Grab};
use crate::diff::{self, Rect};
use crate::files;
use crate::platform::{capture, input, DesktopTracker};
use anyhow::Result;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use rayon::prelude::*;
use rd_common::clipboard;
use rd_common::proto::{read_msg, write_msg, CursorShape, MonitorInfo, MouseButton, Tile, ToAgent, ToViewer, FEATURE_PROBE};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc as tmpsc;

/// Frames the viewer may have unacknowledged. Keeps latency low: on a slow link
/// frames get skipped instead of queueing.
const MAX_IN_FLIGHT: i64 = 2;
/// Capped so a session doesn't compete with the PC's own work.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);
const DEFAULT_QUALITY: u8 = 75;

struct Shared {
    stop: AtomicBool,
    monitor: AtomicU32,
    monitor_changed: AtomicBool,
    full_frame: AtomicBool,
    quality: AtomicU8,
    in_flight: AtomicI64,
    monitors: Mutex<Vec<MonitorInfo>>,
    /// Set just before writing to the clipboard, so that write isn't echoed back.
    clipboard_echo: Mutex<Option<String>>,
}

enum InputEvent {
    Move(u16, u16),
    Button(MouseButton, bool),
    Wheel(bool, i16),
    Key(u16, bool, bool),
}

/// `log` records file transfers in the connection log.
pub async fn run<S>(stream: S, log: files::Log) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let monitors = capture::list_monitors();
    let current = monitors.iter().position(|m| m.primary).unwrap_or(0) as u32;
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        monitor: AtomicU32::new(current),
        monitor_changed: AtomicBool::new(false),
        full_frame: AtomicBool::new(true),
        quality: AtomicU8::new(DEFAULT_QUALITY),
        in_flight: AtomicI64::new(0),
        monitors: Mutex::new(monitors.clone()),
        clipboard_echo: Mutex::new(None),
    });

    let _stop_guard = StopOnDrop(shared.clone());

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (out_tx, mut out_rx) = tmpsc::channel::<ToViewer>(8);
    out_tx.send(ToViewer::Monitors { monitors, current }).await?;

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_msg(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let capture_thread = {
        let (shared, tx) = (shared.clone(), out_tx.clone());
        std::thread::spawn(move || capture_loop(shared, tx))
    };
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
    let input_thread = {
        let shared = shared.clone();
        std::thread::spawn(move || input_loop(shared, input_rx))
    };
    let (clip_tx, clip_rx) = mpsc::channel::<String>();
    let clipboard_thread = {
        let (shared, tx) = (shared.clone(), out_tx.clone());
        std::thread::spawn(move || clipboard_loop(shared, tx, clip_rx))
    };
    let (files_tx, files_rx) = mpsc::channel::<files::Command>();
    let files_thread = {
        let tx = out_tx.clone();
        std::thread::spawn(move || files::worker(files_rx, tx, log))
    };
    drop(out_tx);

    // Runs until the viewer disconnects, or the writer fails because it has.
    let result: Result<()> = loop {
        let msg = match read_msg::<_, ToAgent>(&mut reader).await {
            Ok(msg) => msg,
            Err(e) => break Err(e),
        };
        match msg {
            ToAgent::Ack { .. } => {
                shared.in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            ToAgent::SelectMonitor(FEATURE_PROBE) => drop(files_tx.send(files::Command::Features)),
            ToAgent::SelectMonitor(i) => {
                if (i as usize) < shared.monitors.lock().unwrap().len() {
                    shared.monitor.store(i, Ordering::SeqCst);
                    shared.monitor_changed.store(true, Ordering::SeqCst);
                }
            }
            ToAgent::SetQuality(q) => {
                shared.quality.store(q.clamp(20, 95), Ordering::SeqCst);
                shared.full_frame.store(true, Ordering::SeqCst);
            }
            ToAgent::RequestFullFrame => shared.full_frame.store(true, Ordering::SeqCst),
            ToAgent::MouseMove { x, y } => drop(input_tx.send(InputEvent::Move(x, y))),
            ToAgent::MouseButton { button, down } => drop(input_tx.send(InputEvent::Button(button, down))),
            ToAgent::Wheel { horizontal, delta } => drop(input_tx.send(InputEvent::Wheel(horizontal, delta))),
            ToAgent::Key { scan, extended, down } => drop(input_tx.send(InputEvent::Key(scan, extended, down))),
            ToAgent::Clipboard(text) => drop(clip_tx.send(text)),
            // Sending the secure attention sequence needs the service. Ignored in console mode.
            ToAgent::CtrlAltDel => {}
            ToAgent::ListDir { request, path } => drop(files_tx.send(files::Command::List { request, path })),
            ToAgent::UploadBegin { id, dir, name, size } => {
                drop(files_tx.send(files::Command::UploadBegin { id, dir, name, size }))
            }
            ToAgent::UploadData { id, data } => drop(files_tx.send(files::Command::UploadData { id, data })),
            ToAgent::UploadEnd { id } => drop(files_tx.send(files::Command::UploadEnd { id })),
            ToAgent::DownloadBegin { id, path } => drop(files_tx.send(files::Command::DownloadBegin { id, path })),
            ToAgent::TransferAck { id, received } => drop(files_tx.send(files::Command::DownloadAck { id, received })),
            ToAgent::TransferCancel { id } => drop(files_tx.send(files::Command::Cancel { id })),
        }
    };

    shared.stop.store(true, Ordering::SeqCst);
    // Aborted first: closing the channel frees a thread blocked sending to a stalled viewer.
    writer_task.abort();
    drop(input_tx);
    drop(clip_tx);
    // Closing this makes the worker delete unfinished uploads and exit.
    drop(files_tx);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = files_thread.join();
        let _ = capture_thread.join();
        let _ = input_thread.join();
        let _ = clipboard_thread.join();
    })
    .await;
    result
}

/// Tells the worker threads to finish even if the session future is dropped midway.
struct StopOnDrop(Arc<Shared>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::SeqCst);
    }
}

fn encode_tile(frame: &[u8], frame_width: u32, r: Rect, quality: u8) -> Option<Tile> {
    let pixels = diff::crop(frame, frame_width, r);
    let mut jpeg = Vec::with_capacity(pixels.len() / 8);
    let mut encoder = Encoder::new(&mut jpeg, quality);
    // Full chroma keeps coloured text and thin UI lines readable.
    encoder.set_sampling_factor(SamplingFactor::R_4_4_4);
    encoder.encode(&pixels, r.w as u16, r.h as u16, ColorType::Bgra).ok()?;
    Some(Tile { x: r.x, y: r.y, width: r.w, height: r.h, jpeg })
}

fn capture_loop(shared: Arc<Shared>, tx: tmpsc::Sender<ToViewer>) {
    // At most half the cores: the PC has its own work to do.
    let threads = (num_cpus::get() / 2).max(1);
    let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(threads).build() else { return };

    let mut desktop = DesktopTracker::default();
    let mut capturer: Option<Box<dyn Capturer>> = None;
    let mut previous: Vec<u8> = Vec::new();
    let mut seq = 0u64;
    let mut last_sent = Instant::now() - MIN_FRAME_INTERVAL;
    let mut last_cursor = None::<CursorShape>;

    while !shared.stop.load(Ordering::SeqCst) {
        // A switch to or from the secure desktop invalidates the capture device.
        if desktop.sync() {
            capturer = None;
            shared.full_frame.store(true, Ordering::SeqCst);
        }
        if shared.monitor_changed.swap(false, Ordering::SeqCst) {
            capturer = None;
            previous.clear();
        }

        let cursor = capture::current_cursor();
        if last_cursor != Some(cursor) {
            last_cursor = Some(cursor);
            if tx.blocking_send(ToViewer::Cursor(cursor)).is_err() {
                break;
            }
        }

        if shared.in_flight.load(Ordering::SeqCst) >= MAX_IN_FLIGHT {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let since = last_sent.elapsed();
        if since < MIN_FRAME_INTERVAL {
            std::thread::sleep(MIN_FRAME_INTERVAL - since);
        }

        if capturer.is_none() {
            let monitors = capture::list_monitors();
            let index = shared.monitor.load(Ordering::SeqCst) as usize;
            let Some(monitor) = monitors.get(index).or(monitors.first()).cloned() else {
                // No display attached, for example a PC with no monitor plugged in.
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };
            let mut known = shared.monitors.lock().unwrap();
            if *known != monitors {
                *known = monitors.clone();
                let current = monitors.iter().position(|m| *m == monitor).unwrap_or(0) as u32;
                let _ = tx.blocking_send(ToViewer::Monitors { monitors, current });
            }
            drop(known);
            capturer = Some(capture::create_capturer(&monitor));
        }
        let Some(cap) = capturer.as_mut() else { continue };

        let grabbed = match cap.grab(50) {
            Ok(g) => g,
            Err(_) => {
                // Lost access, usually a resolution change or a desktop switch. Recreate it.
                capturer = None;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let force = shared.full_frame.swap(false, Ordering::SeqCst);
        if matches!(grabbed, Grab::Unchanged) && !force {
            continue;
        }

        let (pixels, width, height) = cap.frame();
        let rects = if force {
            vec![Rect { x: 0, y: 0, w: width, h: height }]
        } else {
            diff::dirty_rects(&previous, pixels, width, height)
        };
        if rects.is_empty() {
            continue;
        }

        let quality = shared.quality.load(Ordering::SeqCst);
        let tiles: Vec<Tile> =
            pool.install(|| rects.par_iter().filter_map(|r| encode_tile(pixels, width, *r, quality)).collect());
        previous.clear();
        previous.extend_from_slice(pixels);

        seq += 1;
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        last_sent = Instant::now();
        if tx.blocking_send(ToViewer::Frame { seq, width, height, tiles }).is_err() {
            break;
        }
    }
}

fn input_loop(shared: Arc<Shared>, rx: mpsc::Receiver<InputEvent>) {
    let mut desktop = DesktopTracker::default();
    let mut keys_down: HashSet<(u16, bool)> = HashSet::new();
    let mut buttons_down: Vec<MouseButton> = Vec::new();

    while !shared.stop.load(Ordering::SeqCst) {
        let event = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        desktop.sync();
        match &event {
            InputEvent::Key(scan, extended, true) => drop(keys_down.insert((*scan, *extended))),
            InputEvent::Key(scan, extended, false) => drop(keys_down.remove(&(*scan, *extended))),
            InputEvent::Button(button, true) if !buttons_down.contains(button) => buttons_down.push(*button),
            InputEvent::Button(button, false) => buttons_down.retain(|b| b != button),
            _ => {}
        }
        match event {
            InputEvent::Move(x, y) => {
                let monitor = {
                    let monitors = shared.monitors.lock().unwrap();
                    monitors.get(shared.monitor.load(Ordering::SeqCst) as usize).cloned()
                };
                if let Some(monitor) = monitor {
                    input::mouse_move(x, y, &monitor);
                }
            }
            InputEvent::Button(button, down) => input::mouse_button(button, down),
            InputEvent::Wheel(horizontal, delta) => input::wheel(horizontal, delta),
            InputEvent::Key(scan, extended, down) => input::key(scan, extended, down),
        }
    }

    // A dropped connection must not leave Ctrl or a mouse button held on the machine.
    desktop.sync();
    for (scan, extended) in keys_down {
        input::key(scan, extended, false);
    }
    for button in buttons_down {
        input::mouse_button(button, false);
    }
}

fn clipboard_loop(shared: Arc<Shared>, tx: tmpsc::Sender<ToViewer>, rx: mpsc::Receiver<String>) {
    let mut seen = clipboard::sequence_number();
    while !shared.stop.load(Ordering::SeqCst) {
        if let Ok(text) = rx.recv_timeout(Duration::from_millis(400)) {
            *shared.clipboard_echo.lock().unwrap() = Some(text.clone());
            clipboard::set_text(&text);
        }
        let now = clipboard::sequence_number();
        if now != seen {
            seen = now;
            if let Some(text) = clipboard::get_text() {
                let echo = shared.clipboard_echo.lock().unwrap().take();
                if echo.as_deref() != Some(text.as_str()) && tx.blocking_send(ToViewer::Clipboard(text)).is_err() {
                    break;
                }
            }
        }
    }
}
