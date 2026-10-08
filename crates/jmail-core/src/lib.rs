//! Core backend for Jmail: accounts, mail sync, storage, search and sending.
//!
//! This crate is intentionally free of any GTK/GUI dependency so that the
//! engine can be tested and reused independently of the frontend.

pub mod account;
pub mod cache;
pub mod credentials;
pub mod error;
pub mod events;
pub mod folder;
pub mod goa;
pub mod imap;
pub mod markdown;
pub mod message;
pub mod outgoing;
pub mod secrets;
pub mod smtp;
pub mod sync;
pub mod threading;
pub mod verify;

pub use account::{Account, AccountId, AuthKind, Provider, Security};
pub use cache::{Db, ThreadFilter, ThreadSummary};
pub use credentials::{Credential, Purpose};
pub use error::{Error, Result};
pub use events::Event;
pub use folder::{Folder, FolderId, SpecialUse};
pub use markdown::{StyleKind, StyleRange};
pub use message::{Address, Attachment, Flags, Message, MessageBody, MessageId, NewMessage};
pub use outgoing::{
    parse_address, parse_address_list, prefixed_subject, reply_address, reply_recipients,
    sanitize_header, select_reply_target, DraftRef, OutgoingAttachment, OutgoingMessage,
};
pub use smtp::SmtpSettings;
pub use sync::{SyncCommand, SyncEngine};
