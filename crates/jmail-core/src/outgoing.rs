//! Outgoing messages: parse addresses, build RFC 5322/MIME with [`lettre`],
//! and serialize for SMTP or IMAP `APPEND`.

use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment, Mailbox, Message as LettreMessage, MultiPart};
use lettre::Address as LettreAddress;

use crate::error::{Error, Result};
use crate::markdown;
use crate::message::{Address, Message, MessageId};

/// An attachment to include when sending.
#[derive(Debug, Clone)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
    pub inline: bool,
    pub content_id: Option<String>,
}

/// Identifies a draft stored on the server, so it can be replaced or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRef {
    /// IMAP folder path of the draft.
    pub folder: String,
    /// UID of the draft message in that folder.
    pub uid: u32,
    /// The draft's `Message-ID` (without angle brackets).
    pub message_id: String,
}

/// A message being composed, in markdown, ready to send or save as a draft.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// The `Message-ID` without angle brackets; kept so the sent/draft copy can
    /// be located after `APPEND`.
    pub message_id: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub markdown: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
    /// The original message whose parts should be re-attached when forwarding.
    /// Filled in by the sync worker at send time.
    pub forward_of: Option<MessageId>,
}

impl OutgoingMessage {
    /// A fresh, unique `Message-ID` (without angle brackets) for `from_email`.
    pub fn generate_message_id(from_email: &str) -> String {
        let domain = from_email
            .split_once('@')
            .map(|(_, domain)| domain)
            .filter(|domain| !domain.is_empty())
            .unwrap_or("jmail.local");
        format!("{}.{}@{}", uuid::Uuid::new_v4().simple(), std::process::id(), domain)
    }

    /// Build the message for sending: a `multipart/alternative` (text + HTML)
    /// wrapped in `multipart/mixed` when attachments are present.
    pub fn build_message(&self) -> Result<LettreMessage> {
        let builder = self.base_builder()?;
        let plain = markdown::render_text(&self.markdown);
        let html = markdown::render_html(&self.markdown);
        let alternative = MultiPart::alternative_plain_html(plain, html);

        if self.attachments.is_empty() {
            builder.multipart(alternative).map_err(build_error)
        } else {
            let mut mixed = MultiPart::mixed().multipart(alternative);
            for attachment in &self.attachments {
                mixed = mixed.singlepart(attachment_part(attachment));
            }
            builder.multipart(mixed).map_err(build_error)
        }
    }

    /// Build the draft to `APPEND`: the raw markdown as `text/plain`, tagged
    /// with headers so it can be recognised and reopened.
    pub fn build_draft(&self) -> Result<LettreMessage> {
        let builder = self.base_builder()?.raw_header(HeaderValue::new(
            HeaderName::new_from_ascii_str("X-Jmail-Markdown"),
            "1".to_string(),
        ));
        builder
            .singlepart(
                lettre::message::SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(self.markdown.clone()),
            )
            .map_err(build_error)
    }

    fn base_builder(&self) -> Result<lettre::message::MessageBuilder> {
        let mut builder = LettreMessage::builder()
            .from(mailbox(&self.from)?)
            .subject(sanitize_header(&self.subject))
            .message_id(Some(format!("<{}>", self.message_id)))
            .date_now();
        for address in &self.to {
            builder = builder.to(mailbox(address)?);
        }
        for address in &self.cc {
            builder = builder.cc(mailbox(address)?);
        }
        for address in &self.bcc {
            builder = builder.bcc(mailbox(address)?);
        }
        if !self.in_reply_to.is_empty() {
            builder = builder.in_reply_to(format!("<{}>", self.in_reply_to));
        }
        if !self.references.is_empty() {
            let references = self
                .references
                .iter()
                .map(|id| format!("<{id}>"))
                .collect::<Vec<_>>()
                .join(" ");
            builder = builder.references(references);
        }
        Ok(builder)
    }
}

fn attachment_part(attachment: &OutgoingAttachment) -> lettre::message::SinglePart {
    let content_type = ContentType::parse(&attachment.mime).unwrap_or(ContentType::TEXT_PLAIN);
    match (&attachment.content_id, attachment.inline) {
        (Some(content_id), true) => Attachment::new_inline_with_name(
            content_id.clone(),
            attachment.filename.clone(),
        )
        .body(attachment.data.clone(), content_type),
        _ => Attachment::new(attachment.filename.clone())
            .body(attachment.data.clone(), content_type),
    }
}

fn mailbox(address: &Address) -> Result<Mailbox> {
    let email = sanitize_header(&address.email);
    let parsed: LettreAddress = email.parse().map_err(|error| {
        Error::Config(format!("invalid email address {email:?}: {error}"))
    })?;
    if address.name.is_empty() {
        Ok(Mailbox::new(None, parsed))
    } else {
        Ok(Mailbox::new(Some(sanitize_header(&address.name)), parsed))
    }
}

/// Strip CR/LF so user input cannot inject headers.
pub fn sanitize_header(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_error(error: lettre::error::Error) -> Error {
    Error::Config(format!("could not build message: {error}"))
}

/// Parse a free-form recipient field (`Name <a@b>, c@d; ...`) into addresses.
pub fn parse_address_list(input: &str) -> Vec<Address> {
    input
        .split([',', ';'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter_map(parse_address)
        .collect()
}

/// Parse a single `Name <email>` or bare `email`.
pub fn parse_address(input: &str) -> Option<Address> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some((name, rest)) = input.rsplit_once('<') {
        let email = rest.trim_end_matches('>').trim();
        let name = name.trim().trim_matches('"').trim();
        if email.is_empty() {
            return None;
        }
        return Some(Address::new(name, email));
    }
    Some(Address::new("", input))
}

/// Add a reply/forward subject prefix unless it is already present.
pub fn prefixed_subject(subject: &str, prefix: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return prefix.trim_end().to_string();
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with(&prefix.to_ascii_lowercase()) {
        trimmed.to_string()
    } else {
        format!("{prefix}{trimmed}")
    }
}

/// Choose which message a conversation reply should target: the latest message
/// in the thread (so `In-Reply-To`/`References` and the quote refer to the
/// message actually being replied to).
pub fn select_reply_target(messages: &[Message]) -> Option<&Message> {
    messages.last()
}

/// The address a reply should go to: `Reply-To` if present, else `From`.
pub fn reply_address(target: &Message) -> Address {
    target
        .reply_to
        .first()
        .cloned()
        .unwrap_or_else(|| target.from.clone())
}

/// Recipients for a reply. Reply-all adds the other original recipients to Cc,
/// excluding the user's own addresses and the primary recipient.
pub fn reply_recipients(
    target: &Message,
    my_addresses: &[String],
    reply_all: bool,
) -> (Vec<Address>, Vec<Address>) {
    let is_mine = |address: &Address| {
        my_addresses
            .iter()
            .any(|mine| mine.eq_ignore_ascii_case(&address.email))
    };
    let mut primary = reply_address(target);
    if is_mine(&primary) {
        // We sent the target message; address another participant instead of
        // replying to ourselves.
        if let Some(other) = target
            .to
            .iter()
            .chain(target.cc.iter())
            .find(|address| !is_mine(address))
        {
            primary = other.clone();
        }
    }
    let to = vec![primary.clone()];
    let mut cc = Vec::new();
    if reply_all {
        for address in target.to.iter().chain(target.cc.iter()) {
            if is_mine(address)
                || address.email.eq_ignore_ascii_case(&primary.email)
                || to.iter()
                    .chain(cc.iter())
                    .any(|other| other.email.eq_ignore_ascii_case(&address.email))
            {
                continue;
            }
            cc.push(address.clone());
        }
    }
    (to, cc)
}

/// Format an address for a header or the reading pane.
pub fn display_address(address: &Address) -> String {
    if address.name.is_empty() {
        address.email.clone()
    } else {
        format!("{} <{}>", address.name, address.email)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OutgoingMessage {
        OutgoingMessage {
            message_id: "abc@example.org".to_string(),
            from: Address::new("You", "you@example.org"),
            to: vec![Address::new("Ada", "ada@example.org")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Hello there".to_string(),
            markdown: "# Hi\n\nThis is **bold**.".to_string(),
            in_reply_to: "parent@example.org".to_string(),
            references: vec!["root@example.org".to_string(), "parent@example.org".to_string()],
            attachments: Vec::new(),
            forward_of: None,
        }
    }

    #[test]
    fn parses_display_names_and_bare_addresses() {
        let addresses = parse_address_list("Ada Lovelace <ada@example.org>, grace@example.org");
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].name, "Ada Lovelace");
        assert_eq!(addresses[0].email, "ada@example.org");
        assert_eq!(addresses[1].email, "grace@example.org");
    }

    #[test]
    fn subject_prefix_is_idempotent() {
        assert_eq!(prefixed_subject("Hi", "Re: "), "Re: Hi");
        assert_eq!(prefixed_subject("Re: Hi", "Re: "), "Re: Hi");
        assert_eq!(prefixed_subject("", "Re: "), "Re:");
    }

    #[test]
    fn builds_multipart_alternative_with_headers() {
        let raw = String::from_utf8(sample().build_message().unwrap().formatted()).unwrap();
        assert!(raw.contains("Message-ID: <abc@example.org>"));
        assert!(raw.contains("In-Reply-To: <parent@example.org>"));
        assert!(raw.contains("References: <root@example.org> <parent@example.org>"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("<strong>bold</strong>"));
    }

    #[test]
    fn attaches_files_as_multipart_mixed() {
        let mut message = sample();
        message.attachments.push(OutgoingAttachment {
            filename: "note.txt".to_string(),
            mime: "text/plain".to_string(),
            data: b"hello".to_vec(),
            inline: false,
            content_id: None,
        });
        let raw = String::from_utf8(message.build_message().unwrap().formatted()).unwrap();
        assert!(raw.contains("multipart/mixed"));
        assert!(raw.contains("filename=\"note.txt\""));
    }

    #[test]
    fn draft_carries_markdown_marker_header() {
        let raw = String::from_utf8(sample().build_draft().unwrap().formatted()).unwrap();
        assert!(raw.contains("X-Jmail-Markdown: 1"));
        assert!(raw.contains("# Hi"));
    }

    #[test]
    fn sanitizes_header_injection() {
        assert_eq!(sanitize_header("a\r\nBcc: evil@x"), "a Bcc: evil@x");
    }

    fn message(from_email: &str, message_id: &str) -> Message {
        Message {
            id: MessageId(0),
            account_id: crate::account::AccountId(0),
            folder_id: crate::folder::FolderId(0),
            uid: 0,
            message_id: message_id.to_string(),
            thread_id: String::new(),
            in_reply_to: String::new(),
            references: Vec::new(),
            subject: "Subject".to_string(),
            from: Address::new("", from_email),
            to: vec![Address::new("", "me@example.org")],
            cc: Vec::new(),
            reply_to: Vec::new(),
            date: time::OffsetDateTime::now_utc(),
            flags: crate::message::Flags::default(),
            snippet: String::new(),
            has_attachments: false,
        }
    }

    #[test]
    fn reply_targets_latest_message() {
        let messages = vec![
            message("ada@example.org", "a"),
            message("me@example.org", "b"),
        ];
        let target = select_reply_target(&messages).unwrap();
        assert_eq!(target.message_id, "b");
    }

    #[test]
    fn reply_to_own_message_addresses_the_other_participant() {
        let mut target = message("me@example.org", "b");
        target.to = vec![Address::new("Alan", "alan@example.org")];
        let (to, cc) = reply_recipients(&target, &["me@example.org".to_string()], false);
        assert_eq!(to[0].email, "alan@example.org");
        assert!(cc.is_empty());
    }

    #[test]
    fn reply_prefers_reply_to_header() {
        let mut target = message("ada@example.org", "a");
        target.reply_to = vec![Address::new("List", "list@example.org")];
        assert_eq!(reply_address(&target).email, "list@example.org");
    }

    #[test]
    fn reply_all_excludes_me_and_primary() {
        let mut target = message("ada@example.org", "a");
        target.to = vec![
            Address::new("", "me@example.org"),
            Address::new("", "grace@example.org"),
        ];
        target.cc = vec![Address::new("", "alan@example.org")];
        let (to, cc) = reply_recipients(&target, &["me@example.org".to_string()], true);
        assert_eq!(to[0].email, "ada@example.org");
        assert_eq!(
            cc.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(),
            vec!["grace@example.org", "alan@example.org"]
        );
    }
}
