# daychat

A lightweight, open chat service for the terminal.

- **Daily IDs.** You get a random ID like `k7f2-q9xm`. It stays the same all day and changes at 00:00 UTC. Nobody can link yesterday's ID to today's unless they hold the server's secret.
- **Plain text only.** Unicode is fine. Control characters and bidi overrides are stripped. There are no attachments, markup or history.
- **Limits.** Messages are capped at 280 characters, counted as Unicode chars rather than bytes. Each client may send a burst of 5 messages, then 1 per second.
- **A public room plus private conversations by consent.** `/connect k7f2-q9xm` sends a request. Once the other person accepts, everything you type goes to them, end-to-end encrypted. The server refuses private messages between people who haven't both agreed, and can't read the ones it relays.

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

Download them from [Releases](https://github.com/Zeyln/daychat/releases). Each release has:

| Platform | Asset |
|---|---|
| Arch Linux | `daychat-<ver>-1-x86_64.pkg.tar.zst` |
| Any Linux x86_64 (static musl) | `daychat-<ver>-x86_64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `daychat-<ver>-x86_64-pc-windows-msvc.zip` |
| macOS Apple Silicon | `daychat-<ver>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `daychat-<ver>-x86_64-apple-darwin.tar.gz` |

There are also bare `daychat-client-<target>` binaries, which `/update` uses, and `SHA256SUMS` with its minisign signature `SHA256SUMS.minisig`. To check a download by hand: `minisign -V -p packaging/release.pub -m SHA256SUMS`, then `sha256sum -c SHA256SUMS --ignore-missing`.

The macOS binaries are not notarized by Apple (releases are signed with minisign instead; see Updating). Clear the quarantine flag before running them: `xattr -d com.apple.quarantine daychat daychat-server`.

## Usage

Run `daychat` to open the shell. It joins the official server straight away (the only server it connects to); everything else happens inside.

```
╭──────────────────────────────────────────────────────────╮
│ ✻ daychat v0.3.0                                         │
│                                                          │
│   /join            rejoin the official server            │
│   /connect <id>    start a private conversation          │
│   /help            all commands and keys                 │
╰──────────────────────────────────────────────────────────╯
  ◆ k7f2-q9xm wants to connect · /accept or /decline
  ◆ connected with k7f2-q9xm · your messages now go to them
    safety code maple tiger velvet orbit · compare it with them…
╭──────────────────────────────────────────────────────────╮
│ > hey!                                                   │
╰──────────────────────────────────────────────────────────╯
 ● 3m9a-x2pd on the official server · 4 online · to k7f2-q9xm (encrypted, verified)
```

`--fresh` starts with a throwaway identity.

### Private conversations

1. Ask someone for a conversation with `/connect <their-id>`.
2. They accept with `/accept`, or by running `/connect <your-id>` back. `/decline` refuses.
3. Once accepted, plain text goes to that person, and the input box and status bar turn magenta to show it.
4. **Tab** cycles between the public room and each open conversation. `/room` and `/chat <id>` jump directly.
5. `/close` ends the current conversation, or withdraws a request that hasn't been answered yet.
6. Compare the **safety code** with the other person outside daychat (in person or by phone), then run `/verified`. The status bar shows "not verified" until you do. Matching codes mean nobody, not even the server, is reading along.

Conversations end when either person leaves the server. They also reset at the daily ID rotation, since both IDs change.

### Commands

| Input | Effect |
|---|---|
| `text` | Send to the current conversation, or to the room |
| `/connect <id>` | Request a private conversation, or accept one from that ID |
| `/accept [id]`, `/decline [id]` | Answer a request (default: the newest) |
| `/close [id]` | End a conversation or withdraw a request |
| `/room`, `/chat <id>` | Switch where your text goes |
| `/peers` | Conversations and pending requests |
| `/verify [id]`, `/verified [id]` | Show a conversation's safety code; mark it as checked |
| `/join`, `/leave` | Rejoin or leave the official server |
| `/update` | Install the latest signed release and restart |
| `/who` | Who's online |
| `/id` | Your ID on this server today |
| `/fresh` | Toggle a throwaway identity for new connections |
| `/clear`, `/help`, `/quit` | Clear the screen, list commands, exit |
| `//text` | Send text that starts with `/` |

**Keys:**
- Typing `/` shows matching commands. Tab completes, ↑/↓ select, Enter runs.
- Otherwise, Tab cycles between the room and your conversations.
- ↑/↓ also scroll through what you've typed before.
- PgUp/PgDn scroll the chat.
- Esc clears the input.
- Ctrl-C clears the input; press it twice to exit.

## Updating

daychat checks for a new release when it starts. If there is one, joining a server is blocked until you run `/update`:
- **Installed from a release archive** (Windows, macOS, any Linux): `/update` replaces the program and restarts it.
- **Installed with pacman on Arch**: `/update` downloads the new package and runs `sudo pacman -U`, so pacman keeps tracking it. You'll be asked for your password.

Every update is checked before it's installed: the release's checksums must carry a valid signature from the daychat release key built into your client, and the signature must name that exact release. A tampered, unsigned or replayed release is refused. If GitHub can't be reached, daychat lets you join anyway.

Set `DAYCHAT_NO_UPDATE_CHECK=1` to skip the check, for example when running a development build. See [RELEASING.md](RELEASING.md) for how releases are built and signed.

## The official server

`daychat` connects to one server only: the official one. Its address and full certificate fingerprint are built into the client (from `packaging/official-server`), and only that exact certificate is accepted. Not even a certificate from a public certificate authority will do, so nobody can impersonate the official server, even on your very first connection.

Third-party servers are deliberately unsupported. A modified server could phish users with official-looking messages, or sit in the middle of conversations whose safety codes nobody compared, and most people can't judge that risk. The server code is still public (MIT) so anyone can audit what the official server does.

Even the official server is treated as untrusted by the client: anything it says is shown labelled "server says ›", malformed IDs are dropped, and private messages are end-to-end encrypted.

### Running it (operator notes)

1. On the server machine (Arch: `sudo pacman -U daychat-*.pkg.tar.zst`), start it once: `sudo systemctl enable --now daychat-server`. It creates its secret and a self-signed certificate in `/var/lib/daychat`. `journalctl -u daychat-server` shows the full fingerprint.
2. Point the domain (or a dynamic-DNS name) at the machine, and forward TCP port 7878 on the router.
3. Put `<domain>:7878 <full fingerprint>` into `packaging/official-server`, then cut a release. Clients pick it up with `/update`.
4. **Back up `/var/lib/daychat`.** If the certificate is lost or replaced, every client refuses the server until a release ships the new fingerprint. The secret keeps IDs stable across restarts.

Hosting at home makes your home IP visible to every user and exposes your connection to floods the per-address limits can't stop. A small VPS avoids both; moving later only needs the files in `/var/lib/daychat` and a DNS change.

### Development servers

```sh
daychat-server --bind 127.0.0.1:7878 --secret-file ./daychat-server.secret
```

The server prints its full certificate fingerprint at startup. A **debug build** of the client (`cargo run --bin daychat`, not `--release`) can then be pointed at it:

```sh
DAYCHAT_DEV_SERVER="127.0.0.1:7878 <full fingerprint>" DAYCHAT_NO_UPDATE_CHECK=1 cargo run --bin daychat
```

Release builds ignore `DAYCHAT_DEV_SERVER` entirely, so users can't be talked into pointing their client at someone else's server. Every connection uses TLS; there is no unencrypted mode.

## Run from source

```sh
cargo run --release --bin daychat
cargo run --release --bin daychat-server
```

## How IDs and login work

1. On first run, the client creates a random 256-bit key in your config directory (`%APPDATA%\daychat\key` or `~/.config/daychat/key`). It never leaves your machine.
2. For each server, the client derives a separate Ed25519 signing key from that key and the server address. Servers therefore can't link you to each other.
3. **Login:** the client sends its public key. The server answers with a random challenge, and the client signs it. Nothing secret crosses the wire, so an eavesdropper can't replay a login to impersonate you.
4. The server computes `id = sha256("daychat-id-v1" ‖ server_secret ‖ utc_date ‖ public key)` and truncates it to 40 bits, written as Crockford base32.
5. Connected clients get a `rotated` frame at midnight UTC. No join or leave is broadcast at that moment, so the switch can't be used to link the old ID to the new one.

## End-to-end encryption

Private conversations are end-to-end encrypted. The public room isn't, since everyone in it can read it anyway, but like all traffic it travels over TLS.

- When you `/connect` or `/accept`, the client makes a one-off X25519 key for that conversation. The server only relays the public halves.
- Messages are sealed with XChaCha20-Poly1305, under a key derived with HKDF-SHA256. The sender's key is bound into every message, so a server can't reflect your own messages back to you as theirs.
- The server sees who talks to whom, when, and roughly how long each message is. It can't read the content.
- **Safety code:** both sides see four words, like `maple tiger velvet orbit`, when the conversation opens (and again with `/verify`). Read them to each other in person or by phone, then run `/verified`. If the words match, no server sat in the middle of the key exchange.
- **Commit, then reveal.** The person who asks first sends only a hash of their key; the other person answers with theirs; only then is the first key revealed and checked against the hash. A server in the middle has to pick its fake keys before it can see the real ones, so it can't search for keys that make both codes match: it gets one blind guess, right about 1 time in 4 billion. That is what makes four words enough.
- Keys live only in memory. Leaving the server or the daily rotation ends the conversation and throws the keys away.

## Abuse limits

| Limit | Value |
|---|---|
| Messages per connection | burst of 5, then 1/s |
| Room messages, whole server | burst of 20, then 10/s in total |
| Join/leave announcements | burst of 10, then 1/s; extra ones are skipped and the online count is resent every 5 s instead |
| Message length | 280 characters |
| Protocol line | 8 KiB |
| Simultaneous connections | 8 per address, 32 per IPv6 /48 |
| Unfinished logins | 2 per address, 6 per IPv6 /48, 128 server-wide; must finish within 10 s |
| New connections | per address: burst of 10, then 1 every 6 s; per IPv6 /48: burst of 40, then 1 every 2 s |
| Connection requests | per address: burst of 5, then 1 every 20 s; per IPv6 /48: burst of 15, then 1 every 7 s; shared by all IDs there |
| Unanswered requests per user | 16 |
| Logged-in users per server | 1024 |

An "address" is an IPv4 address or an IPv6 /64. The server tracks at most 65,536 addresses at each level and evicts the oldest idle ones first. Put the server directly on the internet, not behind a TCP proxy or tunnel: behind one, every user shares the proxy's address and its limits.

## Protocol

Newline-delimited JSON over TLS (TCP), one object per line, tagged by `"t"`. Lines are capped at 8 KiB. See `src/proto.rs`.

```
→ {"t":"hello","version":3,"pubkey":"<ed25519, base64>"}
← {"t":"challenge","challenge":"<32 random bytes, base64>"}
→ {"t":"auth","answer":"<signature over the challenge, base64>"}
← {"t":"welcome","id":"k7f2-q9xm","max_chars":280,"burst":5,"refill_secs":1.0,"online":3}
→ {"t":"say","text":"hi"}
← {"t":"chat","from":"k7f2-q9xm","text":"hi","ts":1793750400}

# private conversation: A = k7f2-q9xm, B = 3m9a-x2pd
A → {"t":"connect","to":"3m9a-x2pd","commit":"<hash of A's x25519>"}   B ← request + A's commitment
B → {"t":"accept","to":"k7f2-q9xm","key":"<B's x25519>"}          A ← accepted + B's key
A → {"t":"reveal","to":"3m9a-x2pd","key":"<A's x25519>"}          A ← linked, B ← linked + A's key (checked against the commitment)
A → {"t":"dm","to":"3m9a-x2pd","sealed":"<nonce‖ciphertext>"}   (refused unless linked)
B → {"t":"hangup","peer":"k7f2-q9xm"}   A ← {"t":"unlinked","peer":"3m9a-x2pd","reason":"ended the conversation"}
```

v3 added signed login and end-to-end encryption; v4 added the commit-then-reveal exchange and word safety codes. Older clients are refused at hello with a "please upgrade" message.

## Threat model

**Protected against:**
- **Network eavesdroppers** reading anything, room messages included: every connection is TLS.
- **Anyone impersonating the server**, even on your first connection: the official server's exact certificate is built in, and the client connects nowhere else.
- **Phishing through server text.** Everything the server says is labelled as server text, and malformed IDs are dropped.
- Network eavesdroppers impersonating you. Login is a fresh signature each time.
- The server reading private conversations (end-to-end encrypted).
- An active man in the middle on a private conversation, *if you compare safety codes*.
- Terminal injection through messages (control and bidi characters are stripped).
- Floods from a single address, including floods of throwaway IDs.

**Not protected against:**
- **Modified clients.** The code is open source; a modified client can connect anywhere. The official client and its signed updates are what this protects.
- **The server operator** reading the public room, knowing your IP, which ID is yours, who you talk to and when, and (with its secret) linking your IDs across days.
- **Traffic analysis.** Network observers can still see that you connect to a daychat server, when, and roughly how much you send.
- A malicious server swapping keys in a conversation whose safety codes you didn't compare.
- Floods spread across many addresses (botnets).
- Replayed private messages. A server could deliver the same sealed message twice.
