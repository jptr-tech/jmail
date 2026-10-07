//! A small IMAP server used by Jmail's tests and for offline development.

pub mod mailbox;
pub mod server;
pub mod smtp;

pub use mailbox::{MockFolder, MockMailbox, MockMessage};
pub use server::{MockConfig, MockPusher, MockServer};
pub use smtp::MockSmtpServer;
