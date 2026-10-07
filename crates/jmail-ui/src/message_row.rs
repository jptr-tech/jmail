use gtk::glib;
use jmail_core::ThreadSummary;

/// A single row in the message list — one per conversation. `id` is the
/// representative (latest) message in the thread. Plain Rust data wrapped in a
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
    /// Any message in the conversation is unread.
    pub unread: bool,
    /// Any message in the conversation is starred.
    pub starred: bool,
    pub message_count: u32,
    pub unread_count: u32,
    pub starred_count: u32,
}

impl MessageRow {
    pub fn from_thread(summary: &ThreadSummary) -> Self {
        let message = &summary.message;
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
            unread: summary.unread > 0,
            starred: summary.starred > 0,
            message_count: summary.count,
            unread_count: summary.unread,
            starred_count: summary.starred,
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
