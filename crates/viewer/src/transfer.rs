//! File transfers for one session, shared by the Windows and macOS session windows.
//!
//! Event driven: work advances when the window queues something or a reply
//! arrives from the agent, so it needs no thread of its own. Every method can be
//! called from any thread. One transfer runs at a time; the rest wait in a queue.
//!
//! Downloads are written to `<target>.part` and renamed when complete.

use rd_common::proto::{DirEntry, ToAgent, ToViewer, FILE_CHUNK, FILE_WINDOW};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// What the window shows. Cheap to take as often as it repaints.
#[derive(Clone, Debug, Default)]
pub struct TransferState {
    /// The agent said it supports file transfer. False while disconnected.
    pub supported: bool,
    pub current: Option<Progress>,
    /// Transfers waiting behind the current one.
    pub queued: usize,
}

#[derive(Clone, Debug)]
pub struct Progress {
    /// Pass to `cancel`.
    pub id: u32,
    pub name: String,
    pub upload: bool,
    pub done: u64,
    /// None until the agent has said how big a download is.
    pub total: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Finished {
    pub name: String,
    pub upload: bool,
    /// Ok holds where the file was saved: on the remote computer for uploads,
    /// on this one for downloads.
    pub result: Result<String, String>,
}

/// A folder listing from the agent, matched to `list_dir`'s return value by `request`.
#[derive(Clone, Debug)]
pub struct Listing {
    pub request: u32,
    pub path: String,
    /// None at the starting places. Some("") goes back to them.
    pub parent: Option<String>,
    pub entries: Vec<DirEntry>,
    pub error: Option<String>,
}

enum Job {
    Upload { id: u32, local: PathBuf, dir: String },
    Download { id: u32, remote: String, local: PathBuf },
}

enum Active {
    Upload { id: u32, name: String, file: File, size: u64, sent: u64, acked: u64, ended: bool },
    Download { id: u32, name: String, local: PathBuf, part: PathBuf, file: Option<File>, size: Option<u64>, received: u64 },
}

impl Active {
    fn id(&self) -> u32 {
        match self {
            Active::Upload { id, .. } | Active::Download { id, .. } => *id,
        }
    }
}

#[derive(Default)]
struct Inner {
    next_id: u32,
    supported: bool,
    queue: VecDeque<Job>,
    active: Option<Active>,
    listings: Vec<Listing>,
    finished: Vec<Finished>,
}

pub struct Transfers {
    inner: Mutex<Inner>,
    tx: UnboundedSender<ToAgent>,
    repaint: Repaint,
}

impl Transfers {
    /// `repaint` is called after anything the window shows has changed, from whichever thread changed it.
    pub fn new(tx: UnboundedSender<ToAgent>, repaint: Repaint) -> Self {
        Transfers { inner: Mutex::new(Inner::default()), tx, repaint }
    }

    pub fn snapshot(&self) -> TransferState {
        let inner = self.inner.lock().unwrap();
        let current = inner.active.as_ref().map(|active| match active {
            Active::Upload { id, name, size, acked, .. } => {
                Progress { id: *id, name: name.clone(), upload: true, done: *acked, total: Some(*size) }
            }
            Active::Download { id, name, size, received, .. } => {
                Progress { id: *id, name: name.clone(), upload: false, done: *received, total: *size }
            }
        });
        TransferState { supported: inner.supported, current, queued: inner.queue.len() }
    }

    /// Listings that have arrived since the last call.
    pub fn take_listings(&self) -> Vec<Listing> {
        std::mem::take(&mut self.inner.lock().unwrap().listings)
    }

    /// Transfers that have ended since the last call, successfully or not.
    pub fn take_finished(&self) -> Vec<Finished> {
        std::mem::take(&mut self.inner.lock().unwrap().finished)
    }

    /// Asks for a folder's contents. An empty path lists the starting places.
    pub fn list_dir(&self, path: &str) -> u32 {
        let request = self.inner.lock().unwrap().new_id();
        let _ = self.tx.send(ToAgent::ListDir { request, path: path.to_string() });
        request
    }

    /// Queues files to send into `dir` on the remote computer. Folders are
    /// skipped, and the Err names them; the files are still sent.
    pub fn upload(&self, files: Vec<PathBuf>, dir: &str) -> Result<(), String> {
        let mut skipped = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            for local in files {
                if local.is_dir() {
                    skipped.push(display_name(&local.to_string_lossy()));
                    continue;
                }
                let id = inner.new_id();
                inner.queue.push_back(Job::Upload { id, local, dir: dir.to_string() });
            }
            self.pump(&mut inner);
        }
        (self.repaint)();
        if skipped.is_empty() {
            Ok(())
        } else {
            Err(format!("Folders can't be sent, only files. Skipped: {}", skipped.join(", ")))
        }
    }

    /// Queues a remote file to save at `local`, replacing any file already there.
    pub fn download(&self, remote: &str, local: PathBuf) {
        {
            let mut inner = self.inner.lock().unwrap();
            let id = inner.new_id();
            inner.queue.push_back(Job::Download { id, remote: remote.to_string(), local });
            self.pump(&mut inner);
        }
        (self.repaint)();
    }

    /// Stops a running or queued transfer.
    pub fn cancel(&self, id: u32) {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(index) = inner.queue.iter().position(|job| job_id(job) == id) {
                let job = inner.queue.remove(index).expect("index is in range");
                let (name, upload) = job_name(&job);
                inner.finished.push(Finished { name, upload, result: Err("cancelled".to_string()) });
            } else if inner.active.as_ref().is_some_and(|a| a.id() == id) {
                let _ = self.tx.send(ToAgent::TransferCancel { id });
                finish(&mut inner, Err("cancelled".to_string()));
                self.pump(&mut inner);
            }
        }
        (self.repaint)();
    }

    /// Handles file messages from the agent. Returns false for any other message.
    pub fn on_message(&self, msg: &ToViewer) -> bool {
        {
            let mut inner = self.inner.lock().unwrap();
            match msg {
                ToViewer::Features { file_transfer } => inner.supported = *file_transfer,
                ToViewer::DirListing { request, path, parent, entries, error } => inner.listings.push(Listing {
                    request: *request,
                    path: path.clone(),
                    parent: parent.clone(),
                    entries: entries.clone(),
                    error: error.clone(),
                }),
                ToViewer::UploadAck { id, received } => {
                    if let Some(Active::Upload { id: current, acked, .. }) = &mut inner.active {
                        if current == id {
                            *acked = *received;
                        }
                    }
                }
                ToViewer::DownloadInfo { id, size, .. } => self.download_started(&mut inner, *id, *size),
                ToViewer::DownloadData { id, data } => self.download_data(&mut inner, *id, data),
                ToViewer::TransferDone { id, result } => {
                    if inner.active.as_ref().is_some_and(|a| a.id() == *id) {
                        let download = matches!(inner.active, Some(Active::Download { .. }));
                        let result = if download && result.is_ok() { complete_download(&mut inner) } else { result.clone() };
                        finish(&mut inner, result);
                    }
                }
                _ => return false,
            }
            self.pump(&mut inner);
        }
        (self.repaint)();
        true
    }

    /// Ends everything: the agent forgets transfers when the connection drops.
    pub fn on_disconnect(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.supported = false;
            if inner.active.is_some() {
                finish(&mut inner, Err("connection lost".to_string()));
            }
            while let Some(job) = inner.queue.pop_front() {
                let (name, upload) = job_name(&job);
                inner.finished.push(Finished { name, upload, result: Err("connection lost".to_string()) });
            }
            inner.listings.clear();
        }
        (self.repaint)();
    }

    fn download_started(&self, inner: &mut Inner, id: u32, size: u64) {
        let Some(Active::Download { id: current, part, file, size: known, .. }) = &mut inner.active else { return };
        if *current != id {
            return;
        }
        match File::create(&*part) {
            Ok(created) => {
                *file = Some(created);
                *known = Some(size);
            }
            Err(e) => {
                let message = format!("couldn't create {}: {e}", part.display());
                let _ = self.tx.send(ToAgent::TransferCancel { id });
                finish(inner, Err(message));
            }
        }
    }

    fn download_data(&self, inner: &mut Inner, id: u32, data: &[u8]) {
        let Some(Active::Download { id: current, part, file: Some(file), received, .. }) = &mut inner.active else {
            return;
        };
        if *current != id {
            return;
        }
        match file.write_all(data) {
            Ok(()) => {
                *received += data.len() as u64;
                let _ = self.tx.send(ToAgent::TransferAck { id, received: *received });
            }
            Err(e) => {
                let message = format!("couldn't write {}: {e}", part.display());
                let _ = self.tx.send(ToAgent::TransferCancel { id });
                finish(inner, Err(message));
            }
        }
    }

    /// Starts queued work and sends upload data while the window has room.
    fn pump(&self, inner: &mut Inner) {
        loop {
            if inner.active.is_none() {
                let Some(job) = inner.queue.pop_front() else { return };
                self.start(inner, job);
                continue;
            }
            if let Some(Active::Upload { .. }) = inner.active {
                self.send_upload_data(inner);
            }
            if inner.active.is_some() {
                return;
            }
        }
    }

    fn start(&self, inner: &mut Inner, job: Job) {
        match job {
            Job::Upload { id, local, dir } => {
                let name = display_name(&local.to_string_lossy());
                let opened = File::open(&local).and_then(|f| f.metadata().map(|m| (f, m.len())));
                match opened {
                    Ok((file, size)) => {
                        let _ = self.tx.send(ToAgent::UploadBegin { id, dir, name: name.clone(), size });
                        inner.active = Some(Active::Upload { id, name, file, size, sent: 0, acked: 0, ended: false });
                    }
                    Err(e) => inner.finished.push(Finished {
                        name,
                        upload: true,
                        result: Err(format!("couldn't open {}: {e}", local.display())),
                    }),
                }
            }
            Job::Download { id, remote, local } => {
                let _ = self.tx.send(ToAgent::DownloadBegin { id, path: remote.clone() });
                let part = part_path(&local);
                inner.active = Some(Active::Download {
                    id,
                    name: display_name(&remote),
                    local,
                    part,
                    file: None,
                    size: None,
                    received: 0,
                });
            }
        }
    }

    fn send_upload_data(&self, inner: &mut Inner) {
        let Some(Active::Upload { id, file, size, sent, acked, ended, .. }) = &mut inner.active else { return };
        let id = *id;
        while !*ended {
            if *sent >= *size {
                let _ = self.tx.send(ToAgent::UploadEnd { id });
                *ended = true;
            } else if *sent - *acked < FILE_WINDOW {
                let want = FILE_CHUNK.min((*size - *sent) as usize);
                let mut data = vec![0u8; want];
                if let Err(e) = file.read_exact(&mut data) {
                    let _ = self.tx.send(ToAgent::TransferCancel { id });
                    finish(inner, Err(format!("couldn't read the file: {e}")));
                    return;
                }
                *sent += want as u64;
                let _ = self.tx.send(ToAgent::UploadData { id, data });
            } else {
                return;
            }
        }
    }
}

impl Inner {
    fn new_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }
}

/// Ends the active transfer with `result`, removing a partial download.
fn finish(inner: &mut Inner, result: Result<String, String>) {
    let Some(active) = inner.active.take() else { return };
    let (name, upload) = match active {
        Active::Upload { name, .. } => (name, true),
        Active::Download { name, part, file, .. } => {
            drop(file);
            if result.is_err() {
                let _ = fs::remove_file(&part);
            }
            (name, false)
        }
    };
    inner.finished.push(Finished { name, upload, result });
}

/// The agent has sent everything: checks it all arrived and moves it into place.
fn complete_download(inner: &mut Inner) -> Result<String, String> {
    let Some(Active::Download { local, part, file, size, received, .. }) = &mut inner.active else {
        return Err("no download is running".to_string());
    };
    let Some(mut file) = file.take() else { return Err("the computer sent no file".to_string()) };
    if Some(*received) != *size {
        return Err(format!("only {received} of {} bytes arrived", size.unwrap_or(0)));
    }
    file.flush().and_then(|_| file.sync_all()).map_err(|e| format!("couldn't save {}: {e}", local.display()))?;
    drop(file);
    // The user chose this name in a save dialog, which already asked about replacing it.
    if local.exists() {
        fs::remove_file(&*local).map_err(|e| format!("couldn't replace {}: {e}", local.display()))?;
    }
    fs::rename(&*part, &*local).map_err(|e| format!("couldn't save {}: {e}", local.display()))?;
    Ok(local.display().to_string())
}

fn job_id(job: &Job) -> u32 {
    match job {
        Job::Upload { id, .. } | Job::Download { id, .. } => *id,
    }
}

fn job_name(job: &Job) -> (String, bool) {
    match job {
        Job::Upload { local, .. } => (display_name(&local.to_string_lossy()), true),
        Job::Download { remote, .. } => (display_name(remote), false),
    }
}

/// The last part of a path from either platform.
pub fn display_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).to_string()
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdviewer-transfer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup() -> (Transfers, UnboundedReceiver<ToAgent>) {
        let (tx, rx) = unbounded_channel();
        (Transfers::new(tx, Arc::new(|| {})), rx)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn upload_stays_within_the_window_and_sends_every_byte() {
        let dir = temp_dir("upload");
        let local = dir.join("big.bin");
        let content = pattern(FILE_WINDOW as usize * 2 + 12345);
        fs::write(&local, &content).unwrap();
        let (transfers, mut rx) = setup();

        transfers.upload(vec![local, dir.clone()], "C:\\Remote").unwrap_err();
        let mut received = Vec::new();
        let mut in_flight = 0u64;
        let mut id = 0;
        loop {
            let Ok(msg) = rx.try_recv() else {
                // The agent acknowledges what it has written so far.
                in_flight = 0;
                transfers.on_message(&ToViewer::UploadAck { id, received: received.len() as u64 });
                continue;
            };
            match msg {
                ToAgent::UploadBegin { id: begun, dir, name, size } => {
                    assert_eq!((dir.as_str(), name.as_str(), size), ("C:\\Remote", "big.bin", content.len() as u64));
                    id = begun;
                }
                ToAgent::UploadData { data, .. } => {
                    in_flight += data.len() as u64;
                    assert!(in_flight <= FILE_WINDOW);
                    received.extend_from_slice(&data);
                }
                ToAgent::UploadEnd { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(received, content);
        transfers.on_message(&ToViewer::TransferDone { id, result: Ok("C:\\Remote\\big.bin".into()) });
        let finished = transfers.take_finished();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].result.as_deref(), Ok("C:\\Remote\\big.bin"));
        assert!(transfers.snapshot().current.is_none());
    }

    #[test]
    fn download_is_saved_only_when_complete() {
        let dir = temp_dir("download");
        let local = dir.join("copy.bin");
        fs::write(&local, b"old contents").unwrap();
        let content = pattern(FILE_CHUNK * 3 + 7);
        let (transfers, mut rx) = setup();

        transfers.download("/Users/me/Desktop/photo.bin", local.clone());
        let Ok(ToAgent::DownloadBegin { id, path }) = rx.try_recv() else { panic!("no DownloadBegin") };
        assert_eq!(path, "/Users/me/Desktop/photo.bin");
        transfers.on_message(&ToViewer::DownloadInfo { id, name: "photo.bin".into(), size: content.len() as u64 });
        for chunk in content.chunks(FILE_CHUNK) {
            transfers.on_message(&ToViewer::DownloadData { id, data: chunk.to_vec() });
            assert!(matches!(rx.try_recv(), Ok(ToAgent::TransferAck { .. })));
        }
        let progress = transfers.snapshot().current.unwrap();
        assert_eq!((progress.name.as_str(), progress.done), ("photo.bin", content.len() as u64));
        assert_eq!(fs::read(&local).unwrap(), b"old contents");

        transfers.on_message(&ToViewer::TransferDone { id, result: Ok(path) });
        assert_eq!(fs::read(&local).unwrap(), content);
        assert!(!part_path(&local).exists());
        assert!(transfers.take_finished()[0].result.is_ok());
    }

    #[test]
    fn short_download_and_lost_connection_leave_no_partial_file() {
        let dir = temp_dir("short");
        let local = dir.join("file.bin");
        let (transfers, mut rx) = setup();

        transfers.download("C:\\a\\file.bin", local.clone());
        let Ok(ToAgent::DownloadBegin { id, .. }) = rx.try_recv() else { panic!() };
        transfers.on_message(&ToViewer::DownloadInfo { id, name: "file.bin".into(), size: 100 });
        transfers.on_message(&ToViewer::DownloadData { id, data: vec![1; 40] });
        transfers.on_message(&ToViewer::TransferDone { id, result: Ok(String::new()) });
        assert!(transfers.take_finished()[0].result.as_ref().unwrap_err().contains("40 of 100"));
        assert!(!local.exists() && !part_path(&local).exists());

        transfers.download("C:\\a\\file.bin", local.clone());
        transfers.download("C:\\a\\other.bin", dir.join("other.bin"));
        let _ = rx.try_recv();
        let id = transfers.snapshot().current.unwrap().id;
        transfers.on_message(&ToViewer::DownloadInfo { id, name: "file.bin".into(), size: 100 });
        transfers.on_message(&ToViewer::DownloadData { id, data: vec![1; 40] });
        assert!(part_path(&local).exists());
        transfers.on_disconnect();
        assert!(!part_path(&local).exists());
        let finished = transfers.take_finished();
        assert_eq!(finished.len(), 2);
        assert!(finished.iter().all(|f| f.result == Err("connection lost".to_string())));
        assert!(!transfers.snapshot().supported);
    }

    #[test]
    fn cancel_moves_on_to_the_next_job_and_ignores_late_replies() {
        let dir = temp_dir("cancel");
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        fs::write(&a, b"aaa").unwrap();
        fs::write(&b, b"bbb").unwrap();
        let (transfers, mut rx) = setup();

        transfers.upload(vec![a, b], "/tmp").unwrap();
        let first = transfers.snapshot().current.unwrap().id;
        assert_eq!(transfers.snapshot().queued, 1);
        transfers.cancel(first);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(sent.iter().any(|m| matches!(m, ToAgent::TransferCancel { id } if *id == first)));
        let current = transfers.snapshot().current.unwrap();
        assert_eq!(current.name, "b.txt");

        // The agent's answer to the cancelled upload must not end the new one.
        transfers.on_message(&ToViewer::TransferDone { id: first, result: Err("cancelled".into()) });
        assert_eq!(transfers.snapshot().current.unwrap().id, current.id);
    }

    #[test]
    fn names_from_either_platform() {
        assert_eq!(display_name("C:\\Users\\me\\Desktop\\a.txt"), "a.txt");
        assert_eq!(display_name("/Users/me/Desktop/b.txt"), "b.txt");
        assert_eq!(display_name("/Volumes/USB/"), "USB");
    }
}
