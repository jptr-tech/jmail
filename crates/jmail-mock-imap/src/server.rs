//! A minimal IMAP server for development and testing.
//!
//! Implements the subset Jmail's sync engine uses: CAPABILITY, STARTTLS,
//! LOGIN, LIST, SELECT, UID SEARCH, UID FETCH, UID STORE, UID COPY/MOVE,
//! EXPUNGE, IDLE/DONE, NOOP, CLOSE and LOGOUT.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex, Notify};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::mailbox::{quote, MockMailbox, MockMessage, SeqSet};

/// TLS material for STARTTLS / implicit TLS on the mock server.
pub struct MockTls {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub config: Arc<tokio_rustls::rustls::ServerConfig>,
}

pub struct MockConfig {
    pub username: String,
    pub password: String,
    /// When `Some`, STARTTLS and implicit TLS are supported.
    pub tls: Option<MockTls>,
}

impl MockConfig {
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            password: password.to_string(),
            tls: None,
        }
    }

    pub fn with_tls(mut self) -> Self {
        self.tls = Some(generate_tls());
        self
    }
}

struct Shared {
    username: String,
    password: String,
    tls: Option<MockTls>,
    mailbox: Mutex<MockMailbox>,
    /// Bumped whenever the mailbox changes, waking IDLE sessions.
    changed: watch::Sender<u64>,
    /// Extra wakeups for connections currently inside IDLE.
    idle_notify: Notify,
}

/// Handle to a running mock server.
pub struct MockServer {
    pub local_addr: SocketAddr,
    pub cert_der: Option<Vec<u8>>,
    shared: Arc<Shared>,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl MockServer {
    /// Start the server bound to an OS-assigned port.
    pub async fn start(config: MockConfig, mailbox: MockMailbox) -> io::Result<Self> {
        Self::start_on(0, config, mailbox).await
    }

    /// Start the server bound to `127.0.0.1:port` (0 picks a free port).
    pub async fn start_on(port: u16, config: MockConfig, mailbox: MockMailbox) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let local_addr = listener.local_addr()?;
        let (changed, _) = watch::channel(0);
        let cert_der = config.tls.as_ref().map(|tls| tls.cert_der.clone());
        let shared = Arc::new(Shared {
            username: config.username,
            password: config.password,
            tls: config.tls,
            mailbox: Mutex::new(mailbox),
            changed,
            idle_notify: Notify::new(),
        });
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(accept_loop(listener, shared.clone(), shutdown_rx));

        Ok(Self {
            local_addr,
            cert_der,
            shared,
            shutdown: Mutex::new(Some(shutdown_tx)),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Inject a message into a folder, waking IDLE sessions. Returns the UID.
    pub async fn push(&self, folder: &str, message: MockMessage) -> u32 {
        self.pusher().push(folder, message).await
    }

    /// A lightweight handle for pushing messages from other tasks.
    pub fn pusher(&self) -> MockPusher {
        MockPusher {
            shared: self.shared.clone(),
        }
    }

    /// The certificate the mock server presents (for pinning in tests).
    pub fn cert_der(&self) -> Option<&[u8]> {
        self.cert_der.as_deref()
    }

    /// Stop the server.
    pub async fn shutdown(self) {
        if let Some(sender) = self.shutdown.lock().await.take() {
            sender.send(()).ok();
        }
    }
}

fn bump(shared: &Shared) {
    // Compute the new version first: holding the `borrow()` guard across
    // `send()` deadlocks (the guard takes the watch's internal read lock that
    // `send()` needs exclusively).
    let next = shared.changed.borrow().wrapping_add(1);
    shared.changed.send(next).ok();
    shared.idle_notify.notify_waiters();
}

/// Pushes messages into the running server from other tasks.
#[derive(Clone)]
pub struct MockPusher {
    shared: Arc<Shared>,
}

impl MockPusher {
    pub async fn push(&self, folder: &str, message: MockMessage) -> u32 {
        let uid = {
            let mut mailbox = self.shared.mailbox.lock().await;
            mailbox.push(folder, message)
        };
        bump(&self.shared);
        uid
    }
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break };
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(socket, shared).await {
                        tracing::debug!("mock imap connection error: {error}");
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }
}

/// The stream side of a connection: plaintext or TLS, buffered.
enum ServerStream {
    Plain(BufReader<tokio::net::TcpStream>),
    Tls(BufReader<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>),
}

impl AsyncRead for ServerStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(inner) => Pin::new(inner).poll_read(cx, buf),
            Self::Tls(inner) => Pin::new(inner).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ServerStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(inner) => Pin::new(inner).poll_write(cx, buf),
            Self::Tls(inner) => Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(inner) => Pin::new(inner).poll_flush(cx),
            Self::Tls(inner) => Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(inner) => Pin::new(inner).poll_shutdown(cx),
            Self::Tls(inner) => Pin::new(inner).poll_shutdown(cx),
        }
    }
}

impl AsyncBufRead for ServerStream {
    fn poll_fill_buf(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<&[u8]>> {
        match self.get_mut() {
            Self::Plain(inner) => Pin::new(inner).poll_fill_buf(cx),
            Self::Tls(inner) => Pin::new(inner).poll_fill_buf(cx),
        }
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        match self.get_mut() {
            Self::Plain(inner) => Pin::new(inner).consume(amount),
            Self::Tls(inner) => Pin::new(inner).consume(amount),
        }
    }
}

impl ServerStream {
    /// Take the raw socket back out (needed for the STARTTLS upgrade).
    fn into_raw(self) -> io::Result<tokio::net::TcpStream> {
        match self {
            Self::Plain(inner) => Ok(inner.into_inner()),
            Self::Tls(_) => Err(io::Error::other("already TLS")),
        }
    }
}

#[derive(Default)]
struct ConnState {
    authed: bool,
    selected: Option<String>,
}

async fn handle_connection(socket: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    socket.set_nodelay(true).ok();
    let mut stream = ServerStream::Plain(BufReader::new(socket));
    let mut state = ConnState::default();

    write(
        &mut stream,
        "* OK [CAPABILITY IMAP4rev1 IDLE STARTTLS SPECIAL-USE MOVE UIDPLUS] Jmail mock ready\r\n",
    )
    .await?;

    loop {
        let Some(line) = read_line(&mut stream).await? else {
            return Ok(());
        };
        let Some((tag, command)) = parse_command(&line) else {
            write(&mut stream, "* BAD invalid command\r\n").await?;
            continue;
        };

        let name = command.split(' ').next().unwrap_or("").to_uppercase();
        match name.as_str() {
            "CAPABILITY" => {
                write(
                    &mut stream,
                    "* CAPABILITY IMAP4rev1 IDLE STARTTLS SPECIAL-USE MOVE UIDPLUS\r\n",
                )
                .await?;
                ok(&mut stream, &tag).await?;
            }
            "NOOP" => {
                emit_changes(&mut stream, &shared, &state).await?;
                ok(&mut stream, &tag).await?;
            }
            "STARTTLS" => {
                let Some(tls) = &shared.tls else {
                    no(&mut stream, &tag, "TLS not available").await?;
                    continue;
                };
                write(&mut stream, &format!("{tag} OK Begin TLS negotiation now\r\n")).await?;
                stream.flush().await?;
                let raw = stream.into_raw()?;
                let acceptor = tokio_rustls::TlsAcceptor::from(tls.config.clone());
                let tls_stream = acceptor.accept(raw).await?;
                stream = ServerStream::Tls(BufReader::new(tls_stream));
            }
            "LOGIN" => {
                let mut parts = command.split(' ').skip(1);
                let user = unquote(parts.next().unwrap_or(""));
                let pass = unquote(parts.next().unwrap_or(""));
                if user.eq_ignore_ascii_case(&shared.username) && pass == shared.password {
                    state.authed = true;
                    write(
                        &mut stream,
                        &format!(
                            "{tag} OK [CAPABILITY IMAP4rev1 IDLE SPECIAL-USE MOVE UIDPLUS] Logged in\r\n"
                        ),
                    )
                    .await?;
                } else {
                    no(&mut stream, &tag, "[AUTHENTICATIONFAILED] bad credentials").await?;
                }
            }
            "LOGOUT" => {
                write(&mut stream, "* BYE Jmail mock logging out\r\n").await?;
                ok(&mut stream, &tag).await?;
                return Ok(());
            }
            "LIST" => {
                if !state.authed {
                    no(&mut stream, &tag, "not authenticated").await?;
                    continue;
                }
                let mailbox = shared.mailbox.lock().await.clone();
                for folder in &mailbox.folders {
                    let attrs = if folder.attrs.is_empty() {
                        "(\\HasNoChildren)".to_string()
                    } else {
                        format!("({})", folder.attrs.join(" "))
                    };
                    write(
                        &mut stream,
                        &format!("* LIST {attrs} \"/\" {}\r\n", quote(&folder.name)),
                    )
                    .await?;
                }
                ok(&mut stream, &tag).await?;
            }
            "SELECT" | "EXAMINE" => {
                if !state.authed {
                    no(&mut stream, &tag, "not authenticated").await?;
                    continue;
                }
                let name = unquote(command.split(' ').nth(1).unwrap_or(""));
                let mailbox = shared.mailbox.lock().await;
                let Some(folder) = mailbox.folder(&name) else {
                    no(&mut stream, &tag, "no such mailbox").await?;
                    continue;
                };
                state.selected = Some(folder.name.clone());
                write_select(&mut stream, folder).await?;
                ok(&mut stream, &tag).await?;
            }
            "UID" => {
                if !state.authed {
                    no(&mut stream, &tag, "not authenticated").await?;
                    continue;
                }
                handle_uid(&mut stream, &shared, &mut state, &tag, command).await?;
            }
            "EXPUNGE" => {
                let selected = state.selected.clone().unwrap_or_default();
                for seq in expunge_deleted(&shared, &selected).await.iter().rev() {
                    write(&mut stream, &format!("* {seq} EXPUNGE\r\n")).await?;
                }
                ok(&mut stream, &tag).await?;
            }
            "CLOSE" => {
                let selected = state.selected.clone().unwrap_or_default();
                expunge_deleted(&shared, &selected).await;
                state.selected = None;
                ok(&mut stream, &tag).await?;
            }
            "IDLE" => {
                idle(&mut stream, &shared, &state, &tag).await?;
            }
            "CHECK" | "NAMESPACE" | "CREATE" | "DELETE" | "RENAME" | "SUBSCRIBE"
            | "UNSUBSCRIBE" | "STATUS" | "APPEND" | "FETCH" | "SEARCH" | "STORE" | "COPY"
            | "MOVE" => {
                // Accepted but not implemented; the engine only uses UID forms.
                write(&mut stream, &format!("{tag} OK {name} completed\r\n")).await?;
            }
            _ => {
                write(&mut stream, &format!("{tag} BAD unknown command\r\n")).await?;
            }
        }
    }
}

/// Handle `UID FETCH|STORE|SEARCH|COPY|MOVE`.
async fn handle_uid(
    stream: &mut ServerStream,
    shared: &Arc<Shared>,
    state: &mut ConnState,
    tag: &str,
    command: &str,
) -> io::Result<()> {
    let mut parts = command.splitn(2, ' ');
    let _uid = parts.next();
    let Some(rest) = parts.next() else {
        return no(stream, tag, "UID needs a command").await;
    };
    let mut rest = rest.splitn(2, ' ');
    let sub = rest.next().unwrap_or("").to_uppercase();
    let Some(args) = rest.next() else {
        return no(stream, tag, "UID command needs arguments").await;
    };
    let Some(selected) = state.selected.clone() else {
        return no(stream, tag, "no mailbox selected").await;
    };

    match sub.as_str() {
        "FETCH" => uid_fetch(stream, shared, &selected, tag, args).await,
        "STORE" => uid_store(stream, shared, &selected, tag, args).await,
        "SEARCH" => {
            let mailbox = shared.mailbox.lock().await;
            let Some(folder) = mailbox.folder(&selected) else {
                return no(stream, tag, "no such mailbox").await;
            };
            let uids = folder
                .messages
                .iter()
                .map(|m| m.uid.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            write(stream, &format!("* SEARCH {uids}\r\n")).await?;
            ok(stream, tag).await
        }
        "COPY" | "MOVE" => {
            let mut parts = args.splitn(2, ' ');
            let set = parts.next().unwrap_or("");
            let dest = unquote(parts.next().unwrap_or(""));
            let max = shared
                .mailbox
                .lock()
                .await
                .folder(&selected)
                .map(|f| f.uidnext().saturating_sub(1).max(1))
                .unwrap_or(1);
            let Some(set) = SeqSet::parse(set, max) else {
                return no(stream, tag, "bad sequence set").await;
            };

            {
                let mut mailbox = shared.mailbox.lock().await;
                let (copies, source_uids) = {
                    let Some(source) = mailbox.folder(&selected) else {
                        return no(stream, tag, "no such mailbox").await;
                    };
                    let copies = source
                        .messages
                        .iter()
                        .filter(|m| set.contains(m.uid, source.uidnext().saturating_sub(1).max(1)))
                        .cloned()
                        .collect::<Vec<_>>();
                    let uids = source
                        .messages
                        .iter()
                        .filter(|m| set.contains(m.uid, source.uidnext().saturating_sub(1).max(1)))
                        .map(|m| m.uid)
                        .collect::<Vec<_>>();
                    (copies, uids)
                };
                for message in copies {
                    mailbox.push(&dest, message);
                }
                if sub == "MOVE" {
                    if let Some(source) = mailbox.folder_mut(&selected) {
                        for message in source.messages.iter_mut() {
                            if source_uids.contains(&message.uid) {
                                message.flags.push("\\Deleted".to_string());
                            }
                        }
                    }
                }
            }
            bump(shared);
            if sub == "MOVE" {
                for seq in expunge_deleted(shared, &selected).await.iter().rev() {
                    write(stream, &format!("* {seq} EXPUNGE\r\n")).await?;
                }
            }
            ok(stream, tag).await
        }
        _ => no(stream, tag, "unknown UID command").await,
    }
}

async fn uid_fetch(
    stream: &mut ServerStream,
    shared: &Arc<Shared>,
    folder: &str,
    tag: &str,
    args: &str,
) -> io::Result<()> {
    let mut parts = args.splitn(2, ' ');
    let set = parts.next().unwrap_or("");
    let items = parts.next().unwrap_or("FLAGS").to_uppercase();

    let want_uid = items.contains("UID");
    let want_flags = items.contains("FLAGS") || !items.contains("ENVELOPE");
    let want_envelope = items.contains("ENVELOPE");
    let body_full = items.contains("BODY.PEEK[]") || items.contains("BODY[]") || items.contains("RFC822");
    let body_header = items.contains("HEADER");

    let mailbox = shared.mailbox.lock().await;
    let Some(folder_state) = mailbox.folder(folder) else {
        return no(stream, tag, "no such mailbox").await;
    };
    let max = folder_state.uidnext().saturating_sub(1).max(1);
    let Some(set) = SeqSet::parse(set, max) else {
        return no(stream, tag, "bad sequence set").await;
    };

    for (index, message) in folder_state.messages.iter().enumerate() {
        if !set.contains(message.uid, max) {
            continue;
        }
        let seq = index as u32 + 1;
        let mut items_out = Vec::new();
        if want_uid {
            items_out.push(format!("UID {}", message.uid));
        }
        if want_flags {
            items_out.push(format!("FLAGS {}", message.flag_string()));
        }
        if want_envelope {
            items_out.push(format!("ENVELOPE {}", message.envelope()));
        }
        if body_full || body_header {
            let raw = message.raw();
            let bytes = if body_header {
                raw.split("\r\n\r\n")
                    .next()
                    .map(|header| format!("{header}\r\n\r\n"))
                    .unwrap_or(raw.clone())
            } else {
                raw.clone()
            };
            let label = if body_header { "BODY[HEADER]" } else { "BODY[]" };
            items_out.push(format!("{label} {{{}}}\r\n{}", bytes.len(), bytes));
        }
        write(stream, &format!("* {seq} FETCH ({})\r\n", items_out.join(" "))).await?;
    }
    ok(stream, tag).await
}

async fn uid_store(
    stream: &mut ServerStream,
    shared: &Arc<Shared>,
    folder: &str,
    tag: &str,
    args: &str,
) -> io::Result<()> {
    let mut parts = args.splitn(2, ' ');
    let set = parts.next().unwrap_or("");
    let op = parts.next().unwrap_or("").to_uppercase();
    let silent = op.contains(".SILENT");
    let flags = extract_flags(args);

    let mut mailbox = shared.mailbox.lock().await;
    let Some(folder_state) = mailbox.folder_mut(folder) else {
        return no(stream, tag, "no such mailbox").await;
    };
    let max = folder_state.uidnext().saturating_sub(1).max(1);
    let Some(set) = SeqSet::parse(set, max) else {
        return no(stream, tag, "bad sequence set").await;
    };
    let (add, remove) = if op.starts_with('+') {
        (true, false)
    } else if op.starts_with('-') {
        (false, true)
    } else {
        (false, false) // bare FLAGS: replace
    };

    for (index, message) in folder_state.messages.iter_mut().enumerate() {
        if !set.contains(message.uid, max) {
            continue;
        }
        if add {
            for flag in &flags {
                if !message.flags.contains(flag) {
                    message.flags.push(flag.clone());
                }
            }
        } else if remove {
            message.flags.retain(|f| !flags.contains(f));
        } else {
            message.flags = flags.clone();
        }
        if !silent {
            let seq = index as u32 + 1;
            write(
                stream,
                &format!(
                    "* {seq} FETCH (UID {} FLAGS {})\r\n",
                    message.uid,
                    message.flag_string()
                ),
            )
            .await?;
        }
    }
    drop(mailbox);
    bump(shared);
    ok(stream, tag).await
}

fn extract_flags(args: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for ch in args.chars() {
        match ch {
            '(' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                current.clear();
            }
            _ if depth >= 1 => {
                if ch == ' ' {
                    if !current.is_empty() {
                        flags.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(ch);
                }
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        flags.push(current);
    }
    flags
}

/// Remove messages flagged `\Deleted`, returning their old sequence numbers.
async fn expunge_deleted(shared: &Arc<Shared>, folder: &str) -> Vec<u32> {
    let mut mailbox = shared.mailbox.lock().await;
    let mut expunged = Vec::new();
    if let Some(folder_state) = mailbox.folder_mut(folder) {
        let mut seq = 0u32;
        folder_state.messages.retain(|message| {
            seq += 1;
            if message
                .flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case("\\Deleted"))
            {
                expunged.push(seq);
                false
            } else {
                true
            }
        });
    }
    drop(mailbox);
    bump(shared);
    expunged
}

async fn idle(
    stream: &mut ServerStream,
    shared: &Arc<Shared>,
    state: &ConnState,
    tag: &str,
) -> io::Result<()> {
    write(stream, "+ idling\r\n").await?;
    stream.flush().await?;

    let mut changed = shared.changed.subscribe();
    let mut last = *changed.borrow();
    loop {
        tokio::select! {
            line = read_line(stream) => {
                let Some(line) = line? else { return Ok(()) };
                if line.trim_end().eq_ignore_ascii_case("DONE") {
                    break;
                }
            }
            result = changed.changed() => {
                result.map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))?;
                if *changed.borrow() != last {
                    last = *changed.borrow();
                    if let Some(folder) = state.selected.clone() {
                        if let Some(folder_state) = shared.mailbox.lock().await.folder(&folder) {
                            write(stream, &format!("* {} EXISTS\r\n", folder_state.exists())).await?;
                        }
                    }
                }
            }
            _ = shared.idle_notify.notified() => {}
        }
    }
    ok(stream, tag).await
}

async fn write_select(stream: &mut ServerStream, folder: &crate::mailbox::MockFolder) -> io::Result<()> {
    write(stream, "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n").await?;
    write(
        stream,
        "* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n",
    )
    .await?;
    write(stream, &format!("* {} EXISTS\r\n", folder.exists())).await?;
    write(stream, "* 0 RECENT\r\n").await?;
    write(stream, "* OK [UIDVALIDITY 1] UIDs valid\r\n").await?;
    write(
        stream,
        &format!("* OK [UIDNEXT {}] Predicted next UID\r\n", folder.uidnext()),
    )
    .await?;
    Ok(())
}

async fn emit_changes(stream: &mut ServerStream, shared: &Arc<Shared>, state: &ConnState) -> io::Result<()> {
    if let Some(folder) = state.selected.clone() {
        if let Some(folder_state) = shared.mailbox.lock().await.folder(&folder) {
            write(stream, &format!("* {} EXISTS\r\n", folder_state.exists())).await?;
        }
    }
    Ok(())
}

async fn write(stream: &mut ServerStream, data: &str) -> io::Result<()> {
    stream.write_all(data.as_bytes()).await
}

async fn read_line(stream: &mut ServerStream) -> io::Result<Option<String>> {
    let mut line = String::new();
    match AsyncBufReadExt::read_line(stream, &mut line).await {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(line)),
        Err(error) => Err(error),
    }
}

fn parse_command(line: &str) -> Option<(String, &str)> {
    let mut parts = line.trim_end().splitn(2, ' ');
    let tag = parts.next()?.to_string();
    let command = parts.next()?;
    Some((tag, command))
}

async fn ok(stream: &mut ServerStream, tag: &str) -> io::Result<()> {
    write(stream, &format!("{tag} OK completed\r\n")).await
}

async fn no(stream: &mut ServerStream, tag: &str, reason: &str) -> io::Result<()> {
    write(stream, &format!("{tag} NO {reason}\r\n")).await
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').replace("\\\"", "\"")
}

/// Generate a throwaway self-signed certificate for the mock server.
fn generate_tls() -> MockTls {
    let key_pair = rcgen::KeyPair::generate().expect("key pair");
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .expect("cert params");
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".to_string().try_into().expect("dns")),
        rcgen::SanType::IpAddress(std::net::IpAddr::from([127, 0, 0, 1])),
    ];
    let certificate = params.self_signed(&key_pair).expect("self signed");
    let cert_der: Vec<u8> = certificate.der().to_vec();
    let key_der = key_pair.serialize_der();
    let key_der_copy = key_der.clone();

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der_copy)),
        )
        .expect("valid tls config");
    MockTls {
        cert_der,
        key_der,
        config: Arc::new(config),
    }
}
