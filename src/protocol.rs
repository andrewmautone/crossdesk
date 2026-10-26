//! Wire messages. The handshake is plaintext JSON; everything after it is
//! bincode inside encrypted frames (see `crypto`).

use crate::layout::{Layout, Monitor};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const APP: &str = "crossdesk";
pub const PROTOCOL_VERSION: u32 = 1;
pub const TCP_PORT: u16 = 47800;
pub const DISCOVERY_PORT: u16 = 47801;
const MAX_FRAME: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloMode {
    Pair,
    Resume,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub app: String,
    pub version: u32,
    pub id: String,
    pub name: String,
    pub os: String,
    pub mode: HelloMode,
    pub nonce: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelloReply {
    pub id: String,
    pub name: String,
    pub os: String,
    pub nonce: String,
    pub ok: bool,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Msg {
    /// First encrypted message in each direction; proves both sides hold the key.
    Auth,
    Info { name: String, os: String, monitors: Vec<Monitor> },
    Layout(Layout),
    /// The sender now controls the receiver's cursor, starting at this native point.
    Enter { x: i32, y: i32 },
    /// The sender stopped controlling the receiver.
    Leave,
    MouseMove { x: i32, y: i32 },
    MouseButton { button: Button, down: bool },
    /// Wheel deltas in Windows units (120 = one notch). Positive dy scrolls up.
    Wheel { dx: i32, dy: i32 },
    /// USB HID usage id (keyboard page) of a physical key.
    Key { hid: u16, down: bool },
    Clipboard(String),
    /// The sender removed its trust in the receiver.
    Unpair,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Announce {
    pub app: String,
    pub id: String,
    pub name: String,
    pub os: String,
    pub port: u16,
}

pub fn this_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "other"
    }
}

pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&(data.len() as u32).to_be_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}
