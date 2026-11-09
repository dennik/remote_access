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
}

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
    async fn oversized_length_is_rejected_before_allocating() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        assert!(read_msg::<_, ToAgent>(&mut b).await.is_err());
    }
}
