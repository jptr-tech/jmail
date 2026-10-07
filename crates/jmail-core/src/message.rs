use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::account::AccountId;
use crate::folder::FolderId;

/// Stable identifier for a message row in the cache DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub i64);

/// A single RFC 5322 address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub name: String,
    pub email: String,
}

impl Address {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    /// Best display string: the name if present, otherwise the address.
    pub fn display(&self) -> &str {
        if self.name.is_empty() {
            &self.email
        } else {
            &self.name
        }
    }
}

/// The subset of IMAP flags Jmail cares about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub seen: bool,
    pub answered: bool,
    pub flagged: bool,
    pub draft: bool,
}

impl Flags {
    pub(crate) const SEEN: i64 = 1 << 0;
    pub(crate) const ANSWERED: i64 = 1 << 1;
    pub(crate) const FLAGGED: i64 = 1 << 2;
    pub(crate) const DRAFT: i64 = 1 << 3;

    pub fn to_i64(self) -> i64 {
        let mut bits = 0;
        if self.seen {
            bits |= Self::SEEN;
        }
        if self.answered {
            bits |= Self::ANSWERED;
        }
        if self.flagged {
            bits |= Self::FLAGGED;
        }
        if self.draft {
            bits |= Self::DRAFT;
        }
        bits
    }

    pub fn from_i64(bits: i64) -> Self {
        Self {
            seen: bits & Self::SEEN != 0,
            answered: bits & Self::ANSWERED != 0,
            flagged: bits & Self::FLAGGED != 0,
            draft: bits & Self::DRAFT != 0,
        }
    }
}

/// A message as stored in the local cache.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub uid: u32,
    /// RFC 5322 `Message-ID`.
    pub message_id: String,
    /// Conversation this message belongs to (see [`crate::threading`]).
    pub thread_id: String,
    /// RFC 5322 `In-Reply-To`, if any.
    pub in_reply_to: String,
    /// RFC 5322 `References` message-ids, in order.
    pub references: Vec<String>,
    pub subject: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// RFC 5322 `Reply-To`, if any.
    pub reply_to: Vec<Address>,
    pub date: OffsetDateTime,
    pub flags: Flags,
    pub snippet: String,
    pub has_attachments: bool,
}

impl Message {
    pub fn is_read(&self) -> bool {
        self.flags.seen
    }

    pub fn is_starred(&self) -> bool {
        self.flags.flagged
    }
}

/// A message that has not yet been persisted (no [`MessageId`]).
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub uid: u32,
    pub message_id: String,
    pub thread_id: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub subject: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub date: OffsetDateTime,
    pub flags: Flags,
    pub snippet: String,
    pub has_attachments: bool,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
}

/// The cached body of a message. HTML arrives in a later milestone.
#[derive(Debug, Clone, Default)]
pub struct MessageBody {
    pub text: Option<String>,
    pub html: Option<String>,
}

/// Metadata for one MIME attachment (contents are not cached in M3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub mime: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub inline: bool,
    /// MIME part path, e.g. `2`.
    pub part: String,
}
