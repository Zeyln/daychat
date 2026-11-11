//! Wire protocol: newline-delimited JSON over plain TCP.
//!
//! Every frame is one JSON object on one line, tagged by `"t"`.

use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 7878;

/// Hard cap on a single protocol line in bytes. This is a framing guard,
/// independent of (and much larger than) the per-message character limit.
pub const MAX_LINE_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First frame on every connection. `key` is a per-server secret the
    /// client keeps locally; the server turns it into today's ID.
    Hello { version: u32, key: String },
    /// Message to the public room.
    Say { text: String },
    /// Direct message to another user's ID.
    Dm { to: String, text: String },
    /// List who is online.
    Who,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        id: String,
        max_chars: usize,
        burst: u32,
        refill_secs: f64,
        online: usize,
    },
    Chat { from: String, text: String, ts: i64 },
    Dm { from: String, to: String, text: String, ts: i64 },
    Joined { id: String },
    Left { id: String },
    /// The UTC day rolled over; this connection now has a new ID.
    Rotated { id: String },
    Who { ids: Vec<String>, total: usize },
    Info { text: String },
    Error { text: String },
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("protocol types always serialize");
    s.push('\n');
    s
}

/// Read one `\n`-terminated line into `buf`, refusing lines longer than
/// [`MAX_LINE_BYTES`]. Returns `Ok(false)` on clean EOF.
pub async fn read_line_limited<R>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    buf.clear();
    let n = (&mut *r)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() != Some(&b'\n') && n > MAX_LINE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(true)
}
