//! daychat: interactive terminal shell.
//!
//! `daychat` opens the shell and joins the official server, the only server
//! it connects to (see `daychat::official`). Conversations and updates
//! happen inside via slash commands.
//!
//! Everything the server sends is treated as untrusted: its text is shown
//! labelled as coming from the server, and IDs that aren't well-formed are
//! dropped.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    time::Duration,
};

use daychat::{
    crypto::{self, Identity, KeyOffer, Session},
    id, limits,
    official::{self, Official},
    proto::{self, ClientMsg, ServerMsg},
    tls::{self, Pin},
    update,
};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use tokio::{
    io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::mpsc,
    task::JoinHandle,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const USAGE: &str = "usage: daychat [--fresh]

Opens the daychat shell and joins the official daychat server.
Type /help inside for commands.

  --fresh      start with a throwaway identity instead of your daily one";

const ACCENT: Color = Color::Indexed(173);
const MAX_LOG: usize = 2000;
const POPUP_ROWS: usize = 8;

// --- Commands ----------------------------------------------------------------

struct Command {
    name: &'static str,
    args: &'static str,
    about: &'static str,
}

const COMMANDS: &[Command] = &[
    Command { name: "connect", args: "<id>", about: "ask someone for a private conversation (or accept theirs)" },
    Command { name: "accept", args: "[id]", about: "accept a connection request (default: latest)" },
    Command { name: "decline", args: "[id]", about: "decline a connection request (default: latest)" },
    Command { name: "close", args: "[id]", about: "end a conversation or withdraw a request" },
    Command { name: "room", args: "", about: "talk in the public room (Tab cycles conversations)" },
    Command { name: "chat", args: "<id>", about: "switch to a conversation" },
    Command { name: "peers", args: "", about: "list conversations and pending requests" },
    Command { name: "verify", args: "[id]", about: "show a conversation's safety code to compare with them" },
    Command { name: "verified", args: "[id]", about: "mark a conversation's safety code as checked" },
    Command { name: "join", args: "", about: "(re)join the official server" },
    Command { name: "leave", args: "", about: "leave the server" },
    Command { name: "who", args: "", about: "list who is online" },
    Command { name: "id", args: "", about: "show your ID for today" },
    Command { name: "fresh", args: "", about: "toggle a throwaway identity for new connections" },
    Command { name: "clear", args: "", about: "clear the screen" },
    Command { name: "help", args: "", about: "show commands and keys" },
    Command { name: "update", args: "", about: "install the latest signed release and restart" },
    Command { name: "quit", args: "", about: "exit daychat" },
];

// --- Events and connection state -----------------------------------------------

enum AppEvent {
    Term(Event),
    Connected { gen: u64, addr: String, result: Result<Handshake, String> },
    Net { gen: u64, msg: ServerMsg },
    Closed { gen: u64, why: String },
    UpdateChecked(Result<update::Release, String>),
    UpdateFetched(Result<update::Ready, String>),
}

/// What we know about newer releases. Joining a server is blocked while a
/// newer release exists, until the user runs /update.
enum UpdateState {
    Disabled,
    Checking,
    UpToDate,
    Available(update::Release),
    /// Couldn't reach GitHub; joining is allowed (fail open).
    Unknown,
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;
type Reader = BufReader<ReadHalf<Tls>>;
type Writer = WriteHalf<Tls>;

struct Handshake {
    reader: Reader,
    writer: Writer,
    me: String,
    max_chars: usize,
    online: usize,
}

enum Link {
    Offline,
    Connecting {
        addr: String,
    },
    Online {
        addr: String,
        me: String,
        max_chars: usize,
        online: usize,
        writer: Writer,
        reader: JoinHandle<()>,
    },
}

// --- Entry point ---------------------------------------------------------------

#[tokio::main]
async fn main() {
    let mut fresh = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--fresh" => fresh = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            s if s.starts_with('-') => {
                eprintln!("daychat: unknown flag {s}\n\n{USAGE}");
                std::process::exit(2);
            }
            s => {
                eprintln!("daychat: '{s}': daychat only connects to the official server\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let term_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if term_tx.send(AppEvent::Term(ev)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(tx, fresh);
    app.check_for_update();
    app.join();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut rx).await;
    ratatui::restore();

    if let Err(e) = result {
        eprintln!("daychat: {e}");
        std::process::exit(1);
    }
    if let Some(ready) = app.install.take() {
        install_and_restart(&ready);
    }
}

/// Install a verified update with the terminal restored (pacman may ask for
/// a password), then start the new version in place of this one.
fn install_and_restart(ready: &update::Ready) {
    // Resolve our path before the file is replaced.
    let exe = std::env::current_exe();
    println!("installing daychat {} (signature verified)…", ready.tag);
    if let Err(e) = update::install(ready) {
        eprintln!("daychat: update failed: {e}");
        std::process::exit(1);
    }
    println!("updated to {}; restarting…", ready.tag);
    match exe {
        Ok(exe) => {
            let e = update::restart(&exe);
            eprintln!("daychat: updated, but could not restart ({e}); start daychat again");
        }
        Err(_) => println!("updated; start daychat again"),
    }
}

async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| app.draw(f))?;
        let Some(ev) = rx.recv().await else { return Ok(()) };
        app.on_event(ev).await;
        // Handle anything else already queued before redrawing.
        while !app.quit {
            match rx.try_recv() {
                Ok(ev) => app.on_event(ev).await,
                Err(_) => break,
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

// --- Networking helpers --------------------------------------------------------

/// Read one handshake frame from the server.
async fn read_frame(reader: &mut Reader) -> io::Result<ServerMsg> {
    let mut buf = Vec::new();
    let got = tokio::time::timeout(Duration::from_secs(10), proto::read_line_limited(reader, &mut buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))??;
    if !got {
        return Err(io::Error::other("server closed the connection"));
    }
    match serde_json::from_slice::<ServerMsg>(&buf) {
        Ok(ServerMsg::Error { text }) => Err(io::Error::other(format!("refused (server says: {})", limits::sanitize(&text)))),
        Ok(msg) => Ok(msg),
        Err(_) => Err(io::Error::other("unexpected handshake reply")),
    }
}

/// Connect over TLS (checking or pinning the certificate), then log in by
/// signing the server's challenge.
async fn handshake(addr: &str, identity: Identity, pin: Pin) -> io::Result<Handshake> {
    let timed_out = |what: &str| io::Error::new(io::ErrorKind::TimedOut, format!("{what} timed out"));
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .map_err(|_| timed_out("connect"))??;
    tcp.set_nodelay(true)?;

    // With an exact pin there's only one way the certificate can be trusted,
    // so the verifier's outcome slot isn't needed.
    let (config, _) = tls::client_config(pin)?;
    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_rustls::TlsConnector::from(config).connect(tls::server_name(addr)?, tcp),
    )
    .await
    .map_err(|_| timed_out("TLS handshake"))?
    .map_err(|e| match e.kind() {
        // Servers drop connections over their limits before TLS starts.
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => io::Error::other(
            "the server closed the connection (it may be full, limiting your address, or not a daychat v3 server)",
        ),
        _ => e,
    })?;

    let (r, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    let hello = ClientMsg::Hello { version: proto::PROTOCOL_VERSION, pubkey: identity.public() };
    writer.write_all(proto::encode(&hello).as_bytes()).await?;
    let ServerMsg::Challenge { challenge } = read_frame(&mut reader).await? else {
        return Err(io::Error::other("unexpected handshake reply"));
    };
    let auth = ClientMsg::Auth { answer: identity.answer(&challenge) };
    writer.write_all(proto::encode(&auth).as_bytes()).await?;

    match read_frame(&mut reader).await? {
        ServerMsg::Welcome { id, max_chars, online, .. } => Ok(Handshake {
            reader,
            writer,
            me: server_id(&id).ok_or_else(|| io::Error::other("the server assigned a malformed ID"))?,
            max_chars,
            online,
        }),
        _ => Err(io::Error::other("unexpected handshake reply")),
    }
}

fn spawn_reader(
    mut reader: Reader,
    gen: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let why = loop {
            match proto::read_line_limited(&mut reader, &mut buf).await {
                Ok(true) => {
                    // Unknown/malformed frames are ignored (forward compatibility).
                    if let Ok(msg) = serde_json::from_slice::<ServerMsg>(&buf) {
                        if tx.send(AppEvent::Net { gen, msg }).is_err() {
                            return;
                        }
                    }
                }
                Ok(false) => break "server closed the connection".to_string(),
                Err(e) => break e.to_string(),
            }
        };
        let _ = tx.send(AppEvent::Closed { gen, why });
    })
}

// --- Local files ---------------------------------------------------------------

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("daychat"))
}

fn load_key() -> io::Result<String> {
    let dir = config_dir().ok_or_else(|| io::Error::other("could not find a config directory"))?;
    let path = dir.join("key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if id::valid_client_key(&s) {
            return Ok(s);
        }
    }
    std::fs::create_dir_all(&dir)?;
    let key = id::new_client_key();
    std::fs::write(&path, &key)?;
    Ok(key)
}

/// Parse an ID argument, or fall back to `default` when none is given.
fn pick_id(args: &str, default: Option<String>, what: &str) -> Result<String, String> {
    if args.is_empty() {
        return default.ok_or_else(|| format!("no {what}"));
    }
    let id = id::normalize_id(args);
    if id::looks_like_id(&id) {
        Ok(id)
    } else {
        Err(format!("'{args}' is not an ID (expected xxxx-xxxx)"))
    }
}

/// An ID received from the server, if it is a well-formed `xxxx-xxxx` ID.
/// Anything else (a server trying to show "official-admin", say) is dropped.
fn server_id(raw: &str) -> Option<String> {
    let id = limits::sanitize(raw);
    id::looks_like_id(&id).then_some(id)
}

// --- Line builders -------------------------------------------------------------

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn info_line(text: String) -> Line<'static> {
    Line::styled(format!("  · {text}"), dim())
}

fn err_line(text: String) -> Line<'static> {
    Line::styled(format!("  ! {text}"), Style::new().fg(Color::Red))
}

/// Text the server sent, clearly labelled so it can never pass for
/// daychat's own messages.
fn server_line(text: &str, is_error: bool) -> Line<'static> {
    let color = if is_error { Color::Red } else { Color::Gray };
    Line::from(vec![
        Span::styled("  server says › ", Style::new().fg(Color::Blue).add_modifier(Modifier::ITALIC)),
        Span::styled(limits::sanitize(text), Style::new().fg(color)),
    ])
}

fn time_span(ts: i64) -> Span<'static> {
    let t = chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M ").to_string())
        .unwrap_or_default();
    Span::styled(t, dim())
}

/// Stable per-ID color so conversations are easier to follow.
fn id_style(id: &str) -> Style {
    const COLORS: [Color; 6] = [
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::LightRed,
        Color::LightGreen,
        Color::LightBlue,
    ];
    let h = id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    Style::new().fg(COLORS[h as usize % COLORS.len()])
}

fn banner() -> Vec<Line<'static>> {
    let accent = Style::new().fg(ACCENT);
    let join_rows = match official::server() {
        Some(o) if o.dev => vec![vec![Span::styled(
            format!("  DEVELOPMENT BUILD · server {}", o.addr),
            Style::new().fg(Color::Yellow),
        )]],
        Some(_) => vec![vec![Span::raw("  /join            "), Span::styled("rejoin the official server", dim())]],
        None => vec![vec![Span::styled("  this build has no official server configured", Style::new().fg(Color::Yellow))]],
    };
    let mut rows: Vec<Vec<Span<'static>>> = vec![
        vec![
            Span::styled("✻ ", accent),
            Span::styled(
                format!("daychat v{}", env!("CARGO_PKG_VERSION")),
                Style::new().add_modifier(Modifier::BOLD),
            ),
        ],
        vec![],
    ];
    rows.extend(join_rows);
    rows.extend(vec![
        vec![Span::raw("  /connect <id>    "), Span::styled("start a private conversation", dim())],
        vec![Span::raw("  /help            "), Span::styled("all commands and keys", dim())],
        vec![],
        vec![Span::styled("  plain text · 280 chars · IDs rotate daily at 00:00 UTC", dim())],
    ]);
    let width = |r: &[Span<'static>]| r.iter().map(|s| s.width()).sum::<usize>();
    let inner = rows.iter().map(|r| width(r.as_slice())).max().unwrap_or(0) + 2;

    let mut out = vec![Line::styled(format!("╭{}╮", "─".repeat(inner)), accent)];
    for r in rows {
        let pad = inner - 1 - width(r.as_slice());
        let mut spans = vec![Span::styled("│ ", accent)];
        spans.extend(r);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled("│", accent));
        out.push(Line::from(spans));
    }
    out.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), accent));
    out.push(Line::default());
    out
}

// --- The app -------------------------------------------------------------------

struct App {
    events: mpsc::UnboundedSender<AppEvent>,
    link: Link,
    /// Bumped on every connect/disconnect; stale network events are ignored.
    gen: u64,
    local_key: String,
    fresh_key: Option<String>,

    /// People we're in a private conversation with (on the current server).
    peers: Vec<String>,
    /// Requests from others, newest last.
    incoming: Vec<String>,
    /// Our requests that haven't been answered.
    outgoing: Vec<String>,
    /// Where plain text goes: a peer, or the public room (`None`).
    target: Option<String>,
    /// Our half of each key exchange we started, until the peer answers.
    offers: HashMap<String, KeyOffer>,
    /// End-to-end encrypted sessions with connected peers.
    sessions: HashMap<String, Session>,
    /// Key commitments from people who asked to connect, checked against
    /// the key they reveal.
    commits: HashMap<String, String>,
    /// Sessions we set up as the requester, until the server confirms the
    /// link (after our reveal).
    awaiting_link: HashMap<String, Session>,
    /// Peers whose safety code the user marked as checked (`/verified`).
    verified: HashSet<String>,
    /// Messages to send once the current event is handled.
    queued: Vec<ClientMsg>,

    log: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom of the log.
    scroll: u16,
    /// Largest useful `scroll`, as computed by the last draw.
    max_scroll: Cell<u16>,

    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    popup_sel: usize,

    update: UpdateState,
    fetching: bool,
    /// A join requested while the update check was still running.
    pending_join: Option<Official>,
    /// A verified update to install once the UI has shut down.
    install: Option<update::Ready>,

    exit_armed: bool,
    quit: bool,
}

impl App {
    fn new(events: mpsc::UnboundedSender<AppEvent>, fresh: bool) -> Self {
        let mut app = Self {
            events,
            link: Link::Offline,
            gen: 0,
            local_key: String::new(),
            fresh_key: fresh.then(id::new_client_key),
            peers: Vec::new(),
            incoming: Vec::new(),
            outgoing: Vec::new(),
            target: None,
            offers: HashMap::new(),
            sessions: HashMap::new(),
            commits: HashMap::new(),
            awaiting_link: HashMap::new(),
            verified: HashSet::new(),
            queued: Vec::new(),
            log: banner(),
            scroll: 0,
            max_scroll: Cell::new(0),
            input: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            hist_pos: None,
            popup_sel: 0,
            update: UpdateState::Checking,
            fetching: false,
            pending_join: None,
            install: None,
            exit_armed: false,
            quit: false,
        };
        match load_key() {
            Ok(k) => app.local_key = k,
            Err(e) => {
                app.local_key = id::new_client_key();
                app.err(format!("could not load your identity key ({e}); using a throwaway one"));
            }
        }
        app
    }

    // --- log helpers -----------------------------------------------------------

    fn push(&mut self, line: Line<'static>) {
        self.log.push(line);
        if self.log.len() > MAX_LOG {
            self.log.drain(..self.log.len() - MAX_LOG);
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        self.push(info_line(text.into()));
    }

    fn err(&mut self, text: impl Into<String>) {
        self.push(err_line(text.into()));
    }

    fn help(&mut self) {
        self.info("commands:");
        for c in COMMANDS {
            let left = format!("/{} {}", c.name, c.args);
            self.info(format!("  {left:<28}{}", c.about));
        }
        self.info("  //text                      send text that starts with '/'");
        self.info("private chat: /connect <id> sends a request; they /accept (or /connect you back)");
        self.info("private conversations are end-to-end encrypted; /verify shows the safety code to compare, /verified marks it checked");
        self.info("keys: Tab completes commands or cycles room/conversations · ↑/↓ history · PgUp/PgDn scroll");
        self.info("      Esc clears · Ctrl-C twice exits");
    }

    // --- conversations ---------------------------------------------------------

    fn forget_peers(&mut self) {
        self.peers.clear();
        self.incoming.clear();
        self.outgoing.clear();
        self.target = None;
        self.offers.clear();
        self.sessions.clear();
        self.commits.clear();
        self.awaiting_link.clear();
        self.verified.clear();
    }

    /// Ask someone to connect (sending a commitment to a fresh key), or, if
    /// they already asked us, accept (sending the key itself).
    async fn request(&mut self, args: &str, default: Option<String>, what: &str) {
        let to = match pick_id(args, default, what) {
            Ok(id) => id,
            Err(e) => return self.err(e),
        };
        // Reuse an offer we already sent, so a repeated command can't make
        // our key differ from the one we committed to.
        let offer = self.offers.entry(to.clone()).or_default();
        let msg = if self.commits.contains_key(&to) {
            ClientMsg::Accept { to, key: offer.public() }
        } else {
            ClientMsg::Connect { to, commit: offer.commitment() }
        };
        self.send(msg).await;
    }

    /// Encrypt and send to a connected peer, showing our own copy.
    async fn send_private(&mut self, to: String, text: String) {
        let text = match limits::validate_text(&text, self.max_chars()) {
            Ok(t) => t,
            Err(e) => return self.err(e),
        };
        let Some(session) = self.sessions.get(&to) else {
            return self.err(format!("no encrypted session with {to}"));
        };
        let sealed = session.seal(&text);
        self.send(ClientMsg::Dm { to: to.clone(), sealed }).await;
        if self.is_online() {
            let magenta = Style::new().fg(Color::Magenta);
            self.push(Line::from(vec![
                time_span(chrono::Utc::now().timestamp()),
                Span::styled(format!("[you → {to}]"), magenta.add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(text, magenta),
            ]));
        }
    }

    /// Cycle where plain text goes: room → each conversation → room.
    fn cycle_target(&mut self) {
        if self.peers.is_empty() {
            return;
        }
        self.target = match &self.target {
            None => Some(self.peers[0].clone()),
            Some(t) => self.peers.iter().position(|p| p == t).and_then(|i| self.peers.get(i + 1)).cloned(),
        };
    }

    /// Send a connect/hangup-style command for the ID in `args`, or for the
    /// newest entry of `default` when no ID is given.
    async fn send_for(&mut self, args: &str, default: Option<String>, what: &str, make: fn(String) -> ClientMsg) {
        match pick_id(args, default, what) {
            Ok(id) => self.send(make(id)).await,
            Err(e) => self.err(e),
        }
    }

    // --- connection management -------------------------------------------------

    fn max_chars(&self) -> usize {
        match &self.link {
            Link::Online { max_chars, .. } => *max_chars,
            _ => limits::MAX_CHARS,
        }
    }

    fn is_online(&self) -> bool {
        matches!(self.link, Link::Online { .. })
    }

    fn drop_link(&mut self) {
        self.gen += 1;
        self.forget_peers();
        if let Link::Online { reader, .. } = std::mem::replace(&mut self.link, Link::Offline) {
            reader.abort(); // the writer half is dropped with the enum, closing the socket
        }
    }

    fn lost(&mut self, why: String) {
        self.drop_link();
        self.err(format!("disconnected: {why}"));
    }

    // --- updates -----------------------------------------------------------------

    fn check_for_update(&mut self) {
        if std::env::var_os(update::DISABLE_ENV).is_some() {
            self.update = UpdateState::Disabled;
            return;
        }
        self.update = UpdateState::Checking;
        let tx = self.events.clone();
        tokio::spawn(async move {
            let r = tokio::task::spawn_blocking(update::latest).await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = tx.send(AppEvent::UpdateChecked(r));
        });
    }

    fn on_update_checked(&mut self, result: Result<update::Release, String>) {
        self.update = match result {
            Ok(r) if update::is_newer(&r.tag) => {
                self.push(Line::styled(
                    format!("  ◆ daychat {} is available (you have v{}) · run /update; it's required to join servers", r.tag, update::CURRENT),
                    Style::new().fg(Color::Yellow),
                ));
                UpdateState::Available(r)
            }
            Ok(_) => UpdateState::UpToDate,
            Err(e) => {
                self.info(format!("could not check for updates ({e})"));
                UpdateState::Unknown
            }
        };
        if let Some(server) = self.pending_join.take() {
            self.start_connect(server);
        }
    }

    fn run_update(&mut self) {
        if self.fetching {
            return self.err("already downloading an update");
        }
        match &self.update {
            UpdateState::Available(r) => {
                let r = r.clone();
                self.fetching = true;
                self.info(format!("downloading daychat {} and checking its signature…", r.tag));
                let tx = self.events.clone();
                tokio::spawn(async move {
                    let res = tokio::task::spawn_blocking(move || update::fetch(&r))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                    let _ = tx.send(AppEvent::UpdateFetched(res));
                });
            }
            UpdateState::Checking => self.info("still checking for updates; try again in a moment"),
            UpdateState::UpToDate => self.info(format!("daychat v{} is the latest version", update::CURRENT)),
            UpdateState::Unknown => {
                self.info("checking for updates again; run /update once it finishes");
                self.check_for_update();
            }
            UpdateState::Disabled => self.info(format!("update checks are disabled ({} is set)", update::DISABLE_ENV)),
        }
    }

    fn on_update_fetched(&mut self, result: Result<update::Ready, String>) {
        self.fetching = false;
        match result {
            Ok(ready) => {
                self.info(format!("daychat {} downloaded and verified; installing…", ready.tag));
                self.install = Some(ready);
                self.quit = true;
            }
            Err(e) => self.err(format!("update failed: {e}")),
        }
    }

    /// Join the official server (the only server daychat connects to).
    fn join(&mut self) {
        match official::server() {
            Some(server) => self.start_connect(server),
            None => self.err("this build of daychat has no official server configured"),
        }
    }

    /// Join, unless a required update is pending (then explain), or the
    /// update check hasn't finished yet (then join once it has).
    fn start_connect(&mut self, server: Official) {
        match &self.update {
            UpdateState::Available(r) => {
                let tag = r.tag.clone();
                self.err(format!("daychat {tag} is required to join; run /update first"));
            }
            UpdateState::Checking => {
                self.info("checking for updates before joining…");
                self.pending_join = Some(server);
            }
            UpdateState::Disabled | UpdateState::UpToDate | UpdateState::Unknown => self.connect_now(server),
        }
    }

    fn connect_now(&mut self, server: Official) {
        self.drop_link();
        let gen = self.gen;
        let addr = server.addr;
        let local = self.fresh_key.as_ref().unwrap_or(&self.local_key);
        let identity = Identity::for_server(local, &addr);
        // Only the built-in certificate is accepted: no first-use trust, and
        // not even a certificate from a public CA.
        let pin = Pin::Exact(server.fingerprint);
        if server.dev {
            self.err(format!("DEVELOPMENT BUILD: connecting to {addr} instead of the official server"));
        } else {
            self.info("connecting to the official server…");
        }
        self.link = Link::Connecting { addr: addr.clone() };
        let tx = self.events.clone();
        tokio::spawn(async move {
            let result = handshake(&addr, identity, pin).await.map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Connected { gen, addr, result });
        });
    }

    fn on_connected(&mut self, gen: u64, addr: String, result: Result<Handshake, String>) {
        if gen != self.gen {
            return; // superseded by a newer /join or /leave
        }
        match result {
            Err(e) => {
                self.link = Link::Offline;
                self.err(format!("could not connect to the official server: {e}"));
            }
            Ok(h) => {
                let reader = spawn_reader(h.reader, gen, self.events.clone());
                self.info(format!(
                    "connected to the official server (certificate verified) as {} · {} online",
                    h.me, h.online
                ));
                self.link = Link::Online {
                    addr,
                    me: h.me,
                    max_chars: h.max_chars,
                    online: h.online,
                    writer: h.writer,
                    reader,
                };
            }
        }
    }

    // --- sending ---------------------------------------------------------------

    async fn send(&mut self, msg: ClientMsg) {
        let Link::Online { writer, .. } = &mut self.link else {
            return self.err("not connected: /join to rejoin the official server");
        };
        let res = writer.write_all(proto::encode(&msg).as_bytes()).await;
        if let Err(e) = res {
            self.lost(e.to_string());
        }
    }

    /// Client-side length check for instant feedback, then send.
    async fn send_text(&mut self, msg: ClientMsg) {
        let check = match &msg {
            ClientMsg::Say { text } if self.is_online() => {
                limits::validate_text(text, self.max_chars()).map(|_| ())
            }
            _ => Ok(()), // offline: send() reports "not connected"
        };
        match check {
            Ok(()) => self.send(msg).await,
            Err(e) => self.err(e),
        }
    }

    // --- events ----------------------------------------------------------------

    async fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => self.on_key(k).await,
            AppEvent::Term(_) => {} // resize etc.: just redraw
            AppEvent::Connected { gen, addr, result } => self.on_connected(gen, addr, result),
            AppEvent::Net { gen, msg } => {
                if gen == self.gen {
                    self.on_server(msg);
                    for m in std::mem::take(&mut self.queued) {
                        self.send(m).await;
                    }
                }
            }
            AppEvent::Closed { gen, why } => {
                if gen == self.gen && self.is_online() {
                    self.lost(why);
                }
            }
            AppEvent::UpdateChecked(r) => self.on_update_checked(r),
            AppEvent::UpdateFetched(r) => self.on_update_fetched(r),
        }
    }

    fn on_server(&mut self, m: ServerMsg) {
        use limits::sanitize as clean;
        let Link::Online { me, online, max_chars, .. } = &mut self.link else { return };
        let max_chars = *max_chars;
        let mut out: Vec<Line<'static>> = Vec::new();
        // Every ID from the server must be a well-formed ID; frames carrying
        // anything else are dropped rather than shown.
        macro_rules! id_or_drop {
            ($raw:expr) => {
                match server_id(&$raw) {
                    Some(id) => id,
                    None => return,
                }
            };
        }
        match m {
            ServerMsg::Chat { from, text, ts } => {
                let from = id_or_drop!(from);
                let style = if from == *me {
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    id_style(&from)
                };
                out.push(Line::from(vec![
                    time_span(ts),
                    Span::styled(from, style),
                    Span::raw("  "),
                    Span::raw(clean(&text)),
                ]));
            }
            ServerMsg::Dm { from, sealed, ts } => {
                let from = id_or_drop!(from);
                // Decrypted text is untrusted too: clean and length-check it.
                let text = self
                    .sessions
                    .get(&from)
                    .and_then(|s| s.open(&sealed))
                    .and_then(|t| limits::validate_text(&t, max_chars).ok());
                match text {
                    Some(text) => {
                        let magenta = Style::new().fg(Color::Magenta);
                        out.push(Line::from(vec![
                            time_span(ts),
                            Span::styled(format!("[{from}]"), magenta.add_modifier(Modifier::BOLD)),
                            Span::raw("  "),
                            Span::styled(text, magenta),
                        ]));
                    }
                    None => out.push(err_line(format!(
                        "dropped a private message from {from} that failed to decrypt or verify"
                    ))),
                }
            }
            ServerMsg::Joined { id, online: total } => {
                let id = id_or_drop!(id);
                // Older servers send no total (0): count instead.
                *online = if total > 0 { total } else { *online + 1 };
                if id != *me {
                    out.push(info_line(format!("{id} joined")));
                }
            }
            ServerMsg::Left { id, online: total } => {
                let id = id_or_drop!(id);
                *online = if total > 0 { total } else { online.saturating_sub(1) };
                out.push(info_line(format!("{id} left")));
            }
            ServerMsg::Online { total } => *online = total,
            ServerMsg::Rotated { id } => {
                *me = id_or_drop!(id);
                out.push(info_line(format!("new day: your ID is now {me}; conversations were reset")));
                self.peers.clear();
                self.incoming.clear();
                self.outgoing.clear();
                self.target = None;
                self.offers.clear();
                self.sessions.clear();
                self.commits.clear();
                self.awaiting_link.clear();
                self.verified.clear();
            }
            ServerMsg::Request { from, commit } => {
                let from = id_or_drop!(from);
                if !self.incoming.contains(&from) {
                    self.incoming.push(from.clone());
                }
                self.commits.insert(from.clone(), commit);
                out.push(Line::from(vec![
                    Span::styled("  ◆ ", Style::new().fg(ACCENT)),
                    Span::styled(from.clone(), id_style(&from).add_modifier(Modifier::BOLD)),
                    Span::raw(" wants to connect · "),
                    Span::styled("/accept", Style::new().fg(ACCENT)),
                    Span::raw(" or "),
                    Span::styled("/decline", Style::new().fg(ACCENT)),
                ]));
            }
            ServerMsg::Requested { to } => {
                let to = id_or_drop!(to);
                if !self.outgoing.contains(&to) {
                    self.outgoing.push(to.clone());
                }
                out.push(info_line(format!("request sent to {to}; waiting for them to accept")));
            }
            ServerMsg::Accepted { peer, key } => {
                // We asked; they answered with their key. Set up the session
                // and reveal the key we committed to.
                let peer = id_or_drop!(peer);
                let Some(offer) = self.offers.remove(&peer) else { return };
                let mine = offer.public();
                match offer.finish(&key) {
                    Some(session) => {
                        self.awaiting_link.insert(peer.clone(), session);
                        self.queued.push(ClientMsg::Reveal { to: peer.clone(), key: mine });
                        out.push(info_line(format!("{peer} accepted; setting up encryption…")));
                    }
                    None => {
                        self.queued.push(ClientMsg::Hangup { peer: peer.clone() });
                        out.push(err_line(format!("could not set up encryption with {peer}; ending the conversation")));
                    }
                }
            }
            ServerMsg::Linked { peer, key } => {
                let peer = id_or_drop!(peer);
                self.incoming.retain(|p| *p != peer);
                self.outgoing.retain(|p| *p != peer);
                // As the requester, the session was set up on `accepted`. As the
                // acceptor, the revealed key must match the commitment from the
                // request: that is what stops a server from searching for keys
                // that give both sides the same safety code.
                let session = match self.awaiting_link.remove(&peer) {
                    Some(session) => Some(session),
                    None => match self.commits.remove(&peer) {
                        Some(commit) if crypto::commitment_matches(&commit, &key) => {
                            self.offers.remove(&peer).and_then(|offer| offer.finish(&key))
                        }
                        _ => None,
                    },
                };
                let Some(session) = session else {
                    // Without a sound key exchange there is no private
                    // channel: end the conversation rather than fall back.
                    self.offers.remove(&peer);
                    self.queued.push(ClientMsg::Hangup { peer: peer.clone() });
                    out.push(err_line(format!(
                        "could not set up a trustworthy encrypted channel with {peer}; ending the conversation"
                    )));
                    for l in out {
                        self.push(l);
                    }
                    return;
                };
                let code = session.safety_code.clone();
                self.sessions.insert(peer.clone(), session);
                self.verified.remove(&peer);
                if !self.peers.contains(&peer) {
                    self.peers.push(peer.clone());
                }
                self.target = Some(peer.clone());
                let magenta = Style::new().fg(Color::Magenta);
                let yellow = Style::new().fg(Color::Yellow);
                out.push(Line::from(vec![
                    Span::styled("  ◆ connected with ", magenta),
                    Span::styled(peer, magenta.add_modifier(Modifier::BOLD)),
                    Span::styled(" · end-to-end encrypted · your messages now go to them · Tab switches to the room", dim()),
                ]));
                out.push(Line::from(vec![
                    Span::styled("    safety code ", yellow),
                    Span::styled(code, yellow.add_modifier(Modifier::BOLD)),
                    Span::styled(
                        " · compare it with them by phone or in person, then /verified. A mismatch means someone is reading along.",
                        yellow,
                    ),
                ]));
            }
            ServerMsg::Unlinked { peer, reason } => {
                let peer = id_or_drop!(peer);
                self.offers.remove(&peer);
                self.sessions.remove(&peer);
                self.commits.remove(&peer);
                self.awaiting_link.remove(&peer);
                self.verified.remove(&peer);
                self.peers.retain(|p| *p != peer);
                self.incoming.retain(|p| *p != peer);
                self.outgoing.retain(|p| *p != peer);
                if self.target.as_deref() == Some(peer.as_str()) {
                    self.target = None;
                    out.push(info_line(format!("no longer connected with {peer}; now talking in the room")));
                } else {
                    out.push(info_line(format!("no longer connected with {peer}")));
                }
                // The reason is server text: label it as such.
                out.push(server_line(&reason, false));
            }
            ServerMsg::Who { ids, total } => {
                *online = total;
                let list: Vec<String> = ids.iter().filter_map(|s| server_id(s)).collect();
                let more = if total > list.len() { ", …" } else { "" };
                out.push(info_line(format!("{total} online: {}{more}", list.join(", "))));
            }
            ServerMsg::Info { text } => out.push(server_line(&text, false)),
            ServerMsg::Error { text } => out.push(server_line(&text, true)),
            // Handshake frames; not expected once logged in.
            ServerMsg::Welcome { .. } | ServerMsg::Challenge { .. } => {}
        }
        for l in out {
            self.push(l);
        }
    }

    // --- input -----------------------------------------------------------------

    /// Commands matching what's typed, while typing a command name.
    fn popup_matches(&self) -> Vec<&'static Command> {
        if self.input.first() != Some(&'/') || self.input.iter().any(|c| c.is_whitespace()) {
            return Vec::new();
        }
        let typed: String = self.input[1..].iter().collect();
        if typed.starts_with('/') {
            return Vec::new();
        }
        COMMANDS.iter().filter(|c| c.name.starts_with(typed.as_str())).collect()
    }

    fn set_input(&mut self, s: &str) {
        self.input = s.chars().collect();
        self.cursor = self.input.len();
        self.popup_sel = 0;
    }

    fn clear_input(&mut self) {
        self.set_input("");
        self.hist_pos = None;
    }

    fn edited(&mut self) {
        self.popup_sel = 0;
        self.hist_pos = None;
    }

    fn complete(&mut self, c: &Command) {
        self.set_input(&format!("/{} ", c.name));
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = match self.hist_pos {
            None => self.history.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        let s = self.history[i].clone();
        self.set_input(&s);
        self.hist_pos = Some(i);
    }

    fn history_next(&mut self) {
        let Some(i) = self.hist_pos else { return };
        if i + 1 < self.history.len() {
            let s = self.history[i + 1].clone();
            self.set_input(&s);
            self.hist_pos = Some(i + 1);
        } else {
            self.clear_input();
        }
    }

    async fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if !(ctrl && k.code == KeyCode::Char('c')) {
            self.exit_armed = false;
        }
        let popup = self.popup_matches();
        let selected = popup.get(self.popup_sel.min(popup.len().saturating_sub(1))).copied();

        match k.code {
            KeyCode::Char('c') if ctrl => {
                if !self.input.is_empty() {
                    self.clear_input();
                } else if self.exit_armed {
                    self.quit = true;
                } else {
                    self.exit_armed = true;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.quit = true;
                }
            }
            KeyCode::Char('u') if ctrl => self.clear_input(),
            KeyCode::Char('l') if ctrl => self.log.clear(),
            KeyCode::Char(c) if !ctrl => {
                // Generous hard cap; the real limit is checked on submit.
                if !limits::is_forbidden(c) && self.input.len() < self.max_chars() + 64 {
                    self.input.insert(self.cursor, c);
                    self.cursor += 1;
                    self.edited();
                }
            }
            KeyCode::Tab => match selected {
                Some(c) => self.complete(c),
                None => self.cycle_target(),
            },
            // While browsing history, ↑/↓ keep walking it even if the recalled
            // entry is a command that opens the popup.
            KeyCode::Up if !popup.is_empty() && self.hist_pos.is_none() => {
                self.popup_sel = (self.popup_sel + popup.len() - 1) % popup.len();
            }
            KeyCode::Down if !popup.is_empty() && self.hist_pos.is_none() => {
                self.popup_sel = (self.popup_sel + 1) % popup.len();
            }
            KeyCode::Enter => match selected {
                // Typing a command name: run it if it needs no arguments (or is
                // already complete), otherwise complete it and wait for args.
                Some(c) => {
                    let typed: String = self.input[1..].iter().collect();
                    if c.args.is_empty() || typed == c.name {
                        self.set_input(&format!("/{}", c.name));
                        self.submit().await;
                    } else {
                        self.complete(c);
                    }
                }
                None => self.submit().await,
            },
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::Esc => self.clear_input(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
                self.edited();
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
                self.edited();
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5).min(self.max_scroll.get()),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            _ => {}
        }
    }

    async fn submit(&mut self) {
        let raw: String = self.input.iter().collect();
        self.clear_input();
        self.scroll = 0;
        let line = raw.trim().to_string();
        if line.is_empty() {
            return;
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }

        let text = match line.strip_prefix("//") {
            Some(rest) => Some(format!("/{rest}")),
            None if !line.starts_with('/') => Some(line.clone()),
            None => None,
        };
        if let Some(text) = text {
            return match self.target.clone() {
                Some(to) => self.send_private(to, text).await,
                None => self.send_text(ClientMsg::Say { text }).await,
            };
        }
        let cmd = &line[1..];

        self.push(Line::styled(format!("> {line}"), dim()));
        let (name, args) = match cmd.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (cmd, ""),
        };
        match name {
            "c" | "connect" => {
                if args.is_empty() {
                    return self.err("usage: /connect <id>");
                }
                self.request(args, None, "ID given").await;
            }
            "a" | "accept" => {
                let latest = self.incoming.last().cloned();
                self.request(args, latest, "pending requests").await;
            }
            "verify" => {
                let current = self.target.clone();
                match pick_id(args, current, "conversation selected") {
                    Ok(id) => match self.sessions.get(&id).map(|s| s.safety_code.clone()) {
                        Some(code) => {
                            self.info(format!("safety code with {id}: {code}"));
                            self.info("if they see the same code, nobody can read this conversation but the two of you");
                        }
                        None => self.err(format!("no encrypted conversation with {id}")),
                    },
                    Err(e) => self.err(e),
                }
            }
            "verified" => {
                let current = self.target.clone();
                match pick_id(args, current, "conversation selected") {
                    Ok(id) if self.sessions.contains_key(&id) => {
                        self.info(format!("marked your conversation with {id} as verified"));
                        self.verified.insert(id);
                    }
                    Ok(id) => self.err(format!("no encrypted conversation with {id}")),
                    Err(e) => self.err(e),
                }
            }
            "decline" => {
                let latest = self.incoming.last().cloned();
                self.send_for(args, latest, "pending requests", |peer| ClientMsg::Hangup { peer }).await;
            }
            "close" => {
                let current = self.target.clone().or_else(|| self.outgoing.last().cloned());
                self.send_for(args, current, "conversation selected", |peer| ClientMsg::Hangup { peer }).await;
            }
            "room" => {
                self.target = None;
                self.info("now talking in the public room");
            }
            "chat" => match pick_id(args, None, "ID given") {
                Ok(id) if self.peers.contains(&id) => {
                    self.info(format!("now talking with {id}"));
                    self.target = Some(id);
                }
                Ok(id) => self.err(format!("not connected with {id}; /connect {id} first")),
                Err(e) => self.err(e),
            },
            "peers" => {
                if self.peers.is_empty() && self.incoming.is_empty() && self.outgoing.is_empty() {
                    self.info("no conversations or requests; /connect <id> to start one");
                }
                let lines: Vec<String> = self
                    .peers
                    .iter()
                    .map(|p| {
                        let mark = if self.target.as_ref() == Some(p) { " (active)" } else { "" };
                        format!("connected with {p}{mark}")
                    })
                    .chain(self.incoming.iter().map(|p| format!("{p} wants to connect (/accept {p})")))
                    .chain(self.outgoing.iter().map(|p| format!("waiting for {p} to accept")))
                    .collect();
                for l in lines {
                    self.info(l);
                }
            }
            "j" | "join" => {
                if args.is_empty() {
                    self.join();
                } else {
                    self.err("daychat only connects to the official server; just /join");
                }
            }
            "leave" | "disconnect" => {
                if matches!(self.link, Link::Offline) {
                    self.err("not connected");
                } else {
                    self.drop_link();
                    self.info("left the server");
                }
            }
            "w" | "who" => self.send(ClientMsg::Who).await,
            "id" | "me" => match &self.link {
                Link::Online { me, .. } => {
                    let m = me.clone();
                    self.info(format!("your ID today is {m}"));
                }
                _ => self.err("not connected; your ID is assigned by each server"),
            },
            "fresh" => {
                if self.fresh_key.take().is_some() {
                    self.info("new connections will use your daily identity");
                } else {
                    self.fresh_key = Some(id::new_client_key());
                    self.info("new connections will use a throwaway identity (current connection unchanged)");
                }
            }
            "clear" => self.log.clear(),
            "h" | "help" | "?" => self.help(),
            "update" => self.run_update(),
            "q" | "quit" | "exit" => self.quit = true,
            other => self.err(format!("unknown command /{other}; try /help")),
        }
    }

    /// Length of the message body the counter should show.
    fn body_len(&self) -> usize {
        let s: String = self.input.iter().collect();
        if s.starts_with("//") {
            return s.chars().count() - 1;
        }
        if s.starts_with('/') {
            0
        } else {
            s.trim().chars().count()
        }
    }

    // --- rendering -------------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let popup = self.popup_matches();
        let below = if popup.is_empty() { 1 } else { popup.len().min(POPUP_ROWS) as u16 };
        let [log_area, input_area, below_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(below),
        ])
        .areas(f.area());

        self.draw_log(f, log_area);
        self.draw_input(f, input_area);
        if popup.is_empty() {
            self.draw_footer(f, below_area);
        } else {
            self.draw_popup(f, below_area, &popup);
        }
    }

    fn draw_log(&self, f: &mut Frame, area: Rect) {
        let para = Paragraph::new(self.log.clone()).wrap(Wrap { trim: false });
        let total = para.line_count(area.width).min(u16::MAX as usize) as u16;
        let max_scroll = total.saturating_sub(area.height);
        self.max_scroll.set(max_scroll);
        let top = max_scroll - self.scroll.min(max_scroll);
        f.render_widget(para.scroll((top, 0)), area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let border = if self.input.first() == Some(&'/') {
            ACCENT
        } else if self.target.is_some() {
            Color::Magenta
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let [prompt_area, text_area] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
        f.render_widget(Paragraph::new(Span::styled("> ", Style::new().fg(ACCENT))), prompt_area);

        if self.input.is_empty() {
            let hint = match (&self.target, self.is_online()) {
                (Some(peer), _) => format!("message {peer} · Tab switches conversation"),
                (None, true) => "message the room · /connect <id> for a private chat · / for commands".into(),
                (None, false) => "not connected · /join to rejoin the official server".into(),
            };
            f.render_widget(Paragraph::new(Span::styled(hint, dim())), text_area);
            f.set_cursor_position(Position::new(text_area.x, text_area.y));
            return;
        }

        let before: String = self.input[..self.cursor].iter().collect();
        let cur_x = before.width();
        // Scroll horizontally by whole chars, so a double-width char is never
        // cut in half and the cursor stays in step with the text.
        let under_w = self.input.get(self.cursor).and_then(|c| c.width()).unwrap_or(1).max(1);
        let need = (cur_x + under_w).saturating_sub(text_area.width as usize);
        let (mut skip_w, mut skip_n) = (0, 0);
        while skip_w < need {
            skip_w += self.input[skip_n].width().unwrap_or(0);
            skip_n += 1;
        }
        let text: String = self.input[skip_n..].iter().collect();
        f.render_widget(Paragraph::new(text), text_area);
        f.set_cursor_position(Position::new(text_area.x + (cur_x - skip_w) as u16, text_area.y));
    }

    fn draw_popup(&self, f: &mut Frame, area: Rect, popup: &[&'static Command]) {
        let rows = (area.height as usize).max(1);
        let sel = self.popup_sel.min(popup.len() - 1);
        let start = sel.saturating_sub(rows - 1);
        let col = popup.iter().map(|c| c.name.len() + c.args.len() + 5).max().unwrap_or(0);
        let lines: Vec<Line> = popup
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .map(|(i, c)| {
                let left = format!("  /{} {}", c.name, c.args);
                let style = if i == sel {
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                Line::from(vec![Span::styled(format!("{left:<col$}"), style), Span::styled(c.about, dim())])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let mut left = vec![Span::raw(" ")];
        // Name the server only in development builds; normally it is always
        // the official one.
        let dev = official::server().is_some_and(|o| o.dev);
        let server = |addr: &str| if dev { format!("DEV SERVER {addr}") } else { "official server".to_string() };
        match &self.link {
            Link::Offline => left.push(Span::styled("○ offline", dim())),
            Link::Connecting { addr } => left.push(Span::styled(
                format!("◌ connecting to the {}…", server(addr)),
                Style::new().fg(Color::Yellow),
            )),
            Link::Online { addr, me, online, .. } => {
                left.push(Span::styled(format!("● {me}"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
                let style = if dev { Style::new().fg(Color::Yellow) } else { dim() };
                left.push(Span::styled(format!(" on the {} · {online} online", server(addr)), style));
            }
        }
        if self.is_online() {
            match &self.target {
                Some(peer) => {
                    let magenta = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
                    left.push(Span::styled(format!(" · to {peer} (encrypted"), magenta));
                    if self.verified.contains(peer) {
                        left.push(Span::styled(", verified)", magenta));
                    } else {
                        left.push(Span::styled(", not verified: /verify", Style::new().fg(Color::Yellow)));
                        left.push(Span::styled(")", magenta));
                    }
                }
                None => left.push(Span::styled(" · to room", dim())),
            }
        }
        if !self.incoming.is_empty() {
            let n = self.incoming.len();
            let s = if n == 1 { "" } else { "s" };
            left.push(Span::styled(format!(" · {n} request{s} (/accept)"), Style::new().fg(ACCENT)));
        }
        if matches!(self.update, UpdateState::Available(_)) {
            left.push(Span::styled(" · update required (/update)", Style::new().fg(Color::Yellow)));
        }
        if self.fresh_key.is_some() {
            left.push(Span::styled(" · fresh identity", Style::new().fg(Color::Yellow)));
        }
        if self.scroll > 0 {
            left.push(Span::styled(" · scrolled (PgDn)", Style::new().fg(Color::Yellow)));
        }

        let body = self.body_len();
        let left = Line::from(left);
        let right = if self.exit_armed {
            Span::styled("press Ctrl-C again to exit ", Style::new().fg(Color::Yellow))
        } else if self.is_online() && body > 0 {
            let max = self.max_chars();
            let style = if body > max { Style::new().fg(Color::Red).add_modifier(Modifier::BOLD) } else { dim() };
            Span::styled(format!("{body}/{max} "), style)
        } else if left.width() + 32 <= area.width as usize {
            Span::styled("/ for commands · ctrl+c to exit ", dim())
        } else {
            Span::raw("") // the generic hint gives way to the status on the left
        };

        // Give the right side its own column so the two never overlap.
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right.width() as u16)]).areas(area);
        f.render_widget(Paragraph::new(left), left_area);
        f.render_widget(Paragraph::new(Line::from(right).right_aligned()), right_area);
    }
}
