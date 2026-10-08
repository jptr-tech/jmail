//! Credential resolution: turn an account into a password or OAuth2 bearer.
//!
//! Manually configured accounts read their password from the system keyring.
//! Accounts sourced from GNOME Online Accounts fetch their token or password
//! from goa-daemon on demand, so an expired OAuth2 token is refreshed simply by
//! resolving again on the next (re)connect.

use crate::account::{Account, AuthKind};
use crate::cache::Db;
use crate::error::{Error, Result};
use crate::goa;
use crate::secrets;

/// A resolved secret for authenticating one connection.
#[derive(Debug, Clone)]
pub enum Credential {
    /// Username + password (IMAP `LOGIN`, SASL `PLAIN`/`LOGIN`).
    Password(String),
    /// OAuth2 access token, used with SASL `XOAUTH2`.
    Bearer {
        token: String,
        /// Remaining validity in seconds, when the issuer reports it.
        expires_in: Option<i64>,
    },
}

impl From<&str> for Credential {
    fn from(value: &str) -> Self {
        Self::Password(value.to_string())
    }
}

impl From<String> for Credential {
    fn from(value: String) -> Self {
        Self::Password(value)
    }
}

impl From<&String> for Credential {
    fn from(value: &String) -> Self {
        Self::Password(value.clone())
    }
}

/// Which service a credential is being resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Imap,
    Smtp,
}

impl Purpose {
    /// The GOA password identifier for this service (for `PasswordBased`).
    pub(crate) fn goa_password_id(self) -> &'static str {
        match self {
            Self::Imap => "imap-password",
            Self::Smtp => "smtp-password",
        }
    }
}

/// Resolve the credential for an account and service.
pub async fn for_account(db: &Db, account: &Account, purpose: Purpose) -> Result<Credential> {
    if let Some(object_path) = &account.goa_object_path {
        return goa::credential(object_path, account, purpose).await;
    }
    match account.auth {
        AuthKind::Password => {
            let password =
                secrets::load_password(db, account.id)?.ok_or(Error::Authentication(account.id.0))?;
            Ok(Credential::Password(password))
        }
        AuthKind::OAuth2 => Err(Error::Config(format!(
            "{} needs to sign in through Online Accounts; re-add it from GNOME Settings",
            account.email
        ))),
    }
}
