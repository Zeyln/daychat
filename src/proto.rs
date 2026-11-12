//! Wire protocol: newline-delimited JSON over TCP.
//!
//! Every frame is one JSON object on one line, tagged by `"t"`.
//!
//! Handshake: `hello` (public key) → `challenge` → `auth` (signature) →
//! `welcome`. Private messages are end-to-end encrypted (see `crypto`);
//! the server only relays them between users who both agreed to connect.

use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// v3: signed-challenge login, end-to-end encrypted private messages.
pub const PROTOCOL_VERSION: u32 = 3;
pub const DEFAULT_PORT: u16 = 7878;

/// Hard cap on a single protocol line in bytes. This is a framing guard,
/// independent of (and much larger than) the per-message character limit.
pub const MAX_LINE_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First frame on every connection: protocol version and the client's
    /// Ed25519 public key for this server. (`pubkey` defaults to empty so
    /// older clients still parse and get a clear "please upgrade" error.)
    Hello {
        version: u32,
        #[serde(default)]
        pubkey: String,
    },
    /// Signature over the server's challenge.
    Auth { answer: String },
    /// Message to the public room.
    Say { text: String },
    /// End-to-end encrypted message to a user you are connected with.
    Dm { to: String, sealed: String },
    /// Ask to connect with a user, or accept their pending request to you.
    /// `key` is our X25519 public half for this conversation.
    Connect { to: String, key: String },
    /// End a conversation, decline a request, or withdraw your own request.
    Hangup { peer: String },
    /// List who is online.
    Who,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Sign this (see `crypto::Identity::answer`) to log in.
    Challenge { challenge: String },
    Welcome {
        id: String,
        max_chars: usize,
        burst: u32,
        refill_secs: f64,
        online: usize,
    },
    Chat { from: String, text: String, ts: i64 },
    /// End-to-end encrypted message from a connected peer.
    Dm { from: String, sealed: String, ts: i64 },
    /// `online` is the new total, so a client's count corrects itself even
    /// after announcements were skipped during a flood.
    Joined {
        id: String,
        #[serde(default)]
        online: usize,
    },
    Left {
        id: String,
        #[serde(default)]
        online: usize,
    },
    /// Sent instead of skipped announcements, at most every few seconds.
    Online { total: usize },
    /// The UTC day rolled over; this connection now has a new ID. All
    /// conversations and requests were dropped.
    Rotated { id: String },
    /// `from` wants to connect with you.
    Request { from: String },
    /// Your request to `to` was delivered; waiting for them.
    Requested { to: String },
    /// You and `peer` are now connected. `key` is their X25519 public half.
    Linked { peer: String, key: String },
    /// No longer connected (or no longer requesting) with `peer`.
    Unlinked { peer: String, reason: String },
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
