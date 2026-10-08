//! Verify that an account's servers are reachable and the credential is valid.
//!
//! Used by the account editor: both the explicit "Test" button and the save
//! path run this before persisting anything.

use std::time::Duration;

use crate::account::Account;
use crate::credentials::Credential;
use crate::error::{Error, Result};
use crate::imap::{ImapSession, ImapSettings, TlsTrust};
use crate::smtp::{self, SmtpSettings};

/// How long a single protocol check may take.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Check every configured server for `account`.
///
/// IMAP is required and always checked; SMTP is checked only when an SMTP
/// server is configured. The first failure is returned, labelled with the
/// protocol and endpoint so the UI can point at the offending half.
pub async fn account(account: &Account, credential: &Credential) -> Result<()> {
    if account.imap_host.trim().is_empty() {
        return Err(Error::Config("no IMAP server configured".to_string()));
    }
    verify_imap(account, credential).await?;
    if !account.smtp_host.trim().is_empty() {
        verify_smtp(account, credential).await?;
    }
    Ok(())
}

async fn verify_imap(account: &Account, credential: &Credential) -> Result<()> {
    let settings = ImapSettings {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.imap_security,
        username: account.username.clone(),
    };
    let trust = TlsTrust::for_account(account)?;
    match tokio::time::timeout(
        TIMEOUT,
        ImapSession::connect(settings, credential.clone(), &trust),
    )
    .await
    {
        Ok(Ok(_session)) => Ok(()),
        Ok(Err(error)) => Err(label("IMAP", &account.imap_host, account.imap_port, error)),
        Err(_) => Err(Error::Network(format!(
            "IMAP {}:{}: timed out",
            account.imap_host, account.imap_port
        ))),
    }
}

async fn verify_smtp(account: &Account, credential: &Credential) -> Result<()> {
    let settings = SmtpSettings {
        host: account.smtp_host.clone(),
        port: account.smtp_port,
        security: account.smtp_security,
        username: account.username.clone(),
        accept_invalid_certs: account.accept_invalid_certs,
    };
    match tokio::time::timeout(TIMEOUT, smtp::verify(&settings, credential)).await {
        Ok(result) => result.map_err(|error| {
            label("SMTP", &account.smtp_host, account.smtp_port, error)
        }),
        Err(_) => Err(Error::Network(format!(
            "SMTP {}:{}: timed out",
            account.smtp_host, account.smtp_port
        ))),
    }
}

fn label(protocol: &str, host: &str, port: u16, error: Error) -> Error {
    Error::Network(format!("{protocol} {host}:{port}: {error}"))
}
