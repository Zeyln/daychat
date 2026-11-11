//! daychat-server: relays cleartext messages. Stores nothing but its own
//! 32-byte secret; no history, no logs of message content.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use daychat::{
    id,
    limits::{self, TokenBucket},
    proto::{self, ClientMsg, ServerMsg},
};
use rand::RngCore;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::{mpsc, Semaphore},
};

const MAX_CLIENTS: usize = 1024;
/// Per-client outbound queue. If a client can't keep up, it misses messages
/// rather than slowing everyone else down.
const OUTBOX: usize = 256;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const ROTATION_CHECK: Duration = Duration::from_secs(15);
const MAX_WHO: usize = 200;

const USAGE: &str = "usage: daychat-server [--bind ADDR:PORT] [--secret-file PATH]";

struct State {
    secret: [u8; 32],
    users: Mutex<HashMap<String, mpsc::Sender<ServerMsg>>>,
}

impl State {
    fn broadcast(&self, msg: &ServerMsg) {
        for tx in self.users.lock().unwrap().values() {
            let _ = tx.try_send(msg.clone());
        }
    }

    fn send_to(&self, id: &str, msg: ServerMsg) -> bool {
        match self.users.lock().unwrap().get(id) {
            Some(tx) => {
                let _ = tx.try_send(msg);
                true
            }
            None => false,
        }
    }

    fn online(&self) -> usize {
        self.users.lock().unwrap().len()
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut bind = format!("0.0.0.0:{}", proto::DEFAULT_PORT);
    let mut secret_path = PathBuf::from("daychat-server.secret");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-b" | "--bind" => bind = args.next().unwrap_or_else(|| usage()),
            "--secret-file" => secret_path = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage()
            }
        }
    }

    let secret = load_or_create_secret(&secret_path)?;
    let state = Arc::new(State {
        secret,
        users: Mutex::new(HashMap::new()),
    });
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("daychat-server listening on {bind}");

    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
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
        tokio::spawn(async move {
            if let Err(e) = handle(state, stream).await {
                eprintln!("connection error: {e}");
            }
            drop(permit);
        });
    }
}

fn load_or_create_secret(path: &Path) -> io::Result<[u8; 32]> {
    match std::fs::read(path) {
        Ok(bytes) => bytes.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "secret file must be exactly 32 bytes")
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut s = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut s);
            std::fs::write(path, s)?;
            eprintln!("created new server secret at {}", path.display());
            Ok(s)
        }
        Err(e) => Err(e),
    }
}

async fn reject(w: &mut OwnedWriteHalf, why: &str) -> io::Result<()> {
    let msg = ServerMsg::Error { text: why.into() };
    w.write_all(proto::encode(&msg).as_bytes()).await?;
    w.shutdown().await
}

async fn handle(state: Arc<State>, stream: TcpStream) -> io::Result<()> {
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
    let taken = {
        let mut users = state.users.lock().unwrap();
        if users.contains_key(&me) {
            true
        } else {
            users.insert(me.clone(), tx.clone());
            false
        }
    };
    if taken {
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
            _ = tick.tick() => {
                let today = id::today();
                if today == day {
                    continue;
                }
                // Day rolled over: swap to the new ID silently. No join/leave
                // broadcast, so others can't link yesterday's ID to today's.
                day = today;
                let new_id = id::derive_id(&state.secret, &day, &key);
                let ok = {
                    let mut users = state.users.lock().unwrap();
                    if users.contains_key(&new_id) {
                        false
                    } else {
                        users.remove(&me);
                        users.insert(new_id.clone(), tx.clone());
                        true
                    }
                };
                if !ok {
                    let _ = tx.try_send(ServerMsg::Error { text: "ID collision at day rollover; please reconnect".into() });
                    break Ok(());
                }
                me = new_id;
                let _ = tx.try_send(ServerMsg::Rotated { id: me.clone() });
            }
        }
    };

    // --- Cleanup -----------------------------------------------------------
    state.users.lock().unwrap().remove(&me);
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

fn process(state: &State, me: &str, msg: ClientMsg) -> Option<ServerMsg> {
    match msg {
        ClientMsg::Hello { .. } => Some(err("already connected")),
        ClientMsg::Who => {
            let mut ids: Vec<String> = state.users.lock().unwrap().keys().cloned().collect();
            ids.sort();
            let total = ids.len();
            ids.truncate(MAX_WHO);
            Some(ServerMsg::Who { ids, total })
        }
        ClientMsg::Say { text } => match limits::validate_text(&text, limits::MAX_CHARS) {
            Err(e) => Some(err(e)),
            Ok(text) => {
                state.broadcast(&ServerMsg::Chat { from: me.to_string(), text, ts: now() });
                None
            }
        },
        ClientMsg::Dm { to, text } => {
            let text = match limits::validate_text(&text, limits::MAX_CHARS) {
                Ok(t) => t,
                Err(e) => return Some(err(e)),
            };
            let to = id::normalize_id(&to);
            let dm = ServerMsg::Dm { from: me.to_string(), to: to.clone(), text, ts: now() };
            if to == me || state.send_to(&to, dm.clone()) {
                Some(dm) // echo back to sender
            } else {
                Some(err(format!("{} is not online", limits::sanitize(&to))))
            }
        }
    }
}
