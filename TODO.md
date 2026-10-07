# Jmail — TODO / Roadmap

Keyboard-driven GTK4 email client. Flatpak app id `tech.jptr.jmail`.

## Stack & decisions
- **Rust + gtk-rs + libadwaita 1.8** (GNOME Platform 50 runtime).
- **IMAP + SMTP**, Gmail via **OAuth2 XOAUTH2** (user-supplied Google client id/secret in Settings).
- **WebKitGTK 6.0** for HTML mail (remote content blocked by default) — not wired yet.
- **SQLite** (`rusqlite`, bundled, FTS5) local cache + **IMAP IDLE + 60s poll** sync.
- Secrets in the **system keyring** (`keyring` crate), with `JMAIL_DEV_PLAINTEXT=1` fallback into a DB table.
- Theme: UI accent follows the system (Adwaita `@accent_*`); brand color `#673AB7` lives in the icon/AppStream branding only.

## Layout
- `crates/jmail-core` — engine, no GTK: models, `Db` (cache), `imap`, `sync`, `secrets`, `threading`, `events`.
- `crates/jmail-ui` — GTK4/libadwaita app (binary `jmail`).
- `crates/jmail-mock-imap` — dev/test IMAP server (lib + binary).
- `data/` — `.desktop`, AppStream metainfo, icon.
- `tech.jptr.jmail.yaml` — Flatpak manifest; `build-aux/flatpak/cargo-sources.json` (regenerated via `flatpak-cargo-generator.py`).

## Build / run / test
```sh
# build + install Flatpak
flatpak-builder --user --install --force-clean --assumeyes --disable-rofiles-fuse \
  --state-dir=/tmp/opencode/jmail-state /tmp/opencode/jmail-build tech.jptr.jmail.yaml
flatpak run tech.jptr.jmail

# core tests (unit + integration against the mock)
cargo test -p jmail-core

# dev mock IMAP server (plaintext): then add 127.0.0.1:1143 in Settings
cargo run -p jmail-mock-imap -- --port 1143 [--tls] [--push-after N]

# regenerate vendored crates after changing dependencies
cargo generate-lockfile
python3 flatpak-cargo-generator.py Cargo.lock -o build-aux/flatpak/cargo-sources.json
```
Env: `JMAIL_SEED=1` seeds demo accounts/messages; `JMAIL_DEV_PLAINTEXT=1` stores passwords in the DB (dev only).
Cache: `~/.var/app/tech.jptr.jmail/data/jmail/cache.db`.

---

## M0 — Scaffold + Flatpak shell  ✅ DONE
- [x] Cargo workspace (`jmail-core`, `jmail-ui`), Flatpak manifest, cargo vendoring.
- [x] libadwaita 3-pane window (folders | list | reading), static status dot.
- [x] `.desktop`, metainfo, icon.

## M0.5 — Interaction + polish  ✅ DONE
- [x] Fixed window-action dispatch (`win.*` / `app.*` prefixes) — root cause of dead keys.
- [x] Message selection model (`j/k`, ↑/↓, click) + `o`/Enter opens a standalone window.
- [x] Compose dialog stub, search bar, in-memory actions, delete-selects-next fix.
- [x] `adw::ShortcutsDialog`, preview-in-pane on selection, delayed mark-read, Gmail keymap.
- [x] Theme-aware CSS; purple `#673AB7` icon + AppStream branding.

## M1 — Local cache (SQLite + FTS5)  ✅ DONE
- [x] `Db`: migrations (`user_version`), schema `accounts/folders/messages/bodies/settings`, FTS5 triggers.
- [x] Repository API (upsert, unified inbox, flags, move/delete, search, settings, seed).
- [x] UI list stack: `MessageRow` (`glib::BoxedAnyObject`) → `gio::ListStore` → `FilterListModel` → `SingleSelection` → `ListView`.
- [x] Dev seed + persistence tests.

## M2 — IMAP receive + sync engine  ✅ DONE
- [x] `ImapBackend`: implicit TLS / STARTTLS / plaintext, SPECIAL-USE, incremental `UID FETCH`, lazy body, `UID STORE`, MOVE (COPY+EXPUNGE fallback).
- [x] Sync engine: per-account task, IDLE + 60s poll, backoff, resync-after-IDLE, `SyncCommand`/`Event`s.
- [x] `jmail-mock-imap` (TLS via rcgen, live push) + integration tests (STARTTLS, sync, move, IDLE push).
- [x] Add Account dialog, keyring + `JMAIL_DEV_PLAINTEXT`, account removal, DB v2.
- [x] Actions push star/read/archive/delete to the server.
- [x] Settings → **Advanced → Force resync** rebuilds the whole cache from the server (`SyncCommand::Rebuild`).
- [x] Verified end-to-end in the Flatpak sandbox against the mock.

---

## M3 — Conversations, real bodies, HTML  🚧 IN PROGRESS
- [x] Reading pane renders cached **text** bodies; newest at bottom.
- [x] Fetch bodies **on open**: preview sends `SyncCommand::FetchBody`; pane re-renders on `MessagesChanged` (`actions.rs ensure_body`/`reload_preview`).
- [x] **WebKitGTK 6.0** HTML rendering (spike): `webkit6` renders `body_html` with page scripts disabled (`enable-javascript-markup=false`), images not auto-loaded, and `script`/`iframe`/`object`/`embed` stripped. Runtime already ships WebKitGTK 6.0; no extra Flatpak module.
- [x] WebView theming: background/foreground/accent pulled from the active GTK theme and injected as CSS; re-themes live on `AdwStyleManager::dark` change.
- [x] WebView auto-size: content height measured via `evaluate_javascript` and re-measured on width changes, so the reading pane scrolls as a whole.
- [ ] Real threading: build `thread_id` from `References`/`In-Reply-To` (+ Gmail `X-GM-THRID`). Currently `thread_id = message_id` (one message per thread).
- [ ] Collapse the list to one row per conversation (message-count badge); update action identity.
- [ ] Collapse quoted text / show quoted behind an expander.
- [ ] MIME: attachments metadata, multipart, reply attribution.
- [ ] Link handling in HTML bodies (open externally instead of navigating the WebView).

> Local `cargo run -p jmail-ui` needs the WebKitGTK dev files: `sudo dnf install webkitgtk6.0-devel javascriptcoregtk6.0-devel libsoup3-devel`. The Flatpak SDK already has them.

## M4 — Sending (SMTP)  ⬜ TODO
- [ ] SMTP via `lettre` (STARTTLS/implicit TLS; password now, XOAUTH2 after M5).
- [ ] Wire the compose dialog's Send; reply / reply-all / forward with quoting.
- [ ] Save sent copy to the Sent folder (`APPEND`), handle drafts (`SyncCommand`/folder).
- [ ] Address autocomplete (ties into M9 contacts).

## M5 — Google OAuth2 / XOAUTH2  ⬜ TODO  ← likely next
- [ ] Google Cloud "Desktop app" OAuth client, loopback redirect + PKCE, user-supplied id/secret (stored in keyring).
- [ ] Browser launch via OpenURI portal; localhost listener; token refresh; XOAUTH2 for IMAP and SMTP.
- [ ] Enable the currently-disabled Google option in Add Account.
- [ ] README/setup steps for obtaining the client id.

## M6 — Actions & folder semantics  🟡 MOSTLY DONE
- [x] Archive → Archive, Delete → Trash, star/read/unread, server-applied via `SyncCommand`.
- [x] SPECIAL-USE detection (RFC 6154).
- [ ] Gmail semantics: archive = remove `\Inbox`, `X-GM-LABELS` for "label as" (`l`).
- [ ] "Move to folder" picker (`v`) — currently status-only.
- [ ] Clicking a folder row filters the message list to that folder (currently **Joined Inbox only**); `g i/t/d/a` switch the view, not just the subtitle.

## M7 — Search  🟡 PARTIAL
- [x] `Db::search` over FTS5 (subject/from/body) + tests.
- [ ] UI search should query `Db::search` across **all cached mail**, not just filter the loaded page.
- [ ] Server-side IMAP `SEARCH` for uncached mail; merge results.
- [ ] Search-as-you-type UX, result highlighting, `n`/`N` or `j/k` through results.

## M8 — Notifications & desktop integration  🟡 PARTIAL
- [x] Settings pages (Notifications off by default, Reading, Sync, Accounts), desktop entry, metainfo, icon, branding.
- [ ] Emit `GNotification` on `Event::NewInboxMessage` (Inbox only, grouped per account, click opens the message).
- [ ] Persist settings (notifications toggle, poll interval, remote images, conversation view) to `Db::settings`; load on startup.
- [ ] Respect the master + per-account notification toggles.

## M9 — Polish & distribution  🟡 PARTIAL
- [x] Responsive breakpoint (collapse to one pane), Flatpak build + AppStream branding, tests.
- [ ] Attachments: view (save to `xdg-download`) and send.
- [ ] App icon: add 256/512 PNG exports for AppStream/Flathub; run `appstreamcli` clean.
- [ ] Flathub-ready manifest: fully offline (`cargo-sources.json`), lint, remove `--share=network` only if feasible.
- [ ] Accessibility pass (labels, focus order), dark-theme check.
- [ ] Contacts / address book (`libfolks`/EDS) — deferred earlier.
- [ ] Rich/HTML compose with formatting toolbar — deferred earlier.

---

## Cross-cutting known limitations
- Gmail is unusable until M5 (OAuth); the Add Account dialog is generic IMAP only.
- Send is a stub (compose opens, Send closes).
- Search covers only the loaded list; folder rows don't switch the view (Joined Inbox only).
- Snippets are empty until a body is fetched; initial sync fetches **all** headers per folder (no cap yet).
- Notifications are configured but never emitted.
- Non-mail engines (calendar, contacts) intentionally out of scope.

## Verification checklist (regression)
- [ ] `cargo test -p jmail-core` (8 unit + 4 integration) green.
- [ ] Flatpak builds clean (no warnings) and launches.
- [ ] Mock e2e: `jmail-mock-imap --port 1143 --push-after 8`, add `127.0.0.1:1143`, confirm folders/messages sync and the pushed mail arrives.
- [ ] Keys: `j/k`, `o`(new window), `c`, `r/a/f`, `e`, `#`, `s`, `Shift+I/U`, `/`, `g i/t/d/a`, `?`, `Shift+,`.
- [ ] Hover shows only the highlight (does **not** change the reading pane); click/`j/k` preview; double-click opens a window.
