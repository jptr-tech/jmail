//! The receive-side backend abstraction.
//!
//! IMAP is the only implementation today, but keeping this trait lets us add
//! alternative transports (e.g. the Gmail API) later without touching the sync
//! engine or the UI.

use async_trait::async_trait;

use crate::error::Result;
use crate::folder::{Folder, FolderId};
use crate::message::Message;

/// A connected mail backend for a single account.
#[async_trait]
pub trait MailBackend: Send + Sync {
    /// Establish (or re-establish) the connection and authenticate.
    async fn connect(&mut self) -> Result<()>;

    /// List all selectable folders, including SPECIAL-USE classification.
    async fn list_folders(&mut self) -> Result<Vec<Folder>>;

    /// Fetch message metadata for a folder since `since_uid` (exclusive).
    async fn fetch_messages(&mut self, folder: FolderId, since_uid: u32) -> Result<Vec<Message>>;

    /// Wait for server-side changes on `folder` (IMAP IDLE).
    async fn idle(&mut self, folder: FolderId) -> Result<()>;

    /// Disconnect cleanly.
    async fn disconnect(&mut self) -> Result<()>;
}
