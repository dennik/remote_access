//! Length-prefixed postcard messages exchanged after the TLS handshake.

use anyhow::{bail, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Upper bound on one message. A whole 8K frame at high quality fits well within it.
const MAX_MESSAGE: usize = 128 * 1024 * 1024;

/// Discovery reply, sent as postcard over UDP.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiscoveryReply {
    pub name: String,
    pub port: u16,
    pub fingerprint: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum AuthMsg {
    Challenge { nonce: [u8; 32], name: String },
    Response { public_key: [u8; 32], signature: Vec<u8> },
    Accepted,
    Denied,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    #[serde(with = "serde_bytes")]
    pub jpeg: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Arrow,
    IBeam,
    Hand,
    Wait,
    AppStarting,
    Cross,
    SizeAll,
    SizeNs,
    SizeWe,
    SizeNesw,
    SizeNwse,
    No,
    Hidden,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ToViewer {
    Monitors { monitors: Vec<MonitorInfo>, current: u32 },
    Frame { seq: u64, width: u32, height: u32, tiles: Vec<Tile> },
    Cursor(CursorShape),
    Clipboard(String),
    // Added for file transfer. New variants go at the end so older ones keep their encoding.
    /// The answer to `FEATURE_PROBE`. Never sent unasked, because older viewers can't decode it.
    Features { file_transfer: bool },
    DirListing {
        request: u32,
        path: String,
        parent: Option<String>,
        entries: Vec<DirEntry>,
        error: Option<String>,
    },
    UploadAck { id: u32, received: u64 },
    DownloadInfo { id: u32, name: String, size: u64 },
    DownloadData {
        id: u32,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Ok holds the saved path on the agent for uploads; Err holds a message to show.
    TransferDone { id: u32, result: Result<String, String> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub name: String,
    /// Full path on the agent. The viewer never builds paths itself.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Sent as `ToAgent::SelectMonitor(FEATURE_PROBE)` right after login. Agents that
/// know about features answer with `ToViewer::Features`; older agents ignore it as
/// a monitor that doesn't exist. That way neither side ever receives a message it
/// can't decode, which would end the session.
pub const FEATURE_PROBE: u32 = u32::MAX;

/// Size of each file chunk.
pub const FILE_CHUNK: usize = 256 * 1024;
/// Unacknowledged bytes allowed in flight per transfer, so screen updates aren't starved.
pub const FILE_WINDOW: u64 = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ToAgent {
    Ack { seq: u64 },
    SelectMonitor(u32),
    SetQuality(u8),
    RequestFullFrame,
    /// Position on the selected monitor, scaled to 0..=65535 on each axis.
    MouseMove { x: u16, y: u16 },
    MouseButton { button: MouseButton, down: bool },
    Wheel { horizontal: bool, delta: i16 },
    Key { scan: u16, extended: bool, down: bool },
    Clipboard(String),
    CtrlAltDel,
    // Added for file transfer.
    /// An empty path lists the starting places: drives or volumes and the user's folders.
    ListDir { request: u32, path: String },
    UploadBegin { id: u32, dir: String, name: String, size: u64 },
    UploadData {
        id: u32,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    UploadEnd { id: u32 },
    DownloadBegin { id: u32, path: String },
    /// Bytes of a download the viewer has written so far.
    TransferAck { id: u32, received: u64 },
    TransferCancel { id: u32 },
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_MESSAGE {
        bail!("message too large: {} bytes", body.len());
    }
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_msg<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        bail!("peer sent an oversized message: {len} bytes");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_msg(&mut a, &ToAgent::Key { scan: 0x1d, extended: true, down: true }).await.unwrap();
        match read_msg::<_, ToAgent>(&mut b).await.unwrap() {
            ToAgent::Key { scan, extended, down } => {
                assert_eq!(scan, 0x1d);
                assert!(extended && down);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_messages_round_trip_and_old_variants_keep_their_encoding() {
        // Encodings older agents and viewers depend on must not move.
        assert_eq!(postcard::to_stdvec(&ToAgent::CtrlAltDel).unwrap(), vec![9]);
        assert_eq!(postcard::to_stdvec(&ToViewer::Clipboard(String::new())).unwrap(), vec![3, 0]);

        let (mut a, mut b) = tokio::io::duplex(1024 * 1024);
        let chunk = vec![7u8; FILE_CHUNK];
        write_msg(&mut a, &ToAgent::UploadData { id: 3, data: chunk.clone() }).await.unwrap();
        write_msg(&mut a, &ToViewer::TransferDone { id: 3, result: Err("disk full".into()) }).await.unwrap();
        match read_msg::<_, ToAgent>(&mut b).await.unwrap() {
            ToAgent::UploadData { id, data } => assert!(id == 3 && data == chunk),
            other => panic!("unexpected {other:?}"),
        }
        match read_msg::<_, ToViewer>(&mut b).await.unwrap() {
            ToViewer::TransferDone { id: 3, result: Err(message) } => assert_eq!(message, "disk full"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_length_is_rejected_before_allocating() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        assert!(read_msg::<_, ToAgent>(&mut b).await.is_err());
    }
}
