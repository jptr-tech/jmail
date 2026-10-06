//! Credential storage: the system keyring with a dev-only plaintext fallback.
//!
//! Passwords live in the Secret Service via the `keyring` crate. Setting
//! `JMAIL_DEV_PLAINTEXT=1` (e.g. when no keyring daemon is available in a
//! sandbox) stores them in the local cache database instead, loudly.

use crate::account::AccountId;
use crate::cache::Db;
use crate::error::{Error, Result};

const SERVICE: &str = "tech.jptr.jmail";
const DEV_PLAINTEXT_ENV: &str = "JMAIL_DEV_PLAINTEXT";

fn key(account: AccountId) -> String {
    format!("imap:{}", account.0)
}

fn dev_fallback() -> bool {
    std::env::var(DEV_PLAINTEXT_ENV)
        .map(|value| value == "1")
        .unwrap_or(false)
}

fn entry(account: AccountId) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &key(account)).map_err(|error| {
        Error::Config(format!("keyring unavailable: {error} (set {DEV_PLAINTEXT_ENV}=1 to store passwords in the local database instead)"))
    })
}

/// Store the IMAP password for an account.
pub fn store_password(db: &Db, account: AccountId, password: &str) -> Result<()> {
    if dev_fallback() {
        tracing::warn!(
            "STORING PASSWORD IN PLAINTEXT (dev fallback via {DEV_PLAINTEXT_ENV}=1)"
        );
        return db.set_secret(&key(account), password);
    }
    entry(account)?.set_password(password).map_err(|error| {
        Error::Config(format!("could not store password in keyring: {error}"))
    })
}

/// Load the IMAP password for an account, if stored.
pub fn load_password(db: &Db, account: AccountId) -> Result<Option<String>> {
    if dev_fallback() {
        return db.get_secret(&key(account));
    }
    match entry(account)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(Error::Config(format!(
            "could not read password from keyring: {error}"
        ))),
    }
}

/// Delete the stored password for an account.
pub fn delete_password(db: &Db, account: AccountId) -> Result<()> {
    if dev_fallback() {
        return db.delete_secret(&key(account));
    }
    match entry(account)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(Error::Config(format!(
            "could not delete password from keyring: {error}"
        ))),
    }
}
