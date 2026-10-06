//! Core backend for Jmail: accounts, mail sync, storage, search and sending.
//!
//! This crate is intentionally free of any GTK/GUI dependency so that the
//! engine can be tested and reused independently of the frontend.

pub mod account;
pub mod cache;
pub mod error;
pub mod events;
pub mod folder;
pub mod imap;
pub mod message;
pub mod secrets;
pub mod sync;
pub mod threading;

pub use account::{Account, AccountId, AuthKind, Provider, Security};
pub use cache::Db;
pub use error::{Error, Result};
pub use events::Event;
pub use folder::{Folder, FolderId, SpecialUse};
pub use message::{Address, Flags, Message, MessageBody, MessageId, NewMessage};
pub use sync::{SyncCommand, SyncEngine};
