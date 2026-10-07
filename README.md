# Jmail

A fast, keyboard-driven GTK4 email client for GNOME, written in Rust with
gtk-rs + libadwaita. Flatpak app id `tech.jptr.jmail`.

## Features

- Joined Inbox across multiple accounts, adaptive three-pane layout.
- IMAP receive with **IDLE** push and a 60s fallback poll; local SQLite cache + FTS5.
- Gmail-style vim keybindings (`j`/`k`, `o`, `e`, `#`, `s`, …).
- HTML mail rendered with WebKitGTK 6.0 (page scripts disabled, remote images blocked,
  themed to match the app, auto-sized to content).
- Compose in **markdown** with live inline styling and a rendered preview; send via
  SMTP, reply/reply-all/forward, and drafts saved to the server's Drafts folder.
- Generic IMAP/SMTP with a password; Gmail via OAuth is planned.

## Requirements

- **Flatpak** (GNOME runtime 50) — for the sandboxed build, or
- A local toolchain: Rust, plus GTK4, libadwaita, and WebKitGTK development files.
  On Fedora:

  ```sh
  sudo dnf install gtk4-devel libadwaita-devel webkitgtk6.0-devel \
      javascriptcoregtk6.0-devel libsoup3-devel
  ```

## Running

### Flatpak (recommended)

```sh
make flatpak     # build + install
make run         # ...or run the installed app
flatpak run tech.jptr.jmail
```

`make flatpak` runs the equivalent of:

```sh
flatpak-builder --user --install --force-clean --assumeyes \
  --disable-rofiles-fuse --state-dir=.flatpak-builder build-dir tech.jptr.jmail.yaml
```

### Locally with Cargo

```sh
make run         # = cargo run -p jmail-ui
```

Other targets: `make test`, `make mock`, `make clean`, `make help`.

The cache lives at `~/.local/share/jmail/cache.db` (local runs) or
`~/.var/app/tech.jptr.jmail/data/jmail/cache.db` (Flatpak).

Useful environment variables:

| Variable                | Effect                                                        |
|-------------------------|---------------------------------------------------------------|
| `JMAIL_DEV_PLAINTEXT=1` | Store passwords in the DB instead of the system keyring (dev). |
| `JMAIL_SEED=1`          | Insert demo accounts/messages on first launch.                 |
| `RUST_LOG=debug`        | Verbose sync-engine logging.                                   |

## Test server

A mock IMAP server ships in the workspace (`crates/jmail-mock-imap`) with a
canned mailbox and live IDLE push — perfect for trying the app without a real
account. It also runs a companion **SMTP** server that captures anything the app
sends, so you can exercise compose/reply/drafts locally.

Start it from the repo root (plaintext, default IMAP port 1143):

```sh
cargo run -p jmail-mock-imap
```

It prints both listeners, e.g. `Mock IMAP listening on 127.0.0.1:1143` and
`Mock SMTP listening on 127.0.0.1:2525`. The SMTP port is **stable by default
(2525)** so the account setting survives restarts — use it when adding the
account (see below).

### Server options

```sh
cargo run -p jmail-mock-imap -- [--port N] [--smtp-port N] [--tls] [--push-after N]
```

| Flag            | Meaning                                                                 |
|-----------------|-------------------------------------------------------------------------|
| `--port N`      | IMAP listen port `N` (default `1143`).                                  |
| `--smtp-port N` | SMTP listen port `N` (default `2525`; `0` picks a free port).           |
| `--tls`         | Enable implicit TLS / STARTTLS with a self-signed cert.                 |
| `--push-after N`| After `N` seconds, inject a new Inbox message to exercise IDLE push.    |

The demo mailbox has the folders **INBOX, Sent, Drafts, Archive, Trash, Spam,
Projects**, with sample messages (including an HTML one) and reply messages.

## Adding an account / logging in

1. Launch the app.
2. Open **Settings** (`Shift+,`) → **Accounts** → **Add**.
3. With the mock server running, fill in:

   | Field         | Value            |
   |---------------|------------------|
   | Name          | `Mock` (anything)|
   | Email         | `you@example.com`|
   | IMAP server   | `127.0.0.1`      |
   | Port          | `1143`           |
   | Security      | `None`           |
   | Username      | `you@example.com`|
   | Password      | `hunter2`        |
   | SMTP server   | `127.0.0.1`      |
   | SMTP port     | `2525` (the mock's default; change if you passed `--smtp-port`) |
   | SMTP security | `None`           |

4. Click **Add Account**. The engine connects, syncs folders, and the Joined
   Inbox fills. Select a message to preview it; the body is fetched on demand.

> In the Flatpak sandbox `127.0.0.1` is the host loopback, so the mock server
> running on your machine is reachable.

To verify live push, start the server with `--push-after 15`; a new Inbox
message ("Live push while you idled") arrives without any interaction.

### Rebuilding the cache

If mail looks stale — or after changing the mock demo data — open
**Settings → Advanced → Force resync**. This deletes the cached mail for every
account and re-fetches all folders from the server (`SyncCommand::Rebuild`).
Message bodies are cached, so this is also how to pull an updated body for a
message you have already opened.

## Keyboard shortcuts

| Key                | Action                                  |
|--------------------|-----------------------------------------|
| `j` / `k`          | Next / previous message                 |
| `o` / `Enter`      | Open conversation in a new window       |
| `click`            | Preview in the reading pane             |
| `c`                | Compose                                 |
| `r` / `a` / `f`    | Reply / reply all / forward (inline, at the bottom of the conversation) |
| `e`                | Archive                                 |
| `#`                | Delete                                  |
| `s`                | Star                                    |
| `Shift+I` / `Shift+U` | Mark read / unread                   |
| `/`                | Search                                  |
| `g` then `i`/`t`/`d`/`a` | Jump to Inbox / Sent / Drafts / All |
| `Shift+,`          | Settings                                |
| `?`                | Keyboard shortcuts                      |

## Development

```sh
cargo test -p jmail-core     # unit + integration tests (uses the mock server)
cargo build -p jmail-ui
```

See [`TODO.md`](TODO.md) for the roadmap and current status.

## Project layout

- `crates/jmail-core` — engine (no GTK): models, cache, IMAP, sync, secrets.
- `crates/jmail-ui` — GTK4/libadwaita application (binary `jmail`).
- `crates/jmail-mock-imap` — mock IMAP server for tests and local use.
- `data/` — desktop entry, AppStream metadata, icon.
- `tech.jptr.jmail.yaml` — Flatpak manifest.
