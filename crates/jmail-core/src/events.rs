//! Events emitted by the sync engine and consumed by the UI.
//!
//! The UI runs on the GLib main loop while the engine runs on Tokio; these
//! messages are the only channel between them.

use crate::account::AccountId;
use crate::folder::FolderId;
use crate::message::MessageId;

/// Something that happened in the background that the UI may want to reflect.
#[derive(Debug, Clone)]
pub enum Event {
    /// A full account sync cycle started.
    SyncStarted { account: AccountId },
    /// A full account sync cycle finished.
    SyncFinished { account: AccountId },
    /// The folder list for an account changed.
    FoldersChanged { account: AccountId },
    /// A folder's message list changed (new mail, flags, deletions).
    MessagesChanged { account: AccountId, folder: FolderId },
    /// A new message arrived in the Inbox and should be considered for a
    /// desktop notification.
    NewInboxMessage {
        account: AccountId,
        folder: FolderId,
        message: MessageId,
    },
    /// A recoverable error the UI should surface.
    Error { account: AccountId, message: String },
}
