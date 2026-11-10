//! Connecting to agents: pinning, login, discovery and the diagnostic snapshot.

use crate::framebuffer::Framebuffer;
use crate::store;
use anyhow::{anyhow, bail, Context, Result};
use rd_common::proto::{read_msg, write_msg, AuthMsg, DiscoveryReply, ToAgent, ToViewer};
use rd_common::{auth, tls, DISCOVERY_PORT, DISCOVERY_REQUEST, TCP_PORT};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::client::TlsStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The computer presented a different certificate from the one that was trusted.
#[derive(Debug)]
pub struct FingerprintChanged;

impl std::fmt::Display for FingerprintChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this computer's identity has changed, so the connection was refused. If the agent was reinstalled, remove the computer from the dashboard and add it again. Otherwise something may be impersonating it.")
    }
}
impl std::error::Error for FingerprintChanged {}

/// The agent doesn't have this viewer's key in its authorized list.
#[derive(Debug)]
pub struct NotAuthorized;

impl std::fmt::Display for NotAuthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this computer doesn't authorize this viewer's admin key. Re-run rdagent install with the key shown in the dashboard.")
    }
}
impl std::error::Error for NotAuthorized {}

pub fn with_default_port(addr: &str) -> String {
    let addr = addr.trim();
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{TCP_PORT}")
    }
}

async fn open_tls(addr: &str) -> Result<(TlsStream<TcpStream>, String)> {
    let socket = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("timed out connecting to {addr}"))?
        .with_context(|| format!("connecting to {addr}"))?;
    socket.set_nodelay(true)?;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, tls::connector()?.connect(tls::server_name(), socket))
        .await
        .map_err(|_| anyhow!("timed out during the TLS handshake"))??;
    let cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or_else(|| anyhow!("agent presented no certificate"))?;
    let fingerprint = tls::fingerprint(cert);
    Ok((stream, fingerprint))
}

/// Reads a computer's name and fingerprint. Sends nothing identifying, so it's safe
/// to use on a computer that isn't trusted yet.
pub async fn probe(addr: &str) -> Result<(String, String)> {
    let (mut stream, fingerprint) = open_tls(addr).await?;
    let AuthMsg::Challenge { name, .. } = tokio::time::timeout(CONNECT_TIMEOUT, read_msg(&mut stream)).await?? else {
        bail!("agent sent an unexpected first message");
    };
    Ok((name, fingerprint))
}

/// Connects and logs in. `pin` of None accepts any certificate, for `snapshot` only.
pub async fn connect(addr: &str, pin: Option<&str>, key: &[u8; 32]) -> Result<TlsStream<TcpStream>> {
    let (mut stream, fingerprint) = open_tls(addr).await?;
    // Checked before anything is signed or sent.
    if pin.is_some_and(|p| p != fingerprint) {
        return Err(FingerprintChanged.into());
    }
    let AuthMsg::Challenge { nonce, .. } = tokio::time::timeout(CONNECT_TIMEOUT, read_msg(&mut stream)).await?? else {
        bail!("agent sent an unexpected first message");
    };
    let response = AuthMsg::Response {
        public_key: auth::public_key(key),
        signature: auth::sign(key, &nonce, &fingerprint),
    };
    write_msg(&mut stream, &response).await?;
    match tokio::time::timeout(CONNECT_TIMEOUT, read_msg::<_, AuthMsg>(&mut stream)).await?? {
        AuthMsg::Accepted => Ok(stream),
        _ => Err(NotAuthorized.into()),
    }
}

/// Finds agents on the local subnet by broadcast.
pub async fn discover(wait: Duration) -> Result<Vec<(String, DiscoveryReply)>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await?;
    socket.set_broadcast(true)?;
    socket.send_to(DISCOVERY_REQUEST, ("255.255.255.255", DISCOVERY_PORT)).await?;

    let mut found = HashMap::new();
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 1024];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(Ok((n, from))) = tokio::time::timeout(left, socket.recv_from(&mut buf)).await else { break };
        if let Ok(reply) = postcard::from_bytes::<DiscoveryReply>(&buf[..n]) {
            found.insert(from.ip().to_string(), reply);
        }
    }
    Ok(found.into_iter().map(|(ip, reply)| (format!("{ip}:{}", reply.port), reply)).collect())
}

pub fn snapshot(addr: &str, pin: Option<&str>, duration: Duration, out: Option<&Path>) -> Result<()> {
    let key = store::load_or_create_key()?;
    let addr = with_default_port(addr);
    tokio::runtime::Runtime::new()?.block_on(async {
        let mut stream = connect(&addr, pin, &key).await?;
        let mut fb = Framebuffer::default();
        let (mut frames, mut tiles_total, mut bytes) = (0u64, 0u64, 0u64);
        let start = Instant::now();

        while start.elapsed() < duration || frames == 0 {
            let left = (duration + Duration::from_secs(10)).saturating_sub(start.elapsed());
            let msg = tokio::time::timeout(left, read_msg::<_, ToViewer>(&mut stream))
                .await
                .map_err(|_| anyhow!("no frame arrived"))??;
            match msg {
                ToViewer::Monitors { monitors, current } => {
                    println!("monitors: {} (showing {current})", monitors.len());
                    for m in &monitors {
                        println!("  {} {}x{} at {},{} primary={}", m.name, m.width, m.height, m.x, m.y, m.primary);
                    }
                }
                ToViewer::Frame { seq, width, height, tiles } => {
                    frames += 1;
                    tiles_total += tiles.len() as u64;
                    bytes += tiles.iter().map(|t| t.jpeg.len() as u64).sum::<u64>();
                    let decoded = Framebuffer::decode_tiles(&tiles)?;
                    fb.apply(width, height, &tiles, &decoded)?;
                    write_msg(&mut stream, &ToAgent::Ack { seq }).await?;
                }
                ToViewer::Cursor(shape) => println!("cursor: {shape:?}"),
                ToViewer::Clipboard(text) => println!("clipboard: {} chars", text.chars().count()),
                ToViewer::Features { file_transfer } => println!("file transfer: {file_transfer}"),
                _ => {}
            }
        }

        let opaque = fb.pixels.chunks_exact(4).filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0).count();
        let total = (fb.width as usize * fb.height as usize).max(1);
        println!(
            "frames={frames} tiles={tiles_total} bytes={bytes} size={}x{} non_black_percent={}",
            fb.width,
            fb.height,
            opaque * 100 / total
        );
        if let Some(path) = out {
            std::fs::write(path, bitmap(&fb))?;
            println!("saved {}", path.display());
        }
        Ok(())
    })
}

pub struct FileJob {
    pub list: Option<String>,
    pub put: Vec<std::path::PathBuf>,
    pub dir: Option<String>,
    pub get: Option<String>,
    pub to: Option<std::path::PathBuf>,
}

/// Runs file operations through the same engine as the session windows.
pub fn files(addr: &str, pin: Option<&str>, job: FileJob) -> Result<()> {
    use crate::transfer::Transfers;
    use std::sync::Arc;

    let key = store::load_or_create_key()?;
    let addr = with_default_port(addr);
    tokio::runtime::Runtime::new()?.block_on(async {
        let stream = connect(&addr, pin, &key).await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_msg(&mut writer, &ToAgent::SelectMonitor(rd_common::proto::FEATURE_PROBE)).await?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ToAgent>();
        let changed = Arc::new(tokio::sync::Notify::new());
        let transfers = {
            let changed = changed.clone();
            Arc::new(Transfers::new(tx.clone(), Arc::new(move || changed.notify_one())))
        };
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write_msg(&mut writer, &msg).await.is_err() {
                    break;
                }
            }
        });
        let reading = {
            let transfers = transfers.clone();
            tokio::spawn(async move {
                loop {
                    let msg = read_msg::<_, ToViewer>(&mut reader).await?;
                    if transfers.on_message(&msg) {
                        continue;
                    }
                    if let ToViewer::Frame { seq, .. } = msg {
                        let _ = tx.send(ToAgent::Ack { seq });
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            })
        };

        let wait = |what: &'static str| {
            let (changed, reading) = (changed.clone(), &reading);
            async move {
                tokio::select! {
                    _ = changed.notified() => Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(30)) => Err(anyhow!("timed out waiting for {what}")),
                    _ = async { while !reading.is_finished() { tokio::time::sleep(Duration::from_millis(100)).await } } => {
                        Err(anyhow!("the connection closed"))
                    }
                }
            }
        };

        while !transfers.snapshot().supported {
            wait("the feature list").await?;
        }
        if let Some(path) = job.list {
            let request = transfers.list_dir(&path);
            let listing = loop {
                if let Some(l) = transfers.take_listings().into_iter().find(|l| l.request == request) {
                    break l;
                }
                wait("the listing").await?;
            };
            println!("{} (parent {:?})", if listing.path.is_empty() { "<places>" } else { &listing.path }, listing.parent);
            if let Some(e) = listing.error {
                println!("error: {e}");
            }
            for e in listing.entries {
                println!("  {} {}  {}", if e.is_dir { "dir " } else { "file" }, e.size, e.path);
            }
        }
        let mut expected = 0;
        if !job.put.is_empty() {
            let dir = job.dir.ok_or_else(|| anyhow!("--put needs --dir"))?;
            expected += job.put.iter().filter(|p| !p.is_dir()).count();
            if let Err(e) = transfers.upload(job.put, &dir) {
                println!("{e}");
            }
        }
        if let Some(remote) = job.get {
            let to = job.to.ok_or_else(|| anyhow!("--get needs --to"))?;
            transfers.download(&remote, to);
            expected += 1;
        }
        let start = Instant::now();
        let mut failed = false;
        while expected > 0 {
            for done in transfers.take_finished() {
                expected -= 1;
                let verb = if done.upload { "sent" } else { "got" };
                match done.result {
                    Ok(path) => println!("{verb} {} -> {path}", done.name),
                    Err(e) => {
                        failed = true;
                        println!("FAILED {}: {e}", done.name);
                    }
                }
            }
            if expected > 0 {
                wait("a transfer").await?;
            }
        }
        if start.elapsed() > Duration::from_millis(1) {
            println!("transfers took {:.2}s", start.elapsed().as_secs_f64());
        }
        if failed {
            bail!("some transfers failed");
        }
        Ok(())
    })
}

/// Encodes the framebuffer as a 32-bit top-down BMP.
fn bitmap(fb: &Framebuffer) -> Vec<u8> {
    let size = fb.pixels.len() as u32;
    let mut out = Vec::with_capacity(54 + fb.pixels.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + size).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(fb.width as i32).to_le_bytes());
    out.extend_from_slice(&(-(fb.height as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&fb.pixels);
    out
}
