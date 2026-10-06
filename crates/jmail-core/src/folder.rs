use crate::account::AccountId;

/// Stable identifier for a folder, matching the primary key in the cache DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId(pub i64);

/// RFC 6154 SPECIAL-USE classification for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialUse {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Spam,
    All,
    /// A user folder with no special meaning.
    None,
}

impl SpecialUse {
    /// Map the IMAP `\Special` attribute (without the backslash) to our enum.
    pub fn from_imap_attr(attr: &str) -> Self {
        match attr.trim_start_matches('\\').to_ascii_lowercase().as_str() {
            "inbox" => Self::Inbox,
            "sent" | "sentmail" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "archive" | "all" | "allmail" => Self::Archive,
            "junk" | "spam" => Self::Spam,
            _ => Self::None,
        }
    }

    /// Human readable label for the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
            Self::Trash => "Trash",
            Self::Archive => "Archive",
            Self::Spam => "Spam",
            Self::All => "All Mail",
            Self::None => "",
        }
    }

    pub fn to_i64(self) -> i64 {
        match self {
            Self::Inbox => 0,
            Self::Sent => 1,
            Self::Drafts => 2,
            Self::Trash => 3,
            Self::Archive => 4,
            Self::Spam => 5,
            Self::All => 6,
            Self::None => 7,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Inbox,
            1 => Self::Sent,
            2 => Self::Drafts,
            3 => Self::Trash,
            4 => Self::Archive,
            5 => Self::Spam,
            6 => Self::All,
            _ => Self::None,
        }
    }
}

/// A mail folder within an account.
#[derive(Debug, Clone)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: AccountId,
    /// Display name (last path component).
    pub name: String,
    /// Full IMAP mailbox path, e.g. `[Gmail]/All Mail`.
    pub path: String,
    pub special_use: SpecialUse,
    pub unread: u32,
    pub total: u32,
    /// IMAP UIDVALIDITY, used to detect server-side resets.
    pub uidvalidity: u32,
    /// Next expected UID, used for incremental sync.
    pub uidnext: u32,
    /// Highest message UID already cached for this folder.
    pub last_uid: u32,
}
