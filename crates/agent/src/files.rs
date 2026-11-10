//! File transfer for one session: folder listings, uploads from the viewer and
//! downloads to it. Runs on its own thread so disk access never holds up the
//! screen or input.
//!
//! Uploads are written to `<name>.part` and renamed when complete. Existing
//! files are never overwritten: a clash gets a numbered name instead.

use crate::platform;
use rd_common::proto::{DirEntry, ToViewer, FILE_CHUNK, FILE_WINDOW};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc as tmpsc;

/// More entries than this in one folder are left out of the listing.
const MAX_ENTRIES: usize = 5000;

pub enum Command {
    /// The viewer asked which features this agent has.
    Features,
    List { request: u32, path: String },
    UploadBegin { id: u32, dir: String, name: String, size: u64 },
    UploadData { id: u32, data: Vec<u8> },
    UploadEnd { id: u32 },
    DownloadBegin { id: u32, path: String },
    DownloadAck { id: u32, received: u64 },
    Cancel { id: u32 },
}

pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

struct Upload {
    file: File,
    part: PathBuf,
    target: PathBuf,
    expected: u64,
    written: u64,
}

struct Download {
    file: File,
    path: PathBuf,
    size: u64,
    sent: u64,
    acked: u64,
}

/// Runs until the session's command channel closes. Unfinished uploads are deleted.
pub fn worker(commands: Receiver<Command>, out: tmpsc::Sender<ToViewer>, log: Log) {
    let mut uploads: HashMap<u32, Upload> = HashMap::new();
    let mut downloads: HashMap<u32, Download> = HashMap::new();
    let send = |msg: ToViewer| out.blocking_send(msg).is_ok();

    'session: loop {
        // Idle waits are short only while a download has room to send more.
        let busy = downloads.values().any(|d| d.sent < d.size && d.sent - d.acked < FILE_WINDOW);
        let timeout = if busy { Duration::ZERO } else { Duration::from_millis(200) };
        match commands.recv_timeout(timeout) {
            Ok(command) => {
                if !handle(command, &mut uploads, &mut downloads, &send, &log) {
                    break 'session;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 'session,
        }

        let mut finished = Vec::new();
        for (&id, d) in downloads.iter_mut() {
            while d.sent < d.size && d.sent - d.acked < FILE_WINDOW {
                let want = FILE_CHUNK.min((d.size - d.sent) as usize);
                let mut data = vec![0u8; want];
                let result = d.file.read_exact(&mut data);
                if let Err(e) = result {
                    finished.push((id, Err(format!("couldn't read {}: {e}", d.path.display()))));
                    break;
                }
                d.sent += want as u64;
                if !send(ToViewer::DownloadData { id, data }) {
                    break 'session;
                }
            }
            if d.sent >= d.size && d.acked >= d.size {
                finished.push((id, Ok(d.path.display().to_string())));
            }
        }
        for (id, result) in finished {
            if let Some(d) = downloads.remove(&id) {
                log(&match &result {
                    Ok(_) => format!("sent file {} ({} bytes)", d.path.display(), d.size),
                    Err(e) => format!("download failed: {e}"),
                });
            }
            if !send(ToViewer::TransferDone { id, result }) {
                break 'session;
            }
        }
    }

    for (_, upload) in uploads.drain() {
        drop(upload.file);
        let _ = fs::remove_file(&upload.part);
        log(&format!("upload to {} stopped before finishing", upload.target.display()));
    }
}

/// Returns false when the viewer is gone.
fn handle(
    command: Command,
    uploads: &mut HashMap<u32, Upload>,
    downloads: &mut HashMap<u32, Download>,
    send: &dyn Fn(ToViewer) -> bool,
    log: &Log,
) -> bool {
    match command {
        Command::Features => send(ToViewer::Features { file_transfer: true }),
        Command::List { request, path } => {
            let (parent, entries, error) = if path.is_empty() {
                (None, platform::files::places(), None)
            } else {
                match list(Path::new(&path)) {
                    Ok(entries) => (Some(parent_of(Path::new(&path))), entries, None),
                    Err(e) => (Some(parent_of(Path::new(&path))), Vec::new(), Some(e)),
                }
            };
            send(ToViewer::DirListing { request, path, parent, entries, error })
        }
        Command::UploadBegin { id, dir, name, size } => match begin_upload(&dir, &name, size) {
            Ok(upload) => {
                uploads.insert(id, upload);
                true
            }
            Err(e) => send(ToViewer::TransferDone { id, result: Err(e) }),
        },
        Command::UploadData { id, data } => {
            let Some(upload) = uploads.get_mut(&id) else { return true };
            let too_long = upload.written + data.len() as u64 > upload.expected;
            let result = if too_long {
                Err("received more data than the file's size".to_string())
            } else {
                upload.file.write_all(&data).map_err(|e| format!("couldn't write {}: {e}", upload.part.display()))
            };
            match result {
                Ok(()) => {
                    upload.written += data.len() as u64;
                    send(ToViewer::UploadAck { id, received: upload.written })
                }
                Err(e) => {
                    abandon(uploads.remove(&id));
                    log(&format!("upload failed: {e}"));
                    send(ToViewer::TransferDone { id, result: Err(e) })
                }
            }
        }
        Command::UploadEnd { id } => {
            let Some(upload) = uploads.remove(&id) else { return true };
            let result = finish_upload(upload);
            log(&match &result {
                Ok(path) => format!("received file {path}"),
                Err(e) => format!("upload failed: {e}"),
            });
            send(ToViewer::TransferDone { id, result })
        }
        Command::DownloadBegin { id, path } => {
            let path = PathBuf::from(path);
            let opened = File::open(&path).and_then(|f| f.metadata().map(|m| (f, m)));
            match opened {
                Ok((file, meta)) if meta.is_file() => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let size = meta.len();
                    downloads.insert(id, Download { file, path, size, sent: 0, acked: 0 });
                    send(ToViewer::DownloadInfo { id, name, size })
                }
                Ok(_) => send(ToViewer::TransferDone { id, result: Err(format!("{} is not a file", path.display())) }),
                Err(e) => send(ToViewer::TransferDone { id, result: Err(format!("couldn't open {}: {e}", path.display())) }),
            }
        }
        Command::DownloadAck { id, received } => {
            if let Some(d) = downloads.get_mut(&id) {
                d.acked = received.min(d.sent);
            }
            true
        }
        Command::Cancel { id } => {
            abandon(uploads.remove(&id));
            downloads.remove(&id);
            send(ToViewer::TransferDone { id, result: Err("cancelled".to_string()) })
        }
    }
}

fn abandon(upload: Option<Upload>) {
    if let Some(upload) = upload {
        drop(upload.file);
        let _ = fs::remove_file(&upload.part);
    }
}

/// An empty string means "go back to the starting places".
fn parent_of(path: &Path) -> String {
    path.parent().map(|p| p.display().to_string()).unwrap_or_default()
}

fn list(path: &Path) -> Result<Vec<DirEntry>, String> {
    let read = fs::read_dir(path).map_err(|e| format!("couldn't open {}: {e}", path.display()))?;
    let mut entries: Vec<DirEntry> = read
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if platform::files::is_hidden(&entry.file_name(), &meta) {
                return None;
            }
            Some(DirEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().display().to_string(),
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
            })
        })
        .take(MAX_ENTRIES)
        .collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

/// Keeps only a plain file name that's valid on both Windows and macOS.
pub fn sanitize_name(name: &str) -> Result<String, String> {
    // The viewer sends a bare name, but never trust it to be one.
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    // Windows drops trailing dots and spaces, which would change the name.
    let cleaned = cleaned.trim_end_matches(['.', ' ']).trim_start().to_string();
    if cleaned.is_empty() {
        return Err(format!("\"{name}\" isn't a usable file name"));
    }
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    Ok(if reserved { format!("_{cleaned}") } else { cleaned })
}

/// `name.ext`, else `name (2).ext`, `name (3).ext` and so on.
pub fn unused_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() && !part_of(&first).exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists() && !part_of(p).exists())
        .expect("some numbered name is free")
}

fn part_of(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

fn begin_upload(dir: &str, name: &str, size: u64) -> Result<Upload, String> {
    let dir = Path::new(dir);
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let name = sanitize_name(name)?;
    let target = unused_path(dir, &name);
    let part = part_of(&target);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(|e| format!("couldn't create a file in {}: {e}", dir.display()))?;
    Ok(Upload { file, part, target, expected: size, written: 0 })
}

fn finish_upload(upload: Upload) -> Result<String, String> {
    let Upload { mut file, part, target, expected, written } = upload;
    let flushed = file.flush().and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = flushed {
        let _ = fs::remove_file(&part);
        return Err(format!("couldn't save {}: {e}", target.display()));
    }
    if written != expected {
        let _ = fs::remove_file(&part);
        return Err(format!("only {written} of {expected} bytes arrived"));
    }
    // Another file may have taken the name meanwhile, so check again.
    let target = if target.exists() {
        let name = target.file_name().unwrap_or_default().to_string_lossy().to_string();
        unused_path(target.parent().unwrap_or(Path::new("")), &name)
    } else {
        target
    };
    fs::rename(&part, &target).map_err(|e| {
        let _ = fs::remove_file(&part);
        format!("couldn't save {}: {e}", target.display())
    })?;
    Ok(target.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdagent-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_are_reduced_to_a_safe_file_name() {
        assert_eq!(sanitize_name("report.pdf").unwrap(), "report.pdf");
        assert_eq!(sanitize_name("..\\..\\Windows\\evil.dll").unwrap(), "evil.dll");
        assert_eq!(sanitize_name("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(sanitize_name("a:b?c.txt").unwrap(), "a_b_c.txt");
        assert_eq!(sanitize_name("CON.txt").unwrap(), "_CON.txt");
        assert_eq!(sanitize_name("com1").unwrap(), "_com1");
        assert_eq!(sanitize_name("notes. ").unwrap(), "notes");
        assert!(sanitize_name("..").is_err());
        assert!(sanitize_name("dir/").is_err());
    }

    #[test]
    fn existing_files_get_numbered_names() {
        let dir = temp_dir("names");
        assert_eq!(unused_path(&dir, "a.txt"), dir.join("a.txt"));
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::write(dir.join("a (2).txt"), "x").unwrap();
        assert_eq!(unused_path(&dir, "a.txt"), dir.join("a (3).txt"));
        fs::write(dir.join("README"), "x").unwrap();
        assert_eq!(unused_path(&dir, "README"), dir.join("README (2)"));
        fs::write(dir.join(".env"), "x").unwrap();
        assert_eq!(unused_path(&dir, ".env"), dir.join(".env (2)"));
    }

    struct Harness {
        commands: mpsc::Sender<Command>,
        replies: tmpsc::Receiver<ToViewer>,
        thread: std::thread::JoinHandle<()>,
    }

    fn start() -> Harness {
        let (commands, rx) = mpsc::channel();
        let (tx, replies) = tmpsc::channel(64);
        let thread = std::thread::spawn(move || worker(rx, tx, Arc::new(|_: &str| {})));
        Harness { commands, replies, thread }
    }

    fn next(h: &mut Harness) -> ToViewer {
        h.replies.blocking_recv().expect("worker replied")
    }

    #[test]
    fn upload_lands_complete_and_never_overwrites() {
        let dir = temp_dir("upload");
        fs::write(dir.join("photo.jpg"), "old").unwrap();
        let mut h = start();
        let body: Vec<u8> = (0..FILE_CHUNK * 2 + 10).map(|i| i as u8).collect();
        let d = dir.display().to_string();
        h.commands.send(Command::UploadBegin { id: 1, dir: d, name: "photo.jpg".into(), size: body.len() as u64 }).unwrap();
        for chunk in body.chunks(FILE_CHUNK) {
            h.commands.send(Command::UploadData { id: 1, data: chunk.to_vec() }).unwrap();
            assert!(matches!(next(&mut h), ToViewer::UploadAck { id: 1, .. }));
        }
        h.commands.send(Command::UploadEnd { id: 1 }).unwrap();
        let ToViewer::TransferDone { id: 1, result: Ok(saved) } = next(&mut h) else { panic!("upload failed") };
        assert_eq!(PathBuf::from(&saved), dir.join("photo (2).jpg"));
        assert_eq!(fs::read(&saved).unwrap(), body);
        assert_eq!(fs::read(dir.join("photo.jpg")).unwrap(), b"old");
        assert!(!dir.join("photo (2).jpg.part").exists());
        drop(h.commands);
        h.thread.join().unwrap();
    }

    #[test]
    fn short_or_abandoned_uploads_leave_nothing_behind() {
        let dir = temp_dir("short");
        let mut h = start();
        let d = dir.display().to_string();
        h.commands.send(Command::UploadBegin { id: 1, dir: d.clone(), name: "a.bin".into(), size: 100 }).unwrap();
        h.commands.send(Command::UploadData { id: 1, data: vec![1; 40] }).unwrap();
        assert!(matches!(next(&mut h), ToViewer::UploadAck { id: 1, received: 40 }));
        h.commands.send(Command::UploadEnd { id: 1 }).unwrap();
        assert!(matches!(next(&mut h), ToViewer::TransferDone { id: 1, result: Err(_) }));

        h.commands.send(Command::UploadBegin { id: 2, dir: d, name: "b.bin".into(), size: 100 }).unwrap();
        h.commands.send(Command::UploadData { id: 2, data: vec![1; 40] }).unwrap();
        assert!(matches!(next(&mut h), ToViewer::UploadAck { id: 2, .. }));
        // The viewer disconnects mid-upload.
        drop(h.commands);
        h.thread.join().unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "partial files were left in {}", dir.display());
    }

    #[test]
    fn download_respects_the_window_and_completes_after_acks() {
        let dir = temp_dir("download");
        let body: Vec<u8> = (0..FILE_WINDOW as usize + FILE_CHUNK * 3 + 7).map(|i| (i * 7) as u8).collect();
        let path = dir.join("video.mp4");
        fs::write(&path, &body).unwrap();
        let mut h = start();
        h.commands.send(Command::DownloadBegin { id: 5, path: path.display().to_string() }).unwrap();
        let ToViewer::DownloadInfo { id: 5, name, size } = next(&mut h) else { panic!("no info") };
        assert_eq!((name.as_str(), size), ("video.mp4", body.len() as u64));

        let mut received = Vec::new();
        let mut acked = 0u64;
        loop {
            match next(&mut h) {
                ToViewer::DownloadData { id: 5, data } => {
                    received.extend_from_slice(&data);
                    let total = received.len() as u64;
                    // Never more than one window ahead of what was acknowledged.
                    assert!(total - acked <= FILE_WINDOW, "{} bytes unacknowledged", total - acked);
                    // Acknowledge only when the window is full, to prove the agent waits.
                    if total - acked == FILE_WINDOW || total == size {
                        acked = total;
                        h.commands.send(Command::DownloadAck { id: 5, received: total }).unwrap();
                    }
                }
                ToViewer::TransferDone { id: 5, result } => {
                    assert!(result.is_ok());
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(received, body);
        drop(h.commands);
        h.thread.join().unwrap();
    }

    #[test]
    fn listing_shows_folders_first_and_reports_missing_paths() {
        let dir = temp_dir("list");
        fs::write(dir.join("b.txt"), "12345").unwrap();
        fs::create_dir(dir.join("Zfolder")).unwrap();
        fs::write(dir.join("a.txt"), "1").unwrap();
        let mut h = start();
        h.commands.send(Command::List { request: 9, path: dir.display().to_string() }).unwrap();
        let ToViewer::DirListing { request: 9, entries, error: None, parent: Some(_), .. } = next(&mut h) else {
            panic!("bad listing")
        };
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Zfolder", "a.txt", "b.txt"]);
        assert_eq!(entries[2].size, 5);

        h.commands.send(Command::List { request: 10, path: dir.join("missing").display().to_string() }).unwrap();
        assert!(matches!(next(&mut h), ToViewer::DirListing { request: 10, error: Some(_), .. }));

        h.commands.send(Command::List { request: 11, path: String::new() }).unwrap();
        let ToViewer::DirListing { request: 11, parent: None, entries, .. } = next(&mut h) else { panic!() };
        assert!(!entries.is_empty(), "no starting places");
        drop(h.commands);
        h.thread.join().unwrap();
    }
}
