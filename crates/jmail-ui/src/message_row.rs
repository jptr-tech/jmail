use gtk::glib;
use jmail_core::{Flags, Message};

/// A single row in the message list. Plain Rust data wrapped in a
/// `glib::BoxedAnyObject` so it can live in a `gio::ListStore`.
#[derive(Clone, Debug)]
pub struct MessageRow {
    pub id: i64,
    pub account_id: i64,
    pub thread_id: String,
    pub subject: String,
    pub sender: String,
    pub snippet: String,
    pub date_label: String,
    pub flags: Flags,
    pub unread: bool,
    pub starred: bool,
}

impl MessageRow {
    pub fn from_message(message: &Message) -> Self {
        let sender = if message.from.name.is_empty() {
            message.from.email.clone()
        } else {
            message.from.name.clone()
        };
        Self {
            id: message.id.0,
            account_id: message.account_id.0,
            thread_id: message.thread_id.clone(),
            subject: if message.subject.is_empty() {
                "(no subject)".to_string()
            } else {
                message.subject.clone()
            },
            sender,
            snippet: message.snippet.clone(),
            date_label: format_timestamp(message.date.unix_timestamp()),
            flags: message.flags,
            unread: !message.flags.seen,
            starred: message.flags.flagged,
        }
    }

    /// Text used by the search filter.
    pub fn matches(&self, query: &str) -> bool {
        self.subject.to_lowercase().contains(query)
            || self.sender.to_lowercase().contains(query)
            || self.snippet.to_lowercase().contains(query)
    }
}

/// Format a Unix timestamp for the list and conversation headers.
pub fn format_timestamp(timestamp: i64) -> String {
    glib::DateTime::from_unix_local(timestamp)
        .ok()
        .and_then(|dt| dt.format("%b %d, %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}
