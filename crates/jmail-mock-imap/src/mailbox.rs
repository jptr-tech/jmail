//! In-memory mailbox used by the mock IMAP server.

/// An attachment carried by a [`MockMessage`].
#[derive(Clone, Debug)]
pub struct MockAttachment {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// A message stored in the mock server. Structured fields drive both the
/// RFC 822 rendering and the ENVELOPE response.
#[derive(Clone, Debug)]
pub struct MockMessage {
    /// UID assigned when the message is stored in a folder.
    pub uid: u32,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub to: Vec<(String, String)>,
    pub reply_to: Vec<(String, String)>,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub date: String,
    pub text: String,
    pub html: Option<String>,
    pub attachments: Vec<MockAttachment>,
    pub flags: Vec<String>,
    /// When set, `raw()` returns these bytes verbatim (used for messages
    /// appended by the client, whose MIME structure must be preserved).
    pub raw_override: Option<String>,
}

impl MockMessage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        subject: &str,
        from_name: &str,
        from_email: &str,
        to: &[(&str, &str)],
        message_id: &str,
        date: &str,
        text: &str,
        flags: &[&str],
    ) -> Self {
        Self {
            uid: 0,
            subject: subject.to_string(),
            from_name: from_name.to_string(),
            from_email: from_email.to_string(),
            to: to
                .iter()
                .map(|(name, email)| (name.to_string(), email.to_string()))
                .collect(),
            reply_to: Vec::new(),
            message_id: message_id.to_string(),
            in_reply_to: String::new(),
            references: Vec::new(),
            date: date.to_string(),
            text: text.to_string(),
            html: None,
            attachments: Vec::new(),
            flags: flags.iter().map(|f| f.to_string()).collect(),
            raw_override: None,
        }
    }

    /// Attach a file (`multipart/mixed`, base64-encoded).
    pub fn with_attachment(mut self, filename: &str, mime: &str, data: &[u8]) -> Self {
        self.attachments.push(MockAttachment {
            filename: filename.to_string(),
            mime: mime.to_string(),
            data: data.to_vec(),
        });
        self
    }

    /// Attach an HTML alternative body (renders as `multipart/alternative`).
    pub fn with_html(mut self, html: &str) -> Self {
        self.html = Some(html.to_string());
        self
    }

    /// Mark this message as a reply to `parent` (sets `In-Reply-To` and
    /// `References` so the client can reconstruct the thread).
    pub fn reply_to(mut self, parent: &str) -> Self {
        self.in_reply_to = parent.to_string();
        if self.references.is_empty() {
            self.references.push(parent.to_string());
        } else if !self.references.iter().any(|r| r == parent) {
            self.references.push(parent.to_string());
        }
        self
    }

    /// Set the full `References` chain explicitly.
    pub fn with_references(mut self, references: &[&str]) -> Self {
        self.references = references.iter().map(|r| r.to_string()).collect();
        self
    }

    /// Set a `Reply-To` address (e.g. a mailing list).
    pub fn with_reply_to(mut self, addresses: &[(&str, &str)]) -> Self {
        self.reply_to = addresses
            .iter()
            .map(|(name, email)| (name.to_string(), email.to_string()))
            .collect();
        self
    }

    /// Build a message from bytes appended by the client (`APPEND`). Structured
    /// fields are parsed from the headers so ENVELOPE works; the bytes are kept
    /// verbatim for `BODY[]`.
    pub fn from_raw(raw: &[u8], flags: &[String]) -> Self {
        let text = String::from_utf8_lossy(raw).to_string();
        let (header_block, _body) = text
            .split_once("\r\n\r\n")
            .or_else(|| text.split_once("\n\n"))
            .unwrap_or((text.as_str(), ""));

        let mut headers: Vec<(String, String)> = Vec::new();
        for line in header_block.lines() {
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                }
            } else if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let ids = |value: &str| -> Vec<String> {
            value
                .split_whitespace()
                .map(|token| token.trim_matches(|c| c == '<' || c == '>').to_string())
                .filter(|token| !token.is_empty())
                .collect()
        };

        let (from_name, from_email) = parse_single_address(&get("from"));
        let to = parse_address_list(&get("to"));
        let reply_to = parse_address_list(&get("reply-to"));
        Self {
            uid: 0,
            subject: get("subject"),
            from_name,
            from_email,
            to,
            reply_to,
            message_id: get("message-id")
                .trim_matches(|c| c == '<' || c == '>')
                .trim()
                .to_string(),
            in_reply_to: get("in-reply-to")
                .trim_matches(|c| c == '<' || c == '>')
                .trim()
                .to_string(),
            references: ids(&get("references")),
            date: get("date"),
            text: String::new(),
            html: None,
            attachments: Vec::new(),
            flags: flags.to_vec(),
            raw_override: Some(text),
        }
    }

    /// The message as an RFC 822 byte string.
    pub fn raw(&self) -> String {
        if let Some(raw) = &self.raw_override {
            return raw.clone();
        }
        let from = if self.from_name.is_empty() {
            format!("<{}>", self.from_email)
        } else {
            format!("{} <{}>", self.from_name, self.from_email)
        };
        let to = self
            .to
            .iter()
            .map(|(name, email)| format!("{name} <{email}>"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut headers = format!(
            "Message-ID: <{}>\r\nDate: {}\r\nSubject: {}\r\nFrom: {}\r\nTo: {}",
            self.message_id, self.date, self.subject, from, to
        );
        if !self.reply_to.is_empty() {
            let reply_to = self
                .reply_to
                .iter()
                .map(|(name, email)| format!("{name} <{email}>"))
                .collect::<Vec<_>>()
                .join(", ");
            headers.push_str(&format!("\r\nReply-To: {reply_to}"));
        }
        if !self.in_reply_to.is_empty() {
            headers.push_str(&format!("\r\nIn-Reply-To: <{}>", self.in_reply_to));
        }
        if !self.references.is_empty() {
            let refs = self
                .references
                .iter()
                .map(|r| format!("<{r}>"))
                .collect::<Vec<_>>()
                .join(" ");
            headers.push_str(&format!("\r\nReferences: {refs}"));
        }
        let body = match &self.html {
            Some(html) => {
                let boundary = "jmail-mock-boundary";
                format!(
                    "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n\
                     --{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n\
                     --{boundary}\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{}\r\n\
                     --{boundary}--\r\n",
                    self.text, html
                )
            }
            None => format!(
                "Content-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n",
                self.text
            ),
        };

        if self.attachments.is_empty() {
            return format!("{headers}\r\n{body}");
        }

        let mut raw = format!(
            "{headers}\r\nContent-Type: multipart/mixed; boundary=\"jmail-mock-mixed\"\r\n\r\n\
             --jmail-mock-mixed\r\n{body}\r\n"
        );
        for attachment in &self.attachments {
            raw.push_str(&format!(
                "--jmail-mock-mixed\r\nContent-Type: {}\r\n\
                 Content-Disposition: attachment; filename=\"{}\"\r\n\
                 Content-Transfer-Encoding: base64\r\n\r\n{}\r\n",
                attachment.mime,
                attachment.filename,
                wrap_base64(&base64(&attachment.data))
            ));
        }
        raw.push_str("--jmail-mock-mixed--\r\n");
        raw
    }

    /// IMAP `ENVELOPE` structure:
    /// `(date subject from sender reply-to to cc bcc in-reply-to message-id)`
    pub(crate) fn envelope(&self) -> String {
        let addr = |(name, email): &(String, String)| {
            let (mailbox, host) = split_email(email);
            format!("({} NIL {:?} {:?})", quote(name), mailbox, host)
        };
        let (from_mailbox, from_host) = split_email(&self.from_email);
        let address_group = format!(
            "({} NIL {:?} {:?})",
            quote(&self.from_name),
            from_mailbox,
            from_host
        );
        // `from`, `sender` and `reply_to` are address *lists* wrapped in parens.
        let from = format!("({address_group})");
        let reply_to = if self.reply_to.is_empty() {
            from.clone()
        } else {
            format!("({})", self.reply_to.iter().map(addr).collect::<Vec<_>>().join(" "))
        };
        let to = if self.to.is_empty() {
            "NIL".to_string()
        } else {
            format!("({})", self.to.iter().map(addr).collect::<Vec<_>>().join(" "))
        };
        let in_reply_to = if self.in_reply_to.is_empty() {
            "NIL".to_string()
        } else {
            format!("{:?}", self.in_reply_to)
        };
        format!(
            "({:?} {:?} {from} {from} {reply_to} {to} NIL NIL {in_reply_to} {:?})",
            self.date, self.subject, self.message_id,
        )
    }

    pub(crate) fn flag_string(&self) -> String {
        if self.flags.is_empty() {
            "()".to_string()
        } else {
            format!("({})", self.flags.join(" "))
        }
    }
}

pub(crate) fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Parse `Name <email>` or a bare address.
fn parse_single_address(input: &str) -> (String, String) {
    let input = input.trim();
    if let Some((name, rest)) = input.rsplit_once('<') {
        let email = rest.trim_end_matches('>').trim().to_string();
        let name = name.trim().trim_matches('"').trim().to_string();
        (name, email)
    } else {
        (String::new(), input.to_string())
    }
}

/// Parse a comma-separated address list.
fn parse_address_list(input: &str) -> Vec<(String, String)> {
    input
        .split(',')
        .map(parse_single_address)
        .filter(|(_, email)| !email.is_empty())
        .collect()
}

/// Split `local@host` for the IMAP ENVELOPE address tuple.
fn split_email(email: &str) -> (&str, &str) {
    email.split_once('@').unwrap_or((email, ""))
}

/// RFC 4648 base64 (standard alphabet, `=` padding) for attachment bodies.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Wrap base64 into 76-character MIME lines.
fn wrap_base64(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 + 1);
    let mut start = 0;
    while start < encoded.len() {
        let end = (start + 76).min(encoded.len());
        out.push_str(&encoded[start..end]);
        out.push_str("\r\n");
        start = end;
    }
    out
}

/// A folder in the mock server.
#[derive(Clone, Debug, Default)]
pub struct MockFolder {
    pub name: String,
    /// LIST attributes, e.g. `\Sent`, `\HasNoChildren`.
    pub attrs: Vec<String>,
    pub messages: Vec<MockMessage>,
    /// Monotonic next UID (RFC 3501 UIDNEXT), even across expunges.
    pub next_uid: u32,
}

impl MockFolder {
    pub fn new(name: &str, attrs: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            attrs: attrs.iter().map(|a| a.to_string()).collect(),
            messages: Vec::new(),
            next_uid: 1,
        }
    }

    pub fn exists(&self) -> u32 {
        self.messages.len() as u32
    }

    pub fn uidnext(&self) -> u32 {
        self.next_uid.max(1)
    }

    /// The RFC 6154 special-use attribute, if any.
    pub fn special_use(&self) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.starts_with('\\') && !matches!(a.as_str(), "\\HasNoChildren" | "\\HasChildren"))
            .map(|a| a.as_str())
    }

}

/// A minimal sequence-set parser: `1`, `1:*`, `2,4,10:*`.
#[derive(Debug, Clone)]
pub(crate) struct SeqSet {
    pub ranges: Vec<(u32, u32)>,
}

impl SeqSet {
    pub fn parse(s: &str, max: u32) -> Option<Self> {
        let mut ranges = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            if let Some((a, b)) = part.split_once(':') {
                let lo = resolve(a, max)?;
                let hi = resolve(b, max)?;
                ranges.push((lo.min(hi), lo.max(hi)));
            } else {
                let v = resolve(part, max)?;
                ranges.push((v, v));
            }
        }
        Some(Self { ranges })
    }

    pub fn contains(&self, value: u32, max: u32) -> bool {
        self.ranges
            .iter()
            .any(|(lo, hi)| {
                let lo = *lo;
                let hi = (*hi).max(lo).min(max.max(lo));
                value >= lo && value <= hi
            })
    }
}

fn resolve(token: &str, max: u32) -> Option<u32> {
    match token.trim() {
        "*" => Some(max.max(1)),
        other => other.parse().ok(),
    }
}

/// The whole mailbox: folders plus a broadcast used to wake IDLE sessions.
#[derive(Clone, Debug, Default)]
pub struct MockMailbox {
    pub folders: Vec<MockFolder>,
}

impl MockMailbox {
    pub fn add_folder(&mut self, name: &str, attrs: &[&str]) {
        self.folders.push(MockFolder::new(name, attrs));
    }

    pub fn folder(&self, name: &str) -> Option<&MockFolder> {
        self.folders.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn folder_mut(&mut self, name: &str) -> Option<&mut MockFolder> {
        self.folders
            .iter_mut()
            .find(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// Append a message, returning its UID (monotonic per folder).
    pub fn push(&mut self, folder: &str, message: MockMessage) -> u32 {
        let Some(folder) = self.folder_mut(folder) else {
            return 0;
        };
        let uid = folder.uidnext();
        folder.next_uid = uid + 1;
        let mut message = message;
        message.uid = uid;
        folder.messages.push(message);
        uid
    }

    /// A canned mailbox used by the demo binary and tests.
    pub fn demo() -> Self {
        let mut mailbox = Self::default();
        mailbox.add_folder("INBOX", &["\\HasNoChildren"]);
        mailbox.add_folder("Sent", &["\\Sent", "\\HasNoChildren"]);
        mailbox.add_folder("Drafts", &["\\Drafts", "\\HasNoChildren"]);
        mailbox.add_folder("Archive", &["\\Archive", "\\HasNoChildren"]);
        mailbox.add_folder("Trash", &["\\Trash", "\\HasNoChildren"]);
        mailbox.add_folder("Spam", &["\\Junk", "\\HasNoChildren"]);
        mailbox.add_folder("Projects", &["\\HasNoChildren"]);

        let me = [("You", "you@example.com")];
        let inbox = [
            MockMessage::new("Welcome to Jmail", "Ada Lovelace", "ada@example.org", &me, "m1@example.org", "Mon, 05 Oct 2026 08:12:00 +0000", "Hi! This is the first message in the mock inbox.", &[])
                .with_html(r#"<html>
<body>
<h1>Welcome to Jmail</h1>
<p>This is the <b>HTML</b> part, with <i>italics</i>, <u>underline</u> and inline <code>code</code>.</p>
<h2>Formatting checklist</h2>
<ul>
  <li>Bulleted list item</li>
  <li>Another item with a <a href="https://example.org">link</a></li>
</ul>
<ol>
  <li>First numbered item</li>
  <li>Second numbered item</li>
</ol>
<blockquote>Quoted text should stand apart from the body.</blockquote>
<table border="1" cellpadding="6" cellspacing="0">
  <tr><th align="left">Folder</th><th align="left">Messages</th></tr>
  <tr><td>Inbox</td><td>5</td></tr>
  <tr><td>Sent</td><td>1</td></tr>
</table>
<div style="background:#eef;border-left:4px solid #673AB7;padding:8px;margin-top:8px;">
  A callout with its own background colour.
</div>
<pre>fn main() {
    println!("Hello from a pre block");
}</pre>
<hr>
<p>Remote image (must not load): <img src="https://example.org/tracker.png" alt="remote"></p>
<script>alert('this script must be stripped')</script>
</body>
</html>"#),
            MockMessage::new("Re: Welcome to Jmail", "Ada Lovelace", "ada@example.org", &me, "m2@example.org", "Mon, 05 Oct 2026 09:30:00 +0000", "Did the conversation view land?\n\n> Hi! This is the first message in the mock inbox.\n> It should be folded away.", &["\\Seen"])
                .reply_to("m1@example.org"),
            MockMessage::new("Quarterly report", "Grace Hopper", "grace@example.org", &me, "m3@example.org", "Sun, 04 Oct 2026 14:05:00 +0000", "Attached is the quarterly report.", &[])
                .with_attachment("quarterly-report.pdf", "application/pdf", b"%PDF-1.4 mock quarterly report\n"),
            MockMessage::new("Re: Quarterly report", "Grace Hopper", "grace@example.org", &me, "m4@example.org", "Sun, 04 Oct 2026 18:44:00 +0000", "Thanks, reviewed it.", &["\\Seen", "\\Flagged"])
                .reply_to("m3@example.org")
                .with_reply_to(&[("Reports", "reports@example.org")]),
            MockMessage::new("Lunch tomorrow?", "Alan Turing", "alan@example.org", &me, "m5@example.org", "Fri, 02 Oct 2026 11:00:00 +0000", "Fancy lunch at noon?", &[]),
        ];
        for message in inbox {
            mailbox.push("INBOX", message);
        }
        mailbox.push(
            "Sent",
            MockMessage::new("Re: Welcome to Jmail", "You", "you@example.com", &[("Ada Lovelace", "ada@example.org")], "s1@example.org", "Mon, 05 Oct 2026 09:31:00 +0000", "It landed, thanks!", &["\\Seen"])
                .reply_to("m2@example.org"),
        );
        mailbox.push(
            "Drafts",
            MockMessage::new("Notes (draft)", "You", "you@example.com", &me, "d1@example.org", "Mon, 05 Oct 2026 10:00:00 +0000", "Draft notes.", &["\\Draft"]),
        );
        mailbox.push(
            "Archive",
            MockMessage::new("Old thread", "Margaret Hamilton", "margaret@example.org", &me, "a1@example.org", "Tue, 22 Sep 2026 09:00:00 +0000", "Archived discussion.", &["\\Seen"]),
        );
        mailbox.push(
            "Trash",
            MockMessage::new("You have won", "spam@example.net", "spam@example.net", &me, "t1@example.net", "Wed, 30 Sep 2026 03:14:00 +0000", "Click here!", &["\\Seen"]),
        );
        mailbox.push(
            "Projects",
            MockMessage::new("Milestone review", "Linus Torvalds", "linus@example.org", &me, "p1@example.org", "Thu, 01 Oct 2026 16:20:00 +0000", "Let's review the milestone.", &[]),
        );
        mailbox
    }
}
