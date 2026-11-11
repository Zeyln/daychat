# daychat

A lightweight, open chat service for the terminal.

- **Daily IDs.** You get a random ID like `k7f2-q9xm`. It stays the same all day and changes at 00:00 UTC. Nobody can link yesterday's ID to today's unless they hold the server's secret.
- **Plain text only.** Unicode is fine. Control characters and bidi overrides are stripped. There are no attachments, markup or history.
- **Limits.** Messages are capped at 280 characters, counted as Unicode chars rather than bytes. Each client may send a burst of 5 messages, then 1 per second.
- **A public room plus DMs.** Your ID is also your address: `/msg k7f2-q9xm hello`.

## Install

### Arch Linux

Clone the repo and build a native package. It includes both binaries and a systemd unit for the server:

```sh
cd packaging/arch && makepkg -si
sudo systemctl enable --now daychat-server   # optional: host a server
daychat some.host
```

Tagged releases also attach a prebuilt `daychat-*.pkg.tar.zst`. Install it with `sudo pacman -U daychat-*.pkg.tar.zst`.

### Prebuilt binaries

Push a tag such as `v0.1.0` to get a GitHub Release with:

| Platform | Asset |
|---|---|
| Arch Linux | `daychat-<ver>-1-x86_64.pkg.tar.zst` |
| Any Linux x86_64 (static musl) | `daychat-<ver>-x86_64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `daychat-<ver>-x86_64-pc-windows-msvc.zip` |
| macOS Apple Silicon | `daychat-<ver>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `daychat-<ver>-x86_64-apple-darwin.tar.gz` |

The macOS binaries are unsigned. Clear the quarantine flag before running them: `xattr -d com.apple.quarantine daychat daychat-server`.

## Usage

Run `daychat` to open the shell. From there you can connect, switch servers or host a server without going back to your system shell:

```
╭───────────────────────────────────────────────────────────╮
│ ✻ daychat v0.1.0                                          │
│                                                           │
│   /connect <host>  join a server                          │
│   /host            run a server here and join it          │
│   /help            all commands and keys                  │
╰───────────────────────────────────────────────────────────╯
╭───────────────────────────────────────────────────────────╮
│ > /co                                                     │
╰───────────────────────────────────────────────────────────╯
  /connect [host[:port] | #n]   connect to a server (default: last used)
```

`daychat HOST[:PORT]` opens the shell and connects straight away. `--fresh` starts with a throwaway identity.

| Input | Effect |
|---|---|
| `text` | Say in the public room |
| `/connect [host[:port] \| #n]` | Connect (alias `/c`). With no argument, connects to the last server used. `#n` picks from `/servers` |
| `/disconnect` | Leave the current server |
| `/host [port]` | Run a server inside daychat and join it. Others connect to your IP |
| `/unhost` | Stop hosting |
| `/servers` | Recently used servers |
| `/msg <id> <text>` | Direct message (`/m`, `/dm`) |
| `/who` | Who's online |
| `/id` | Your ID on this server today |
| `/fresh` | Toggle a throwaway identity for new connections |
| `/clear`, `/help`, `/quit` | Clear the screen, list commands, exit |
| `//text` | Send text that starts with `/` |

**Keys:**
- Typing `/` shows matching commands. Tab completes, ↑/↓ select, Enter runs.
- ↑/↓ also scroll through what you've typed before.
- PgUp/PgDn scroll the chat.
- Esc clears the input.
- Ctrl-C clears the input; press it twice to exit.

## Dedicated server

```sh
daychat-server --bind 0.0.0.0:7878 --secret-file ./daychat-server.secret
```

The secret file is created on first run. Keep it so that IDs stay the same across server restarts. On Arch, use the bundled systemd unit instead (see above).

## Run from source

```sh
cargo run --release --bin daychat
cargo run --release --bin daychat-server
```

## How IDs work

1. On first run, the client creates a random 256-bit key in your config directory (`%APPDATA%\daychat\key` or `~/.config/daychat/key`).
2. For each server, it sends `sha256(key ‖ server address)`. A server therefore can't replay your key on a different server.
3. The server computes `id = sha256("daychat-id-v1" ‖ server_secret ‖ utc_date ‖ that key)` and truncates it to 40 bits, written as Crockford base32.
4. Connected clients get a `rotated` frame at midnight UTC. No join or leave is broadcast at that moment, so the switch can't be used to link the old ID to the new one.

## Protocol

Newline-delimited JSON over TCP, one object per line, tagged by `"t"`. Lines are capped at 8 KiB. See `src/proto.rs`.

```
→ {"t":"hello","version":1,"key":"<64 hex>"}
← {"t":"welcome","id":"k7f2-q9xm","max_chars":280,"burst":5,"refill_secs":1.0,"online":3}
→ {"t":"say","text":"hi"}
← {"t":"chat","from":"k7f2-q9xm","text":"hi","ts":1793750400}
```

## Threat model

"Cleartext" means two things here: the content is plain text, and the transport is not encrypted. Anyone on the network path, and the server operator, can read every message and every client key. If you need confidentiality in transit, put the server behind TLS or an SSH tunnel. The server keeps nothing on disk except its 32-byte secret and never logs message content.
