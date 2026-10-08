//! IMAP backend over [`async_imap`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_imap::types::{Fetch, NameAttribute};
use async_imap::Client;
use futures_util::StreamExt;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::account::{Account, AccountId, Security};
use crate::credentials::Credential;
use crate::error::{Error, Result};
use crate::folder::{FolderId, SpecialUse};
use crate::message::{Address, Flags, NewMessage};

/// Server-side folder metadata from LIST.
#[derive(Debug, Clone)]
pub struct ServerFolder {
    pub path: String,
    pub special_use: SpecialUse,
}

/// A folder as seen by the server after SELECT.
#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub exists: u32,
    pub uid_validity: u32,
    pub uid_next: u32,
}

/// A message header fetched from the server.
#[derive(Debug, Clone)]
pub struct ServerMessage {
    pub uid: u32,
    pub flags: Flags,
    pub new: NewMessage,
}

/// Trust anchors used when establishing TLS.
#[derive(Clone)]
pub struct TlsTrust {
    roots: Arc<RootCertStore>,
    insecure: bool,
}

impl TlsTrust {
    /// Mozilla's root store (the default for real servers).
    pub fn system() -> Result<Self> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(Self {
            roots: Arc::new(roots),
            insecure: false,
        })
    }

    /// System roots, but skip certificate verification entirely. Only used when
    /// the user has explicitly accepted an untrusted certificate.
    pub fn accepting_invalid() -> Result<Self> {
        let mut trust = Self::system()?;
        trust.insecure = true;
        Ok(trust)
    }

    /// The trust policy for an account.
    pub fn for_account(account: &Account) -> Result<Self> {
        if account.accept_invalid_certs {
            Self::accepting_invalid()
        } else {
            Self::system()
        }
    }

    /// System roots plus one pinned certificate (used to trust the mock server
    /// in tests).
    pub fn with_pinned(cert_der: &[u8]) -> Result<Self> {
        let mut pinned = (*Self::system()?.roots).clone();
        pinned
            .add(CertificateDer::from(cert_der.to_vec()))
            .map_err(|error| Error::Network(format!("bad pinned certificate: {error}")))?;
        Ok(Self {
            roots: Arc::new(pinned),
            insecure: false,
        })
    }

    fn config(&self) -> ClientConfig {
        let mut config = ClientConfig::builder()
            .with_root_certificates((*self.roots).clone())
            .with_no_client_auth();
        if self.insecure {
            config
                .dangerous()
                .set_certificate_verifier(Arc::new(NoCertificateVerification::new()));
        }
        config
    }
}

impl fmt::Debug for TlsTrust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTrust")
            .field("roots", &self.roots.len())
            .field("insecure", &self.insecure)
            .finish()
    }
}

/// A certificate verifier that accepts any certificate. This is only installed
/// when the user has explicitly confirmed an untrusted certificate for an
/// account; all other connections use the normal verifier.
#[derive(Debug)]
struct NoCertificateVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl NoCertificateVerification {
    fn new() -> Self {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
        Self { provider }
    }
}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Connection settings for one account.
#[derive(Debug, Clone)]
pub struct ImapSettings {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
}

type Session = async_imap::Session<StreamKind>;

/// The plaintext/TLS stream duality async-imap sees.
pub enum StreamKind {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl AsyncRead for StreamKind {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(inner) => std::pin::Pin::new(inner).poll_read(cx, buf),
            Self::Tls(inner) => std::pin::Pin::new(inner).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for StreamKind {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(inner) => std::pin::Pin::new(inner).poll_write(cx, buf),
            Self::Tls(inner) => std::pin::Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(inner) => std::pin::Pin::new(inner).poll_flush(cx),
            Self::Tls(inner) => std::pin::Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(inner) => std::pin::Pin::new(inner).poll_shutdown(cx),
            Self::Tls(inner) => std::pin::Pin::new(inner).poll_shutdown(cx),
        }
    }
}

impl fmt::Debug for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plain(_) => "StreamKind::Plain",
            Self::Tls(_) => "StreamKind::Tls",
        })
    }
}

/// An authenticated IMAP connection for one account.
pub struct ImapSession {
    session: Session,
    settings: ImapSettings,
    /// Whether the server supports the MOVE extension.
    supports_move: bool,
}

impl ImapSession {
    /// Connect and authenticate.
    ///
    /// The credential is accepted as anything convertible into a
    /// [`Credential`], so callers can pass a bare password or an OAuth2 token.
    pub async fn connect(
        settings: ImapSettings,
        credential: impl Into<Credential>,
        trust: &TlsTrust,
    ) -> Result<Self> {
        let credential = credential.into();
        let tls_config = Arc::new(trust.config());
        let server_name = ServerName::try_from(settings.host.clone())
            .map_err(|error| Error::Network(format!("invalid server name {:?}: {error}", settings.host)))?;

        tracing::debug!(
            host = %settings.host,
            port = settings.port,
            security = ?settings.security,
            "connecting to IMAP server"
        );

        let mut session = match settings.security {
            Security::None => {
                let tcp = TcpStream::connect((settings.host.as_str(), settings.port))
                    .await
                    .map_err(protocol)?;
                let mut client = Client::new(StreamKind::Plain(tcp));
                read_greeting(&mut client).await?;
                authenticate(client, &settings.username, &credential).await?
            }
            Security::Tls => {
                let tcp = TcpStream::connect((settings.host.as_str(), settings.port))
                    .await
                    .map_err(protocol)?;
                let connector = tokio_rustls::TlsConnector::from(tls_config);
                let tls = connector
                    .connect(server_name, tcp)
                    .await
                    .map_err(protocol)?;
                let mut client = Client::new(StreamKind::Tls(tls));
                read_greeting(&mut client).await?;
                authenticate(client, &settings.username, &credential).await?
            }
            Security::StartTls => {
                let tcp = TcpStream::connect((settings.host.as_str(), settings.port))
                    .await
                    .map_err(protocol)?;
                let mut client = Client::new(StreamKind::Plain(tcp));
                // The greeting is consumed here, before the stream is upgraded.
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .map_err(protocol)?;
                let StreamKind::Plain(plain) = client.into_inner() else {
                    return Err(Error::Protocol("unexpected TLS stream".into()));
                };
                let connector = tokio_rustls::TlsConnector::from(tls_config);
                let tls = connector
                    .connect(server_name, plain)
                    .await
                    .map_err(protocol)?;
                let client = Client::new(StreamKind::Tls(tls));
                authenticate(client, &settings.username, &credential).await?
            }
        };

        let capabilities = session.capabilities().await.map_err(protocol)?;
        let supports_move = capabilities.has_str("MOVE");
        Ok(Self {
            session,
            settings,
            supports_move,
        })
    }

    pub fn settings(&self) -> &ImapSettings {
        &self.settings
    }

    pub fn supports_move(&self) -> bool {
        self.supports_move
    }

    /// List folders and their special-use classification.
    pub async fn list_folders(&mut self) -> Result<Vec<ServerFolder>> {
        let stream = self.session.list(Some(""), Some("*")).await.map_err(protocol)?;
        let mut folders = Vec::new();
        tokio::pin!(stream);
        while let Some(name) = stream.next().await {
            let name = name.map_err(protocol)?;
            let mut special_use = if name.name().eq_ignore_ascii_case("INBOX") {
                SpecialUse::Inbox
            } else {
                SpecialUse::None
            };
            let mut selectable = true;
            let mut virtual_view = false;
            for attribute in name.attributes() {
                if matches!(attribute, NameAttribute::NoSelect) {
                    // Containers such as Gmail's `[Gmail]` and `Mailspring`
                    // cannot be SELECTed and must not be synced.
                    selectable = false;
                }
                if is_virtual_view(attribute) {
                    // Gmail's Starred / Important re-list messages that are
                    // already held in a real folder or All Mail; syncing them
                    // only adds duplicates.
                    virtual_view = true;
                }
                if let Some(use_) = special_use_of(attribute) {
                    special_use = use_;
                }
            }
            if !selectable {
                tracing::debug!(path = %name.name(), "skipping non-selectable mailbox");
                continue;
            }
            if virtual_view {
                tracing::debug!(path = %name.name(), "skipping virtual duplicate mailbox");
                continue;
            }
            folders.push(ServerFolder {
                path: name.name().to_string(),
                special_use,
            });
        }
        Ok(folders)
    }

    /// Incrementally fetch headers for UIDs above `since_uid`.
    pub async fn fetch_headers(
        &mut self,
        path: &str,
        since_uid: u32,
    ) -> Result<Vec<ServerMessage>> {
        self.select(path).await?;
        let range = format!("{}:*", since_uid + 1);
        let stream = self
            .session
            .uid_fetch(range, "(UID FLAGS ENVELOPE BODY.PEEK[HEADER])")
            .await
            .map_err(protocol)?;
        let mut messages = Vec::new();
        tokio::pin!(stream);
        while let Some(fetch) = stream.next().await {
            let fetch = fetch.map_err(protocol)?;
            if let Some(message) = server_message_from(fetch) {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// Fetch a complete message by UID.
    pub async fn fetch_full(&mut self, path: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        Ok(self
            .fetch_full_many(path, &[uid])
            .await?
            .into_iter()
            .next()
            .map(|(_, body)| body))
    }

    /// Fetch complete messages for several UIDs from one folder in a single
    /// round trip (`UID FETCH uid1,uid2,… (BODY.PEEK[])`). Returns `(uid, body)`
    /// pairs; UIDs the server no longer has are simply absent.
    pub async fn fetch_full_many(
        &mut self,
        path: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        self.select(path).await?;
        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let stream = self
            .session
            .uid_fetch(set, "(UID BODY.PEEK[])")
            .await
            .map_err(protocol)?;
        let mut bodies = Vec::new();
        tokio::pin!(stream);
        while let Some(fetch) = stream.next().await {
            let fetch = fetch.map_err(protocol)?;
            if let (Some(uid), Some(body)) = (fetch.uid, fetch.body()) {
                bodies.push((uid, body.to_vec()));
            }
        }
        Ok(bodies)
    }

    /// Replace a message's flags on the server.
    pub async fn set_flags(&mut self, path: &str, uid: u32, flags: Flags) -> Result<()> {
        self.select(path).await?;
        let mut stored = String::new();
        if flags.seen {
            stored.push_str("\\Seen ");
        }
        if flags.flagged {
            stored.push_str("\\Flagged ");
        }
        if flags.answered {
            stored.push_str("\\Answered ");
        }
        if flags.draft {
            stored.push_str("\\Draft ");
        }
        let stored = stored.trim_end();
        let query = if stored.is_empty() {
            "FLAGS ()".to_string()
        } else {
            format!("FLAGS ({stored})")
        };
        let stream = self
            .session
            .uid_store(uid.to_string(), query)
            .await
            .map_err(protocol)?;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            item.map_err(protocol)?;
        }
        Ok(())
    }

    /// Move a message between folders.
    pub async fn move_message(&mut self, path: &str, uid: u32, destination: &str) -> Result<()> {
        if self.supports_move {
            self.select(path).await?;
            self.session
                .uid_mv(uid.to_string(), destination)
                .await
                .map_err(protocol)?;
            return Ok(());
        }
        // Fallback: COPY + mark deleted + EXPUNGE.
        self.select(path).await?;
        self.session
            .uid_copy(uid.to_string(), destination)
            .await
            .map_err(protocol)?;
        {
            let stream = self
                .session
                .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                .await
                .map_err(protocol)?;
            tokio::pin!(stream);
            while let Some(item) = stream.next().await {
                item.map_err(protocol)?;
            }
        }
        let stream = self.session.expunge().await.map_err(protocol)?;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            item.map_err(protocol)?;
        }
        Ok(())
    }

    /// Append a raw RFC 5322 message to `path` with the given flags.
    pub async fn append(&mut self, path: &str, flags: &str, raw: &[u8]) -> Result<()> {
        self.session
            .append(path, Some(flags), None, raw)
            .await
            .map_err(protocol)
    }

    /// Permanently remove a message by UID (`\Deleted` + `EXPUNGE`).
    pub async fn delete_message(&mut self, path: &str, uid: u32) -> Result<()> {
        self.select(path).await?;
        {
            let stream = self
                .session
                .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                .await
                .map_err(protocol)?;
            tokio::pin!(stream);
            while let Some(item) = stream.next().await {
                item.map_err(protocol)?;
            }
        }
        let stream = self.session.expunge().await.map_err(protocol)?;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            item.map_err(protocol)?;
        }
        Ok(())
    }

    /// Clone the unsolicited-responses channel so IDLE waits can observe
    /// EXISTS/EXPUNGE pushes (async-imap routes them there instead of
    /// returning from the wait future).
    pub fn unsolicited_receiver(
        &self,
    ) -> async_channel::Receiver<async_imap::types::UnsolicitedResponse> {
        self.session.unsolicited_responses.clone()
    }

    /// Begin an IDLE session (consumes the connection; [`idle::Handle::done`]
    /// hands it back).
    pub fn into_idle(self) -> async_imap::extensions::idle::Handle<StreamKind> {
        self.session.idle()
    }

    /// Re-wrap a raw session (returned from an IDLE handle) with its config.
    pub fn from_session(
        session: Session,
        settings: ImapSettings,
        supports_move: bool,
    ) -> Self {
        Self {
            session,
            settings,
            supports_move,
        }
    }

    /// Check for server-side changes (NOOP) and return the folder state.
    pub async fn refresh(&mut self, path: &str) -> Result<MailboxInfo> {
        self.session.noop().await.map_err(protocol)?;
        self.select(path).await
    }

    pub async fn select(&mut self, path: &str) -> Result<MailboxInfo> {
        let mailbox = self.session.select(path).await.map_err(protocol)?;
        Ok(MailboxInfo {
            exists: mailbox.exists,
            uid_validity: mailbox.uid_validity.unwrap_or(0),
            uid_next: mailbox.uid_next.unwrap_or(1),
        })
    }
}

/// How long to wait for the server's greeting after connecting.
const GREETING_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for authentication to complete.
const AUTH_TIMEOUT: Duration = Duration::from_secs(45);

/// Read and discard the server greeting.
///
/// Gmail (and other servers) announce themselves with an unsolicited `* OK`
/// greeting. `async-imap`'s `authenticate` does not skip untagged responses, so
/// without draining the greeting it reads it as the first SASL response and
/// then waits forever for a tagged reply that never arrives. (This affects
/// `AUTHENTICATE`; `LOGIN` tolerates the greeting.) See async-imap issue #84.
async fn read_greeting(client: &mut Client<StreamKind>) -> Result<()> {
    match tokio::time::timeout(GREETING_TIMEOUT, client.read_response()).await {
        Ok(Ok(Some(_greeting))) => {
            tracing::debug!("received IMAP server greeting");
            Ok(())
        }
        Ok(Ok(None)) => Err(Error::Network(
            "server closed the connection before sending its greeting".to_string(),
        )),
        Ok(Err(error)) => Err(protocol(error)),
        Err(_) => Err(Error::Network(
            "timed out waiting for the IMAP server greeting".to_string(),
        )),
    }
}

async fn authenticate(
    client: Client<StreamKind>,
    username: &str,
    credential: &Credential,
) -> Result<Session> {
    let attempt = async {
        match credential {
            Credential::Password(password) => client.login(username, password).await,
            Credential::Bearer { token, .. } => {
                client
                    .authenticate("XOAUTH2", XOAuth2::new(username, token))
                    .await
            }
        }
    };
    match tokio::time::timeout(AUTH_TIMEOUT, attempt).await {
        Ok(Ok(session)) => {
            tracing::debug!(user = username, "authenticated with IMAP server");
            Ok(session)
        }
        Ok(Err((_error, _client))) => Err(Error::Authentication(0)),
        Err(_) => Err(Error::Network(format!(
            "timed out authenticating {username}; the server did not respond"
        ))),
    }
}

/// SASL `XOAUTH2` authenticator, as understood by Gmail and Outlook.
struct XOAuth2 {
    response: String,
    sent: bool,
}

impl XOAuth2 {
    fn new(username: &str, token: &str) -> Self {
        Self {
            response: format!("user={username}\x01auth=Bearer {token}\x01\x01"),
            sent: false,
        }
    }
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        // The first continuation is empty; reply with the initial response.
        // If authentication fails the server follows with a base64 error
        // challenge, which we acknowledge with an empty line so it can return
        // the tagged NO.
        if self.sent {
            String::new()
        } else {
            self.sent = true;
            self.response.clone()
        }
    }
}

fn special_use_of(attribute: &NameAttribute<'_>) -> Option<SpecialUse> {
    Some(match attribute {
        NameAttribute::All => SpecialUse::All,
        NameAttribute::Archive => SpecialUse::Archive,
        NameAttribute::Drafts => SpecialUse::Drafts,
        NameAttribute::Junk => SpecialUse::Spam,
        NameAttribute::Sent => SpecialUse::Sent,
        NameAttribute::Trash => SpecialUse::Trash,
        _ => return None,
    })
}

/// Whether a mailbox is a virtual view that duplicates messages held elsewhere:
/// Gmail's `\Flagged` (Starred) and `\Important`. `\All` (All Mail) is *not*
/// skipped — it is the only home of archived mail that carries no label.
fn is_virtual_view(attribute: &NameAttribute<'_>) -> bool {
    match attribute {
        NameAttribute::Flagged => true,
        NameAttribute::Extension(name) => name.eq_ignore_ascii_case("\\Important"),
        _ => false,
    }
}

fn protocol(error: impl fmt::Display) -> Error {
    Error::Protocol(error.to_string())
}

fn server_message_from(fetch: Fetch) -> Option<ServerMessage> {
    let uid = fetch.uid?;
    let mut flags = Flags::default();
    for flag in fetch.flags() {
        match flag {
            async_imap::types::Flag::Seen => flags.seen = true,
            async_imap::types::Flag::Flagged => flags.flagged = true,
            async_imap::types::Flag::Answered => flags.answered = true,
            async_imap::types::Flag::Draft => flags.draft = true,
            _ => {}
        }
    }
    let envelope = fetch.envelope()?;
    let date = envelope
        .date
        .as_ref()
        .map(|date| String::from_utf8_lossy(date).to_string());
    let date = parse_date(date.as_deref());

    // `References` is not part of the IMAP envelope, so it comes from the raw
    // header; `In-Reply-To` prefers the header too (the envelope copy is the
    // fallback when the server truncates the header fetch).
    let (header_in_reply_to, references) = fetch
        .header()
        .map(parse_threading_headers)
        .unwrap_or_default();
    let in_reply_to = if header_in_reply_to.is_empty() {
        envelope
            .in_reply_to
            .as_ref()
            .map(|id| String::from_utf8_lossy(id).to_string())
            .unwrap_or_default()
    } else {
        header_in_reply_to
    };

    let new = NewMessage {
        account_id: AccountId(0),
        folder_id: FolderId(0),
        uid,
        message_id: envelope
            .message_id
            .as_ref()
            .map(|id| String::from_utf8_lossy(id).to_string())
            .unwrap_or_default(),
        thread_id: String::new(),
        in_reply_to,
        references,
        subject: envelope
            .subject
            .as_ref()
            .map(|s| String::from_utf8_lossy(s).to_string())
            .unwrap_or_default(),
        from: envelope
            .from
            .as_ref()
            .and_then(|addresses| addresses.first())
            .map(|address| address_of(address))
            .unwrap_or_else(|| Address::new("", "")),
        to: address_list(envelope.to.as_ref()),
        cc: address_list(envelope.cc.as_ref()),
        reply_to: address_list(envelope.reply_to.as_ref()),
        date,
        flags,
        snippet: String::new(),
        has_attachments: false,
        body_text: None,
        body_html: None,
    };
    // `thread_id` is assigned locally once the message is cached (see
    // `threading::assign_threads`), so it is left empty here.
    Some(ServerMessage {
        uid,
        flags: new.flags,
        new,
    })
}

/// Extract `(in_reply_to, references)` from a raw RFC 5322 header block.
fn parse_threading_headers(header: &[u8]) -> (String, Vec<String>) {
    let text = String::from_utf8_lossy(header);
    let mut in_reply_to = String::new();
    let mut references = Vec::new();
    let mut current: Option<&'static str> = None;
    let mut value = String::new();

    let mut flush = |name: Option<&'static str>, value: &str| {
        let ids = message_ids(value);
        match name {
            Some("in-reply-to") => {
                if let Some(first) = ids.into_iter().next() {
                    in_reply_to = first;
                }
            }
            Some("references") => references = ids,
            _ => {}
        }
    };

    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            value.push(' ');
            value.push_str(line.trim());
            continue;
        }
        flush(current, &value);
        current = None;
        value.clear();
        if let Some((name, rest)) = line.split_once(':') {
            current = match name.trim().to_ascii_lowercase().as_str() {
                "in-reply-to" => Some("in-reply-to"),
                "references" => Some("references"),
                _ => None,
            };
            if current.is_some() {
                value.push_str(rest.trim());
            }
        }
    }
    flush(current, &value);
    (in_reply_to, references)
}

/// Pull every `<id>` out of a header value (References lists several).
fn message_ids(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter_map(|token| {
            let id = crate::threading::normalize_message_id(token);
            (!id.is_empty()).then_some(id)
        })
        .collect()
}

fn address_of(address: &imap_proto::Address<'_>) -> Address {
    Address {
        name: address
            .name
            .as_ref()
            .map(|name| String::from_utf8_lossy(name).to_string())
            .unwrap_or_default(),
        email: format!(
            "{}@{}",
            address
                .mailbox
                .as_ref()
                .map(|mailbox| String::from_utf8_lossy(mailbox))
                .unwrap_or_default(),
            address
                .host
                .as_ref()
                .map(|host| String::from_utf8_lossy(host))
                .unwrap_or_default()
        ),
    }
}

fn address_list(addresses: Option<&Vec<imap_proto::Address<'_>>>) -> Vec<Address> {
    addresses
        .map(|list| list.iter().map(address_of).collect())
        .unwrap_or_default()
}

fn parse_date(value: Option<&str>) -> OffsetDateTime {
    value
        .and_then(|value| mail_parser::DateTime::parse_rfc822(value))
        .and_then(|parsed| OffsetDateTime::from_unix_timestamp(parsed.to_timestamp()).ok())
        .unwrap_or_else(OffsetDateTime::now_utc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::Authenticator;

    #[test]
    fn xoauth2_response_is_sasl_encoded() {
        let mut auth = XOAuth2::new("you@example.com", "ya29.token");
        assert_eq!(
            auth.process(b""),
            "user=you@example.com\x01auth=Bearer ya29.token\x01\x01"
        );
        // The server's error challenge is acknowledged with an empty response
        // so it can return the tagged NO.
        assert_eq!(auth.process(b"error"), "");
    }

    #[test]
    fn credential_accepts_a_bare_password() {
        let credential: Credential = "hunter2".into();
        assert!(matches!(credential, Credential::Password(ref p) if p == "hunter2"));
    }
}
