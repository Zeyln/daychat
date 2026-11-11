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

## Run from source

```sh
cargo run --release --bin daychat-server            # listens on 0.0.0.0:7878
cargo run --release --bin daychat -- 127.0.0.1      # in another terminal
cargo run --release --bin daychat -- --fresh        # a second client with a throwaway ID
```

Server flags: `--bind ADDR:PORT`, `--secret-file PATH`. The default secret file is `./daychat-server.secret`, created on first run. Keep it to keep IDs stable across server restarts.

## Client commands

| Input | Effect |
|---|---|
| `text` | Say in the public room |
| `/msg <id> <text>` | Direct message (`/m`, `/dm`) |
| `/who` | Who's online |
| `/id` | Show your ID |
| `/clear`, `/help`, `/quit` | Clear the screen, list commands, exit |
| `//text` | Send text that starts with `/` |

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
