//! A small IMAP server used by Jmail's tests and for offline development.

pub mod mailbox;
pub mod server;

pub use mailbox::{MockFolder, MockMailbox, MockMessage};
pub use server::{MockConfig, MockPusher, MockServer};
