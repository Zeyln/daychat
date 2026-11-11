//! The relay server. It stores nothing except its own 32-byte secret: no
//! history, and no logs of message content.
//!
//! It runs standalone as `daychat-server`, or embedded in the client via
//! `/host`.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use rand::RngCore;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::{mpsc, watch, Semaphore},
};

use crate::{
    id,
    limits::{self, TokenBucket},
    proto::{self, ClientMsg, ServerMsg},
};

const MAX_CLIENTS: usize = 1024;
/// Per-client outbound queue. A client that can't keep up misses messages
/// instead of slowing everyone else down.
const OUTBOX: usize = 256;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const ROTATION_CHECK: Duration = Duration::from_secs(15);
const MAX_WHO: usize = 200;

/// Where the server sends its (content-free) operational log lines.
pub type Logger = Arc<dyn Fn(String) + Send + Sync>;

/// Outgoing connection requests a user may have waiting at once.
const MAX_PENDING: usize = 16;

#[derive(Default)]
struct Inner {
    users: HashMap<String, mpsc::Sender<ServerMsg>>,
    /// `(from, to)`: `from` asked to connect with `to`, who hasn't answered.
    pending: HashSet<(String, String)>,
    /// Mutually accepted conversations, stored as `pair(a, b)`.
    links: HashSet<(String, String)>,
}

impl Inner {
    fn send(&self, id: &str, msg: ServerMsg) {
        if let Some(tx) = self.users.get(id) {
            let _ = tx.try_send(msg);
        }
    }
}

/// Order-independent key for a conversation between `a` and `b`.
fn pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

struct State {
    secret: [u8; 32],
    inner: Mutex<Inner>,
}

impl State {
    fn new(secret: [u8; 32]) -> Self {
        Self { secret, inner: Mutex::new(Inner::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn broadcast(&self, msg: &ServerMsg) {
        for tx in self.lock().users.values() {
            let _ = tx.try_send(msg.clone());
        }
    }

    fn online(&self) -> usize {
        self.lock().users.len()
    }

    /// Register `id`; false if it is already connected.
    fn register(&self, id: &str, tx: mpsc::Sender<ServerMsg>) -> bool {
        let mut g = self.lock();
        if g.users.contains_key(id) {
            return false;
        }
        g.users.insert(id.to_string(), tx);
        true
    }

    /// Swap `old` for `new` (day rollover); false if `new` is taken.
    fn rename(&self, old: &str, new: &str, tx: mpsc::Sender<ServerMsg>) -> bool {
        let mut g = self.lock();
        if g.users.contains_key(new) {
            return false;
        }
        g.users.remove(old);
        g.users.insert(new.to_string(), tx);
        true
    }

    fn unregister(&self, id: &str) {
        self.lock().users.remove(id);
    }

    /// Ask to connect with `to`, or accept if `to` already asked `me`.
    fn connect(&self, me: &str, to: &str) -> Result<(), String> {
        if to == me {
            return Err("you can't connect to yourself".into());
        }
        let mut g = self.lock();
        if !g.users.contains_key(to) {
            return Err(format!("{to} is not online"));
        }
        let link = pair(me, to);
        if g.links.contains(&link) {
            return Err(format!("already connected to {to}"));
        }
        if g.pending.remove(&(to.to_string(), me.to_string())) {
            g.links.insert(link);
            g.send(me, ServerMsg::Linked { peer: to.into() });
            g.send(to, ServerMsg::Linked { peer: me.into() });
            return Ok(());
        }
        let mine = (me.to_string(), to.to_string());
        if g.pending.contains(&mine) {
            return Err(format!("already waiting for {to} to accept"));
        }
        if g.pending.iter().filter(|(from, _)| from == me).count() >= MAX_PENDING {
            return Err("too many unanswered requests; withdraw some with /close <id>".into());
        }
        g.pending.insert(mine);
        g.send(to, ServerMsg::Request { from: me.into() });
        g.send(me, ServerMsg::Requested { to: to.into() });
        Ok(())
    }

    /// End a conversation, decline `peer`'s request, or withdraw our own.
    fn hangup(&self, me: &str, peer: &str) -> Result<(), String> {
        let mut g = self.lock();
        let (theirs, mine) = if g.links.remove(&pair(me, peer)) {
            ("ended the conversation", "conversation ended")
        } else if g.pending.remove(&(peer.to_string(), me.to_string())) {
            ("declined your request", "request declined")
        } else if g.pending.remove(&(me.to_string(), peer.to_string())) {
            ("withdrew their request", "request withdrawn")
        } else {
            return Err(format!("no conversation or request with {peer}"));
        };
        g.send(peer, ServerMsg::Unlinked { peer: me.into(), reason: theirs.into() });
        g.send(me, ServerMsg::Unlinked { peer: peer.into(), reason: mine.into() });
        Ok(())
    }

    /// Deliver a private message; only allowed between connected users.
    fn dm(&self, me: &str, to: &str, text: String) -> Result<(), String> {
        let g = self.lock();
        if !g.links.contains(&pair(me, to)) {
            return Err(format!("not connected to {to}; /connect {to} first"));
        }
        let msg = ServerMsg::Dm { from: me.into(), to: to.into(), text, ts: now() };
        g.send(to, msg.clone());
        g.send(me, msg);
        Ok(())
    }

    /// Drop every request and conversation involving `me`, telling the
    /// other side. Used when `me` disconnects or its ID rotates.
    fn forget(&self, me: &str) {
        let mut g = self.lock();
        let mut others = Vec::new();
        let mut keep = |(a, b): &(String, String)| {
            if a == me {
                others.push(b.clone());
                false
            } else if b == me {
                others.push(a.clone());
                false
            } else {
                true
            }
        };
        g.pending.retain(&mut keep);
        g.links.retain(&mut keep);
        for other in others {
            g.send(&other, ServerMsg::Unlinked { peer: me.into(), reason: "went offline".into() });
        }
    }
}

/// Read the server secret, or create a fresh one if the file doesn't exist.
pub fn load_or_create_secret(path: &Path, log: &Logger) -> io::Result<[u8; 32]> {
    match std::fs::read(path) {
        Ok(bytes) => bytes.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "secret file must be exactly 32 bytes")
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            let mut s = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut s);
            std::fs::write(path, s)?;
            (*log)(format!("created new server secret at {}", path.display()));
            Ok(s)
        }
        Err(e) => Err(e),
    }
}

/// Accept and serve clients until `shutdown` changes (or its sender is dropped).
pub async fn serve(
    listener: TcpListener,
    secret: [u8; 32],
    mut shutdown: watch::Receiver<bool>,
    log: Logger,
) -> io::Result<()> {
    let state = Arc::new(State::new(secret));
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));

    loop {
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(e) => {
                    (*log)(format!("accept error: {e}"));
                    continue;
                }
            },
            _ = shutdown.changed() => return Ok(()),
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tokio::spawn(async move {
                let mut s = stream;
                let full = ServerMsg::Error { text: "server full".into() };
                let _ = s.write_all(proto::encode(&full).as_bytes()).await;
            });
            continue;
        };
        let state = state.clone();
        let shutdown = shutdown.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(state, stream, shutdown).await {
                (*log)(format!("connection error: {e}"));
            }
            drop(permit);
        });
    }
}

async fn reject(w: &mut OwnedWriteHalf, why: &str) -> io::Result<()> {
    let msg = ServerMsg::Error { text: why.into() };
    w.write_all(proto::encode(&msg).as_bytes()).await?;
    w.shutdown().await
}

async fn handle(state: Arc<State>, stream: TcpStream, mut shutdown: watch::Receiver<bool>) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();

    // --- Handshake -------------------------------------------------------
    let hello = tokio::time::timeout(HELLO_TIMEOUT, proto::read_line_limited(&mut reader, &mut buf)).await;
    let key = match hello {
        Err(_) => return reject(&mut w, "handshake timeout").await,
        Ok(Err(e)) => return Err(e),
        Ok(Ok(false)) => return Ok(()),
        Ok(Ok(true)) => match serde_json::from_slice::<ClientMsg>(&buf) {
            Ok(ClientMsg::Hello { version, key }) if version == proto::PROTOCOL_VERSION => key,
            Ok(ClientMsg::Hello { .. }) => return reject(&mut w, "unsupported protocol version").await,
            _ => return reject(&mut w, "expected hello").await,
        },
    };
    if !id::valid_client_key(&key) {
        return reject(&mut w, "malformed key").await;
    }

    let (tx, mut rx) = mpsc::channel::<ServerMsg>(OUTBOX);
    let mut day = id::today();
    let mut me = id::derive_id(&state.secret, &day, &key);
    if !state.register(&me, tx.clone()) {
        return reject(&mut w, "this ID is already connected").await;
    }

    // --- Writer task: drains this client's outbox to the socket ------------
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if w.write_all(proto::encode(&msg).as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });

    // --- Reader task: lines -> channel (keeps the select! below cancel-safe)
    let (in_tx, mut in_rx) = mpsc::channel::<io::Result<Vec<u8>>>(16);
    let reader_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        loop {
            match proto::read_line_limited(&mut reader, &mut buf).await {
                Ok(true) => {
                    if in_tx.send(Ok(buf.clone())).await.is_err() {
                        break;
                    }
                }
                Ok(false) => break,
                Err(e) => {
                    let _ = in_tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    let _ = tx.try_send(ServerMsg::Welcome {
        id: me.clone(),
        max_chars: limits::MAX_CHARS,
        burst: limits::RATE_BURST,
        refill_secs: limits::RATE_REFILL_SECS,
        online: state.online(),
    });
    state.broadcast(&ServerMsg::Joined { id: me.clone() });

    // --- Main loop ---------------------------------------------------------
    let mut bucket = TokenBucket::new(limits::RATE_BURST, limits::RATE_REFILL_SECS);
    let mut tick = tokio::time::interval(ROTATION_CHECK);
    let result = loop {
        tokio::select! {
            line = in_rx.recv() => {
                let line = match line {
                    None => break Ok(()),
                    Some(Err(e)) => break Err(e),
                    Some(Ok(l)) => l,
                };
                // Every line costs a token, including malformed ones.
                let reply = if let Err(wait) = bucket.try_take() {
                    Some(err(format!("slow down: try again in {:.1}s", wait.as_secs_f64())))
                } else {
                    match serde_json::from_slice::<ClientMsg>(&line) {
                        Err(_) => Some(err("malformed message")),
                        Ok(msg) => process(&state, &me, msg),
                    }
                };
                if let Some(r) = reply {
                    let _ = tx.try_send(r);
                }
            }
            _ = shutdown.changed() => {
                let _ = tx.try_send(err("server is shutting down"));
                break Ok(());
            }
            _ = tick.tick() => {
                let today = id::today();
                if today == day {
                    continue;
                }
                // Day rolled over: swap to the new ID silently. No join/leave
                // broadcast, so others can't link yesterday's ID to today's.
                day = today;
                let new_id = id::derive_id(&state.secret, &day, &key);
                state.forget(&me);
                if !state.rename(&me, &new_id, tx.clone()) {
                    let _ = tx.try_send(err("ID collision at day rollover; please reconnect"));
                    break Ok(());
                }
                me = new_id;
                let _ = tx.try_send(ServerMsg::Rotated { id: me.clone() });
            }
        }
    };

    // --- Cleanup -----------------------------------------------------------
    state.forget(&me);
    state.unregister(&me);
    state.broadcast(&ServerMsg::Left { id: me });
    reader_task.abort();
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    result
}

fn err(text: impl Into<String>) -> ServerMsg {
    ServerMsg::Error { text: text.into() }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Normalize a user-supplied ID and reject anything that isn't one.
fn parse_id(raw: &str) -> Result<String, String> {
    let id = id::normalize_id(raw);
    if id::looks_like_id(&id) {
        Ok(id)
    } else {
        Err("that is not a valid ID (expected xxxx-xxxx)".into())
    }
}

fn process(state: &State, me: &str, msg: ClientMsg) -> Option<ServerMsg> {
    let result = match msg {
        ClientMsg::Hello { .. } => Err("already connected".to_string()),
        ClientMsg::Who => {
            let mut ids: Vec<String> = state.lock().users.keys().cloned().collect();
            ids.sort();
            let total = ids.len();
            ids.truncate(MAX_WHO);
            return Some(ServerMsg::Who { ids, total });
        }
        ClientMsg::Say { text } => limits::validate_text(&text, limits::MAX_CHARS).map(|text| {
            state.broadcast(&ServerMsg::Chat { from: me.to_string(), text, ts: now() });
        }),
        ClientMsg::Dm { to, text } => parse_id(&to).and_then(|to| {
            let text = limits::validate_text(&text, limits::MAX_CHARS)?;
            state.dm(me, &to, text)
        }),
        ClientMsg::Connect { to } => parse_id(&to).and_then(|to| state.connect(me, &to)),
        ClientMsg::Hangup { peer } => parse_id(&peer).and_then(|peer| state.hangup(me, &peer)),
    };
    match result {
        Ok(()) => None,
        Err(e) => Some(err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(state: &State, id: &str) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel(32);
        assert!(state.register(id, tx));
        rx
    }

    fn drain(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(serde_json::to_string(&m).unwrap());
        }
        out
    }

    const A: &str = "aaaa-aaaa";
    const B: &str = "bbbb-bbbb";

    #[test]
    fn dm_requires_mutual_connect() {
        let s = State::new([0; 32]);
        let (mut ra, mut rb) = (user(&s, A), user(&s, B));

        assert!(s.dm(A, B, "hi".into()).is_err());
        s.connect(A, B).unwrap();
        assert!(drain(&mut rb).iter().any(|m| m.contains("\"request\"")));
        assert!(drain(&mut ra).iter().any(|m| m.contains("\"requested\"")));
        assert!(s.connect(A, B).is_err(), "duplicate request");
        assert!(s.dm(A, B, "hi".into()).is_err(), "not accepted yet");

        s.connect(B, A).unwrap(); // accept by requesting back
        assert!(drain(&mut ra).iter().any(|m| m.contains("\"linked\"")));
        assert!(drain(&mut rb).iter().any(|m| m.contains("\"linked\"")));

        s.dm(A, B, "hi".into()).unwrap();
        assert!(drain(&mut rb).iter().any(|m| m.contains("\"dm\"")));
        assert!(drain(&mut ra).iter().any(|m| m.contains("\"dm\"")), "echoed to sender");

        s.hangup(B, A).unwrap();
        assert!(drain(&mut ra).iter().any(|m| m.contains("ended the conversation")));
        assert!(s.dm(A, B, "hi".into()).is_err());
    }

    #[test]
    fn decline_withdraw_and_disconnect() {
        let s = State::new([0; 32]);
        let (mut ra, mut rb) = (user(&s, A), user(&s, B));

        s.connect(A, B).unwrap();
        s.hangup(B, A).unwrap();
        assert!(drain(&mut ra).iter().any(|m| m.contains("declined your request")));

        s.connect(A, B).unwrap();
        s.hangup(A, B).unwrap();
        assert!(drain(&mut rb).iter().any(|m| m.contains("withdrew their request")));

        s.connect(A, B).unwrap();
        s.connect(B, A).unwrap();
        drain(&mut rb);
        s.forget(A);
        assert!(drain(&mut rb).iter().any(|m| m.contains("went offline")));
        assert!(s.dm(B, A, "hi".into()).is_err());
        assert!(s.connect(A, A).is_err());
    }
}
