//! The relay server. It stores nothing except its own 32-byte secret: no
//! history, and no logs of message content. Private messages pass through
//! end-to-end encrypted, so it can't read those either.
//!
//! It runs standalone as `daychat-server`, or embedded in the client via
//! `/host`.

use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv6Addr},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use rand::RngCore;
use rustls::ServerConfig;
use tokio::{
    io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch, Semaphore},
};
use tokio_rustls::TlsAcceptor;

use crate::{
    crypto, id,
    limits::{self, TokenBucket},
    proto::{self, ClientMsg, ServerMsg},
};

const MAX_CLIENTS: usize = 1024;
/// Per-client outbound queue. A client that can't keep up misses messages
/// instead of slowing everyone else down.
const OUTBOX: usize = 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ROTATION_CHECK: Duration = Duration::from_secs(15);
const MAX_WHO: usize = 200;
/// Outgoing connection requests a user may have waiting at once.
const MAX_PENDING: usize = 16;

// Per-address limits. IPv6 addresses are grouped by /64, since one host
// usually controls a whole /64.
/// Simultaneous connections from one address.
const MAX_CONNS_PER_IP: usize = 8;
/// New connections from one address: a burst, then one every few seconds.
const ACCEPT_BURST: u32 = 10;
const ACCEPT_REFILL_SECS: f64 = 6.0;
/// Connection requests from one address, across all of its IDs.
const REQUEST_BURST: u32 = 5;
const REQUEST_REFILL_SECS: f64 = 20.0;
/// Forget idle addresses after this long (once the table grows).
const IP_IDLE: Duration = Duration::from_secs(600);
const IP_TABLE_PRUNE_AT: usize = 4096;

/// Where the server sends its (content-free) operational log lines.
pub type Logger = Arc<dyn Fn(String) + Send + Sync>;

// --- Users, requests and conversations -------------------------------------------

#[derive(Default)]
struct Inner {
    users: HashMap<String, mpsc::Sender<ServerMsg>>,
    /// `(from, to)` → `from`'s exchange key: `from` asked to connect with
    /// `to`, who hasn't answered yet.
    pending: HashMap<(String, String), String>,
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
    /// `key` is `me`'s exchange key; each side receives the other's.
    fn connect(&self, me: &str, to: &str, key: String) -> Result<(), String> {
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
        if let Some(their_key) = g.pending.remove(&(to.to_string(), me.to_string())) {
            g.links.insert(link);
            g.send(me, ServerMsg::Linked { peer: to.into(), key: their_key });
            g.send(to, ServerMsg::Linked { peer: me.into(), key });
            return Ok(());
        }
        let mine = (me.to_string(), to.to_string());
        if g.pending.contains_key(&mine) {
            return Err(format!("already waiting for {to} to accept"));
        }
        if g.pending.keys().filter(|(from, _)| from == me).count() >= MAX_PENDING {
            return Err("too many unanswered requests; withdraw some with /close <id>".into());
        }
        g.pending.insert(mine, key);
        g.send(to, ServerMsg::Request { from: me.into() });
        g.send(me, ServerMsg::Requested { to: to.into() });
        Ok(())
    }

    /// End a conversation, decline `peer`'s request, or withdraw our own.
    fn hangup(&self, me: &str, peer: &str) -> Result<(), String> {
        let mut g = self.lock();
        let (theirs, mine) = if g.links.remove(&pair(me, peer)) {
            ("ended the conversation", "conversation ended")
        } else if g.pending.remove(&(peer.to_string(), me.to_string())).is_some() {
            ("declined your request", "request declined")
        } else if g.pending.remove(&(me.to_string(), peer.to_string())).is_some() {
            ("withdrew their request", "request withdrawn")
        } else {
            return Err(format!("no conversation or request with {peer}"));
        };
        g.send(peer, ServerMsg::Unlinked { peer: me.into(), reason: theirs.into() });
        g.send(me, ServerMsg::Unlinked { peer: peer.into(), reason: mine.into() });
        Ok(())
    }

    /// Relay a sealed private message; only between connected users.
    fn dm(&self, me: &str, to: &str, sealed: String) -> Result<(), String> {
        if sealed.is_empty() || sealed.len() > crypto::MAX_SEALED_LEN {
            return Err("malformed private message".into());
        }
        let g = self.lock();
        if !g.links.contains(&pair(me, to)) {
            return Err(format!("not connected to {to}; /connect {to} first"));
        }
        g.send(to, ServerMsg::Dm { from: me.into(), sealed, ts: now() });
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
        g.pending.retain(|k, _| keep(k));
        g.links.retain(&mut keep);
        for other in others {
            g.send(&other, ServerMsg::Unlinked { peer: me.into(), reason: "went offline".into() });
        }
    }
}

// --- Per-address limits --------------------------------------------------------

struct IpEntry {
    conns: usize,
    accepts: TokenBucket,
    requests: TokenBucket,
    last_seen: Instant,
}

#[derive(Default)]
struct IpLimits {
    map: Mutex<HashMap<IpAddr, IpEntry>>,
}

/// Group addresses the way one host controls them: IPv4 as-is, IPv6 by /64.
fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

impl IpLimits {
    /// Admit a new connection from `ip`, or say why not. The returned
    /// guard releases the slot when the connection ends.
    fn admit(self: &Arc<Self>, ip: IpAddr) -> Result<IpGuard, &'static str> {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap();
        if map.len() >= IP_TABLE_PRUNE_AT {
            map.retain(|_, e| e.conns > 0 || now.duration_since(e.last_seen) < IP_IDLE);
        }
        let e = map.entry(ip).or_insert_with(|| IpEntry {
            conns: 0,
            accepts: TokenBucket::new(ACCEPT_BURST, ACCEPT_REFILL_SECS),
            requests: TokenBucket::new(REQUEST_BURST, REQUEST_REFILL_SECS),
            last_seen: now,
        });
        e.last_seen = now;
        if e.conns >= MAX_CONNS_PER_IP {
            return Err("too many connections from your address");
        }
        if e.accepts.try_take().is_err() {
            return Err("connecting too often from your address; try again shortly");
        }
        e.conns += 1;
        Ok(IpGuard { limits: self.clone(), ip })
    }

    /// Take a connection-request token for `ip`.
    fn allow_request(&self, ip: IpAddr) -> bool {
        self.map.lock().unwrap().get_mut(&ip).is_some_and(|e| e.requests.try_take().is_ok())
    }
}

struct IpGuard {
    limits: Arc<IpLimits>,
    ip: IpAddr,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        if let Some(e) = self.limits.map.lock().unwrap().get_mut(&self.ip) {
            e.conns = e.conns.saturating_sub(1);
        }
    }
}

// --- Server ----------------------------------------------------------------------

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

/// Accept and serve clients over TLS until `shutdown` changes (or its
/// sender is dropped). Connections over the limits are dropped before the
/// TLS handshake, so refusing them costs almost nothing.
pub async fn serve(
    listener: TcpListener,
    secret: [u8; 32],
    tls: Arc<ServerConfig>,
    mut shutdown: watch::Receiver<bool>,
    log: Logger,
) -> io::Result<()> {
    let state = Arc::new(State::new(secret));
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let ips = Arc::new(IpLimits::default());
    let acceptor = TlsAcceptor::from(tls);

    loop {
        let (stream, addr) = tokio::select! {
            r = listener.accept() => match r {
                Ok(conn) => conn,
                Err(e) => {
                    (*log)(format!("accept error: {e}"));
                    continue;
                }
            },
            _ = shutdown.changed() => return Ok(()),
        };
        let ip = ip_key(addr.ip());
        let Ok(guard) = ips.admit(ip) else {
            continue; // dropping the stream closes it
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let state = state.clone();
        let ips = ips.clone();
        let acceptor = acceptor.clone();
        let shutdown = shutdown.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(state, stream, acceptor, shutdown, &ips, ip).await {
                (*log)(format!("connection error: {e}"));
            }
            drop(guard);
            drop(permit);
        });
    }
}

type Tls = tokio_rustls::server::TlsStream<TcpStream>;
type Reader = BufReader<ReadHalf<Tls>>;
type Writer = WriteHalf<Tls>;

async fn reject(w: &mut Writer, why: &str) -> io::Result<()> {
    let msg = ServerMsg::Error { text: why.into() };
    w.write_all(proto::encode(&msg).as_bytes()).await?;
    w.shutdown().await
}

enum Login {
    /// Authenticated with this (canonical) public key.
    Authed(String),
    Closed,
    Reject(&'static str),
}

/// `hello` → `challenge` → `auth`: the client proves it holds the private
/// key for its public key, without anything replayable crossing the wire.
async fn authenticate(reader: &mut Reader, w: &mut Writer) -> io::Result<Login> {
    let mut buf = Vec::new();
    if !proto::read_line_limited(reader, &mut buf).await? {
        return Ok(Login::Closed);
    }
    let pubkey = match serde_json::from_slice::<ClientMsg>(&buf) {
        Ok(ClientMsg::Hello { version, pubkey }) if version == proto::PROTOCOL_VERSION => pubkey,
        Ok(ClientMsg::Hello { .. }) => {
            return Ok(Login::Reject("unsupported protocol version; please upgrade daychat"))
        }
        _ => return Ok(Login::Reject("expected hello")),
    };
    let Some(pubkey) = crypto::parse_public(&pubkey) else {
        return Ok(Login::Reject("malformed public key"));
    };

    let challenge = crypto::new_challenge();
    let msg = ServerMsg::Challenge { challenge: challenge.clone() };
    w.write_all(proto::encode(&msg).as_bytes()).await?;

    if !proto::read_line_limited(reader, &mut buf).await? {
        return Ok(Login::Closed);
    }
    match serde_json::from_slice::<ClientMsg>(&buf) {
        Ok(ClientMsg::Auth { answer }) if crypto::verify_answer(&pubkey, &challenge, &answer) => {
            Ok(Login::Authed(pubkey))
        }
        _ => Ok(Login::Reject("authentication failed")),
    }
}

async fn handle(
    state: Arc<State>,
    stream: TcpStream,
    acceptor: TlsAcceptor,
    mut shutdown: watch::Receiver<bool>,
    ips: &IpLimits,
    ip: IpAddr,
) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    // One deadline covers both the TLS handshake and the login.
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let stream = match tokio::time::timeout_at(deadline, acceptor.accept(stream)).await {
        Ok(Ok(s)) => s,
        // Failed or abandoned TLS handshakes (scanners, old clients) are
        // routine noise; don't log them.
        _ => return Ok(()),
    };
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    // --- Handshake -------------------------------------------------------
    let login = tokio::time::timeout_at(deadline, authenticate(&mut reader, &mut w)).await;
    let key = match login {
        Err(_) => return reject(&mut w, "handshake timeout").await,
        Ok(Err(e)) => return Err(e),
        Ok(Ok(Login::Closed)) => return Ok(()),
        Ok(Ok(Login::Reject(why))) => return reject(&mut w, why).await,
        Ok(Ok(Login::Authed(key))) => key,
    };

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
                        Ok(msg) => process(&state, ips, ip, &me, msg),
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

fn process(state: &State, ips: &IpLimits, ip: IpAddr, me: &str, msg: ClientMsg) -> Option<ServerMsg> {
    let result = match msg {
        ClientMsg::Hello { .. } | ClientMsg::Auth { .. } => Err("already logged in".to_string()),
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
        ClientMsg::Dm { to, sealed } => parse_id(&to).and_then(|to| state.dm(me, &to, sealed)),
        ClientMsg::Connect { to, key } => parse_id(&to).and_then(|to| {
            if !crypto::is_exchange_key(&key) {
                return Err("malformed exchange key".into());
            }
            // Only new requests cost from the address budget. Accepting one
            // that was made to us can't be used to spam anyone, and must keep
            // working for others behind the same address (e.g. one NAT).
            let accepting = state.lock().pending.contains_key(&(to.clone(), me.to_string()));
            if !accepting && !ips.allow_request(ip) {
                return Err("too many connection requests from your address; wait a little".into());
            }
            state.connect(me, &to, key)
        }),
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
    use crate::tls::{self, Pin, Trust};
    use std::net::Ipv4Addr;

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
    fn dm_requires_mutual_connect_and_keys_are_swapped() {
        let s = State::new([0; 32]);
        let (mut ra, mut rb) = (user(&s, A), user(&s, B));

        assert!(s.dm(A, B, "x".into()).is_err());
        s.connect(A, B, "KEY-A".into()).unwrap();
        assert!(drain(&mut rb).iter().any(|m| m.contains("\"request\"")));
        assert!(drain(&mut ra).iter().any(|m| m.contains("\"requested\"")));
        assert!(s.connect(A, B, "KEY-A".into()).is_err(), "duplicate request");
        assert!(s.dm(A, B, "x".into()).is_err(), "not accepted yet");

        s.connect(B, A, "KEY-B".into()).unwrap(); // accept by requesting back
        let a_got = drain(&mut ra);
        let b_got = drain(&mut rb);
        assert!(a_got.iter().any(|m| m.contains("\"linked\"") && m.contains("KEY-B")));
        assert!(b_got.iter().any(|m| m.contains("\"linked\"") && m.contains("KEY-A")));

        s.dm(A, B, "sealed".into()).unwrap();
        assert!(drain(&mut rb).iter().any(|m| m.contains("\"dm\"")));
        assert!(drain(&mut ra).is_empty(), "no echo; sender shows its own copy");
        assert!(s.dm(A, B, "x".repeat(crypto::MAX_SEALED_LEN + 1)).is_err());

        s.hangup(B, A).unwrap();
        assert!(drain(&mut ra).iter().any(|m| m.contains("ended the conversation")));
        assert!(s.dm(A, B, "x".into()).is_err());
    }

    #[test]
    fn decline_withdraw_and_disconnect() {
        let s = State::new([0; 32]);
        let (mut ra, mut rb) = (user(&s, A), user(&s, B));

        s.connect(A, B, "k".into()).unwrap();
        s.hangup(B, A).unwrap();
        assert!(drain(&mut ra).iter().any(|m| m.contains("declined your request")));

        s.connect(A, B, "k".into()).unwrap();
        s.hangup(A, B).unwrap();
        assert!(drain(&mut rb).iter().any(|m| m.contains("withdrew their request")));

        s.connect(A, B, "k".into()).unwrap();
        s.connect(B, A, "k".into()).unwrap();
        drain(&mut rb);
        s.forget(A);
        assert!(drain(&mut rb).iter().any(|m| m.contains("went offline")));
        assert!(s.dm(B, A, "x".into()).is_err());
        assert!(s.connect(A, A, "k".into()).is_err());
    }

    #[test]
    fn per_address_limits() {
        let ips = Arc::new(IpLimits::default());
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let guards: Vec<_> = (0..MAX_CONNS_PER_IP).map(|_| ips.admit(ip).unwrap()).collect();
        assert!(ips.admit(ip).is_err(), "connection cap");
        drop(guards);
        assert!(ips.admit(ip).is_ok(), "slots released on drop");

        for _ in 0..REQUEST_BURST {
            assert!(ips.allow_request(ip));
        }
        assert!(!ips.allow_request(ip), "request budget shared by the address");

        // With that budget spent, new requests are refused but accepting
        // a request made to us still works.
        let s = State::new([0; 32]);
        let (_ra, _rb, _rc) = (user(&s, A), user(&s, B), user(&s, "cccc-cccc"));
        s.connect(A, B, "k".into()).unwrap();
        let req = |to: &str| ClientMsg::Connect { to: to.into(), key: crypto::new_challenge() };
        let refused = process(&s, &ips, ip, B, req("cccc-cccc"));
        assert!(matches!(refused, Some(ServerMsg::Error { ref text }) if text.contains("too many")));
        assert!(process(&s, &ips, ip, B, req(A)).is_none(), "accepting is never rate-limited");
        assert!(s.lock().links.contains(&pair(A, B)));

        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff:2".parse().unwrap();
        assert_eq!(ip_key(a), ip_key(b), "IPv6 grouped by /64");
    }

    /// Minimal client: TLS, then hello → challenge → auth. Returns the
    /// server's final handshake frame and how the certificate was trusted.
    async fn login(addr: &str, local_key: &str, pin: Pin) -> io::Result<(ServerMsg, Option<Trust>)> {
        let (config, outcome) = tls::client_config(pin)?;
        let tcp = TcpStream::connect(addr).await?;
        let stream = tokio_rustls::TlsConnector::from(config).connect(tls::server_name(addr)?, tcp).await?;
        let (r, mut w) = tokio::io::split(stream);
        let mut r = BufReader::new(r);
        let me = crypto::Identity::for_server(local_key, addr);
        let hello = ClientMsg::Hello { version: proto::PROTOCOL_VERSION, pubkey: me.public() };
        w.write_all(proto::encode(&hello).as_bytes()).await?;
        let mut buf = Vec::new();
        proto::read_line_limited(&mut r, &mut buf).await?;
        let Ok(ServerMsg::Challenge { challenge }) = serde_json::from_slice::<ServerMsg>(&buf) else {
            panic!("expected a challenge");
        };
        let auth = ClientMsg::Auth { answer: me.answer(&challenge) };
        w.write_all(proto::encode(&auth).as_bytes()).await?;
        proto::read_line_limited(&mut r, &mut buf).await?;
        let trust = outcome.lock().unwrap().take();
        Ok((serde_json::from_slice::<ServerMsg>(&buf).unwrap(), trust))
    }

    #[tokio::test]
    async fn tls_login_and_certificate_pinning() {
        let dir = std::env::temp_dir().join(format!("daychat-test-{}", id::new_client_key()));
        let (cert, key) = (dir.join("server.crt"), dir.join("server.key"));
        assert!(tls::create_self_signed(&cert, &key).unwrap());
        assert!(!tls::create_self_signed(&cert, &key).unwrap(), "existing files are kept");
        let (config, fp) = tls::server_config(&cert, &key).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (stop, shutdown) = watch::channel(false);
        let log: Logger = Arc::new(|_: String| {});
        tokio::spawn(serve(listener, [7; 32], config, shutdown, log));

        // First visit: self-signed, so the fingerprint gets pinned.
        let (msg, trust) = login(&addr, &"a1".repeat(32), Pin::Tofu(None)).await.unwrap();
        assert!(matches!(msg, ServerMsg::Welcome { .. }), "{msg:?}");
        assert_eq!(trust, Some(Trust::NewPin(fp.clone())));

        // Later visits match the pin.
        let (msg, trust) = login(&addr, &"b2".repeat(32), Pin::Tofu(Some(fp.clone()))).await.unwrap();
        assert!(matches!(msg, ServerMsg::Welcome { .. }), "{msg:?}");
        assert_eq!(trust, Some(Trust::Pinned));

        // A different certificate than the pinned one is refused.
        assert!(login(&addr, &"c3".repeat(32), Pin::Tofu(Some("0".repeat(64)))).await.is_err());

        // The official server is pinned exactly: right fingerprint only.
        let (msg, trust) = login(&addr, &"d4".repeat(32), Pin::Exact(fp)).await.unwrap();
        assert!(matches!(msg, ServerMsg::Welcome { .. }), "{msg:?}");
        assert_eq!(trust, Some(Trust::Pinned));
        assert!(login(&addr, &"e5".repeat(32), Pin::Exact("0".repeat(64))).await.is_err());

        let _ = stop.send(true);
        let _ = std::fs::remove_dir_all(dir);
    }
}
