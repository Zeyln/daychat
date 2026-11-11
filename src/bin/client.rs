//! daychat: terminal client.

use std::{io, time::Duration};

use daychat::{
    id, limits,
    proto::{self, ClientMsg, ServerMsg},
};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpStream},
    sync::mpsc,
};
use unicode_width::UnicodeWidthStr;

const USAGE: &str = "usage: daychat [HOST[:PORT]] [--fresh]

  HOST[:PORT]  server to connect to (default 127.0.0.1:7878)
  --fresh      use a throwaway key: random ID, not stable for the day";

const MAX_LOG: usize = 2000;

enum AppEvent {
    Term(Event),
    Net(ServerMsg),
    Disconnected(String),
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("daychat: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> io::Result<()> {
    let mut addr = format!("127.0.0.1:{}", proto::DEFAULT_PORT);
    let mut fresh = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--fresh" => fresh = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            s if s.starts_with('-') => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unknown flag {s}\n{USAGE}")))
            }
            s if s.contains(':') => addr = s.to_string(),
            s => addr = format!("{s}:{}", proto::DEFAULT_PORT),
        }
    }

    let local_key = if fresh { id::new_client_key() } else { load_key()? };
    let key = id::server_key(&local_key, &addr);

    // --- Connect + handshake ---------------------------------------------
    let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
    stream.set_nodelay(true)?;
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    send(&mut w, &ClientMsg::Hello { version: proto::PROTOCOL_VERSION, key }).await?;
    let mut buf = Vec::new();
    let got = tokio::time::timeout(Duration::from_secs(10), proto::read_line_limited(&mut reader, &mut buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))??;
    if !got {
        return Err(io::Error::other("server closed the connection"));
    }
    let (me, max_chars, online) = match serde_json::from_slice::<ServerMsg>(&buf) {
        Ok(ServerMsg::Welcome { id, max_chars, online, .. }) => (limits::sanitize(&id), max_chars, online),
        Ok(ServerMsg::Error { text }) => {
            return Err(io::Error::other(format!("server refused: {}", limits::sanitize(&text))))
        }
        _ => return Err(io::Error::other("unexpected handshake reply")),
    };

    // --- Event sources -----------------------------------------------------
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AppEvent>();
    let net_tx = ev_tx.clone();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        loop {
            match proto::read_line_limited(&mut reader, &mut buf).await {
                Ok(true) => {
                    // Unknown/malformed frames are ignored (forward compatibility).
                    if let Ok(m) = serde_json::from_slice::<ServerMsg>(&buf) {
                        if net_tx.send(AppEvent::Net(m)).is_err() {
                            break;
                        }
                    }
                }
                Ok(false) => {
                    let _ = net_tx.send(AppEvent::Disconnected("server closed the connection".into()));
                    break;
                }
                Err(e) => {
                    let _ = net_tx.send(AppEvent::Disconnected(e.to_string()));
                    break;
                }
            }
        }
    });
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if ev_tx.send(AppEvent::Term(ev)).is_err() {
                break;
            }
        }
    });

    // --- UI ----------------------------------------------------------------
    let mut app = App::new(me, addr, max_chars, online);
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut ev_rx, &mut w).await;
    ratatui::restore();
    result
}

fn load_key() -> io::Result<String> {
    let dir = dirs::config_dir()
        .ok_or_else(|| io::Error::other("could not find a config directory"))?
        .join("daychat");
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

async fn send(w: &mut OwnedWriteHalf, m: &ClientMsg) -> io::Result<()> {
    w.write_all(proto::encode(m).as_bytes()).await
}

async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    w: &mut OwnedWriteHalf,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| app.draw(f))?;
        let Some(ev) = rx.recv().await else { return Ok(()) };
        match ev {
            AppEvent::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if let Some(out) = app.on_key(k) {
                    if !app.connected {
                        app.err("not connected");
                    } else if let Err(e) = send(w, &out).await {
                        app.disconnected(e.to_string());
                    }
                }
            }
            AppEvent::Term(_) => {} // resize etc.: just redraw
            AppEvent::Net(m) => app.on_server(m),
            AppEvent::Disconnected(why) => app.disconnected(why),
        }
        if app.quit {
            return Ok(());
        }
    }
}

struct App {
    me: String,
    addr: String,
    max_chars: usize,
    online: usize,
    log: Vec<Line<'static>>,
    input: Vec<char>,
    cursor: usize,
    /// Lines scrolled up from the bottom of the log.
    scroll: u16,
    connected: bool,
    quit: bool,
}

impl App {
    fn new(me: String, addr: String, max_chars: usize, online: usize) -> Self {
        let mut app = Self {
            me,
            addr,
            max_chars,
            online,
            log: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            scroll: 0,
            connected: true,
            quit: false,
        };
        app.info(format!("connected to {} as {}", app.addr, app.me));
        app.info("your ID changes every day at 00:00 UTC. type /help for commands.".into());
        app
    }

    // --- log helpers -------------------------------------------------------

    fn push(&mut self, line: Line<'static>) {
        self.log.push(line);
        if self.log.len() > MAX_LOG {
            self.log.drain(..self.log.len() - MAX_LOG);
        }
    }

    fn info(&mut self, text: String) {
        self.push(Line::styled(format!("  · {text}"), Style::new().fg(Color::DarkGray)));
    }

    fn err(&mut self, text: impl Into<String>) {
        self.push(Line::styled(format!("  ! {}", text.into()), Style::new().fg(Color::Red)));
    }

    fn disconnected(&mut self, why: String) {
        if self.connected {
            self.connected = false;
            self.err(format!("disconnected: {why}. /quit to exit"));
        }
    }

    fn help(&mut self) {
        for l in [
            "commands:",
            "  <text>            send to the public room",
            "  /msg <id> <text>  direct message (also /m, /dm)",
            "  /who              list who is online",
            "  /id               show your ID for today",
            "  /clear            clear the screen",
            "  /quit             exit (also Ctrl-C)",
            "  //text            send text starting with '/'",
            "keys: PgUp/PgDn scroll · Esc clears input · Ctrl-U clears input",
        ] {
            self.info(l.to_string());
        }
    }

    // --- input ---------------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) -> Option<ClientMsg> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.quit = true,
            KeyCode::Char('u') if ctrl => self.clear_input(),
            KeyCode::Char(c) if !ctrl => {
                // Generous hard cap; the real limit is checked on submit.
                if !limits::is_forbidden(c) && self.input.len() < self.max_chars + 64 {
                    self.input.insert(self.cursor, c);
                    self.cursor += 1;
                }
            }
            KeyCode::Enter => return self.submit(),
            KeyCode::Esc => self.clear_input(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            _ => {}
        }
        None
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Length of the message body the counter should show.
    fn body_len(&self) -> usize {
        let s: String = self.input.iter().collect();
        if s.starts_with("//") {
            return s.chars().count() - 1;
        }
        match s.strip_prefix('/') {
            None => s.trim().chars().count(),
            Some(cmd) => {
                let mut p = cmd.splitn(3, ' ');
                match p.next() {
                    Some("msg" | "m" | "dm") => p.nth(1).map_or(0, |t| t.trim().chars().count()),
                    _ => 0,
                }
            }
        }
    }

    fn submit(&mut self) -> Option<ClientMsg> {
        let raw: String = self.input.iter().collect();
        self.clear_input();
        self.scroll = 0;
        let line = raw.trim();
        if line.is_empty() {
            return None;
        }
        if let Some(rest) = line.strip_prefix("//") {
            return self.checked(ClientMsg::Say { text: format!("/{rest}") });
        }
        let Some(cmd) = line.strip_prefix('/') else {
            return self.checked(ClientMsg::Say { text: line.to_string() });
        };
        let mut parts = cmd.splitn(3, ' ');
        match parts.next().unwrap_or("") {
            "q" | "quit" | "exit" => self.quit = true,
            "h" | "help" | "?" => self.help(),
            "clear" => self.log.clear(),
            "w" | "who" => return Some(ClientMsg::Who),
            "id" | "me" => self.info(format!("your ID today is {}", self.me)),
            "m" | "msg" | "dm" => {
                let to = parts.next().unwrap_or("");
                let text = parts.next().unwrap_or("");
                if to.is_empty() || text.trim().is_empty() {
                    self.err("usage: /msg <id> <text>");
                } else {
                    return self.checked(ClientMsg::Dm { to: id::normalize_id(to), text: text.to_string() });
                }
            }
            other => self.err(format!("unknown command /{other}; try /help")),
        }
        None
    }

    /// Client-side length check so the user gets instant feedback.
    fn checked(&mut self, msg: ClientMsg) -> Option<ClientMsg> {
        let text = match &msg {
            ClientMsg::Say { text } | ClientMsg::Dm { text, .. } => text,
            _ => return Some(msg),
        };
        match limits::validate_text(text, self.max_chars) {
            Ok(_) => Some(msg),
            Err(e) => {
                self.err(e);
                None
            }
        }
    }

    // --- server messages -----------------------------------------------------

    fn on_server(&mut self, m: ServerMsg) {
        use limits::sanitize as clean;
        match m {
            ServerMsg::Chat { from, text, ts } => {
                let from = clean(&from);
                let style = if from == self.me {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    id_style(&from)
                };
                self.push(Line::from(vec![
                    time_span(ts),
                    Span::styled(from, style),
                    Span::raw("  "),
                    Span::raw(clean(&text)),
                ]));
            }
            ServerMsg::Dm { from, to, text, ts } => {
                let label = if clean(&from) == self.me {
                    format!("[dm → {}]", clean(&to))
                } else {
                    format!("[dm ← {}]", clean(&from))
                };
                let magenta = Style::new().fg(Color::Magenta);
                self.push(Line::from(vec![
                    time_span(ts),
                    Span::styled(label, magenta.add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(clean(&text), magenta),
                ]));
            }
            ServerMsg::Joined { id } => {
                if clean(&id) != self.me {
                    self.online += 1;
                    self.info(format!("{} joined", clean(&id)));
                }
            }
            ServerMsg::Left { id } => {
                self.online = self.online.saturating_sub(1);
                self.info(format!("{} left", clean(&id)));
            }
            ServerMsg::Rotated { id } => {
                self.me = clean(&id);
                self.info(format!("new day: your ID is now {}", self.me));
            }
            ServerMsg::Who { ids, total } => {
                self.online = total;
                let more = if total > ids.len() { ", …" } else { "" };
                let list: Vec<String> = ids.iter().map(|s| clean(s)).collect();
                self.info(format!("{total} online: {}{more}", list.join(", ")));
            }
            ServerMsg::Info { text } => self.info(clean(&text)),
            ServerMsg::Error { text } => self.err(clean(&text)),
            ServerMsg::Welcome { .. } => {}
        }
    }

    // --- rendering -----------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let [log_area, input_area, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());

        // Message log, pinned to the bottom unless scrolled.
        let para = Paragraph::new(self.log.clone()).wrap(Wrap { trim: false });
        let total = para.line_count(log_area.width).min(u16::MAX as usize) as u16;
        let max_scroll = total.saturating_sub(log_area.height);
        let top = max_scroll - self.scroll.min(max_scroll);
        f.render_widget(para.scroll((top, 0)), log_area);

        // Input box with a live character counter.
        let body = self.body_len();
        let counter_style = if body > self.max_chars {
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::DarkGray))
            .title_bottom(Line::styled(format!(" {body}/{} ", self.max_chars), counter_style).right_aligned());
        let inner_w = input_area.width.saturating_sub(2) as usize;
        let before: String = self.input[..self.cursor].iter().collect();
        let cur_x = before.width();
        let offset = cur_x.saturating_sub(inner_w.saturating_sub(1));
        let text: String = self.input.iter().collect();
        f.render_widget(Paragraph::new(text).block(block).scroll((0, offset as u16)), input_area);
        f.set_cursor_position(Position::new(
            input_area.x + 1 + (cur_x - offset) as u16,
            input_area.y + 1,
        ));

        // Status bar.
        let mut status = vec![
            Span::styled(
                format!(" {} ", self.me),
                Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {} · {} online", self.addr, self.online)),
        ];
        if !self.connected {
            status.push(Span::styled("  DISCONNECTED", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)));
        }
        if self.scroll > 0 {
            status.push(Span::styled("  [scrolled: PgDn]", Style::new().fg(Color::Yellow)));
        }
        status.push(Span::styled("  /help", Style::new().fg(Color::DarkGray)));
        f.render_widget(Paragraph::new(Line::from(status)), status_area);
    }
}

fn time_span(ts: i64) -> Span<'static> {
    let t = chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M ").to_string())
        .unwrap_or_default();
    Span::styled(t, Style::new().fg(Color::DarkGray))
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
