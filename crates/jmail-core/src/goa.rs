//! Client for GNOME Online Accounts (goa-daemon).
//!
//! GOA is a session D-Bus service (`org.gnome.OnlineAccounts`) that owns the
//! OAuth2 grants for Google/Microsoft and can also store plain IMAP/SMTP
//! passwords. We enumerate mail-capable accounts through the standard
//! ObjectManager interface, read their server configuration from the
//! `Mail` interface, and fetch credentials (`OAuth2Based.GetAccessToken` or
//! `PasswordBased.GetPassword`) on demand.
//!
//! Accounts are reconciled into the local cache once per launch; a user can
//! disable an imported account, but the system account itself is never
//! modified.

use std::collections::HashSet;

use zbus::fdo::ManagedObjects;

use crate::account::{Account, AccountId, AuthKind, Provider, Security};
use crate::cache::Db;
use crate::credentials::{Credential, Purpose};
use crate::error::{Error, Result};

const DEST: &str = "org.gnome.OnlineAccounts";
const PATH: &str = "/org/gnome/OnlineAccounts";
const IFACE_ACCOUNT: &str = "org.gnome.OnlineAccounts.Account";
const IFACE_MAIL: &str = "org.gnome.OnlineAccounts.Mail";
const IFACE_OAUTH2: &str = "org.gnome.OnlineAccounts.OAuth2Based";

/// A mail account advertised by GOA, translated into our own [`Account`] type.
#[derive(Debug, Clone)]
pub struct GoaMailAccount {
    /// D-Bus object path of the account, used as the local reconciliation key.
    pub object_path: String,
    pub account: Account,
}

/// Enumerate GOA accounts that can send or receive mail.
///
/// Returns an error if the GOA service is not reachable (e.g. it is not
/// installed, or this is not a GNOME session).
pub async fn list_mail_accounts() -> Result<Vec<GoaMailAccount>> {
    let connection = zbus::Connection::session().await.map_err(goa_error)?;
    let manager = zbus::Proxy::new(
        &connection,
        DEST,
        PATH,
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    .map_err(goa_error)?;
    let objects: ManagedObjects = manager
        .call("GetManagedObjects", &())
        .await
        .map_err(goa_error)?;

    let mut accounts = Vec::new();
    for (path, interfaces) in &objects {
        if !interfaces.contains_key(IFACE_MAIL) {
            continue;
        }
        let object_path = path.as_str().to_string();
        match account_from(&connection, &object_path, interfaces.contains_key(IFACE_OAUTH2)).await
        {
            Ok(Some(account)) => accounts.push(GoaMailAccount {
                object_path,
                account,
            }),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, path = %object_path, "skipping GOA account");
            }
        }
    }
    Ok(accounts)
}

/// Fetch a credential for a GOA-backed account.
pub async fn credential(
    object_path: &str,
    account: &Account,
    purpose: Purpose,
) -> Result<Credential> {
    let connection = zbus::Connection::session().await.map_err(goa_error)?;
    match account.auth {
        AuthKind::OAuth2 => {
            let proxy = zbus::Proxy::new(&connection, DEST, object_path, IFACE_OAUTH2)
                .await
                .map_err(goa_error)?;
            let (token, expires_in): (String, i32) = proxy
                .call("GetAccessToken", &())
                .await
                .map_err(goa_error)?;
            Ok(Credential::Bearer {
                token,
                expires_in: Some(expires_in as i64),
            })
        }
        AuthKind::Password => {
            let proxy = zbus::Proxy::new(
                &connection,
                DEST,
                object_path,
                "org.gnome.OnlineAccounts.PasswordBased",
            )
            .await
            .map_err(goa_error)?;
            let id = purpose.goa_password_id();
            let password: String = proxy
                .call("GetPassword", &(id,))
                .await
                .map_err(goa_error)?;
            Ok(Credential::Password(password))
        }
    }
}

/// Reconcile the local account rows with the system's GOA accounts.
///
/// Newly discovered accounts are inserted enabled; existing rows have their
/// server configuration refreshed while keeping the user's enable/disable
/// choice. Accounts whose GOA object has disappeared are removed locally (only
/// when GOA itself is reachable, so a missing daemon never wipes cached mail).
///
/// Returns the number of newly imported accounts.
pub async fn reconcile(db: &Db) -> Result<usize> {
    let remote = match list_mail_accounts().await {
        Ok(accounts) => accounts,
        Err(error) => {
            tracing::debug!(%error, "GNOME Online Accounts not available");
            return Ok(0);
        }
    };

    let existing = db.accounts()?;
    let mut imported = 0usize;
    for remote_account in &remote {
        match db.account_by_goa_path(&remote_account.object_path)? {
            Some(local) => {
                let mut updated = remote_account.account.clone();
                updated.id = local.id;
                db.update_account_config(&updated)?;
            }
            None => {
                db.insert_account(&remote_account.account)?;
                imported += 1;
            }
        }
    }

    let live: HashSet<&str> = remote
        .iter()
        .map(|account| account.object_path.as_str())
        .collect();
    for local in &existing {
        if let Some(path) = &local.goa_object_path {
            if !live.contains(path.as_str()) {
                db.remove_account(local.id)?;
            }
        }
    }

    if imported > 0 {
        tracing::info!(imported, "imported accounts from GNOME Online Accounts");
    }
    Ok(imported)
}

/// Build an [`Account`] from a GOA mail object, or `None` if it has no usable
/// address.
async fn account_from(
    connection: &zbus::Connection,
    object_path: &str,
    oauth2: bool,
) -> Result<Option<Account>> {
    let mail = zbus::Proxy::new(connection, DEST, object_path, IFACE_MAIL)
        .await
        .map_err(goa_error)?;

    let email: String = mail
        .get_property("EmailAddress")
        .await
        .map_err(goa_error)?;
    if email.trim().is_empty() {
        return Ok(None);
    }

    let imap_supported: bool = mail.get_property("ImapSupported").await.unwrap_or(false);
    let smtp_supported: bool = mail.get_property("SmtpSupported").await.unwrap_or(false);
    if !imap_supported && !smtp_supported {
        return Ok(None);
    }

    let name: String = mail.get_property("Name").await.unwrap_or_default();
    let imap_host: String = mail.get_property("ImapHost").await.unwrap_or_default();
    let imap_ssl: bool = mail.get_property("ImapUseSsl").await.unwrap_or(false);
    let imap_tls: bool = mail.get_property("ImapUseTls").await.unwrap_or(false);
    let imap_user: String = mail.get_property("ImapUserName").await.unwrap_or_default();
    let smtp_host: String = mail.get_property("SmtpHost").await.unwrap_or_default();
    let smtp_ssl: bool = mail.get_property("SmtpUseSsl").await.unwrap_or(false);
    let smtp_tls: bool = mail.get_property("SmtpUseTls").await.unwrap_or(false);
    let smtp_user: String = mail.get_property("SmtpUserName").await.unwrap_or_default();

    let account_iface = zbus::Proxy::new(connection, DEST, object_path, IFACE_ACCOUNT)
        .await
        .map_err(goa_error)?;
    let provider_type: String = account_iface
        .get_property("ProviderType")
        .await
        .unwrap_or_default();

    let provider = if provider_type.eq_ignore_ascii_case("google") {
        Provider::Google
    } else {
        Provider::Generic
    };
    let auth = if oauth2 {
        AuthKind::OAuth2
    } else {
        AuthKind::Password
    };

    let username = if !imap_user.trim().is_empty() {
        imap_user
    } else if !smtp_user.trim().is_empty() {
        smtp_user
    } else {
        email.clone()
    };

    let (imap_host, imap_port) = split_host(&imap_host, if imap_ssl { 993 } else { 143 });
    let (smtp_host, smtp_port) = split_host(&smtp_host, if smtp_ssl { 465 } else { 587 });

    Ok(Some(Account {
        id: AccountId(0),
        name: email.clone(),
        email: email.clone(),
        display_name: name,
        provider,
        auth,
        username,
        imap_host,
        imap_port,
        imap_security: security_of(imap_ssl, imap_tls),
        smtp_host,
        smtp_port,
        smtp_security: security_of(smtp_ssl, smtp_tls),
        color: color_for(provider),
        enabled: true,
        goa_object_path: Some(object_path.to_string()),
        accept_invalid_certs: false,
    }))
}

/// Split a GOA host, which may carry a `host:port` override (or a bracketed
/// IPv6 address), falling back to `default_port`.
fn split_host(value: &str, default_port: u16) -> (String, u16) {
    let value = value.trim();
    if value.is_empty() {
        return (String::new(), default_port);
    }
    if let Some(rest) = value.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &value[..=end + 1];
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|port| port.parse::<u16>().ok())
                .unwrap_or(default_port);
            return (host.to_string(), port);
        }
    }
    if let Some((host, port)) = value.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') {
            if let Ok(port) = port.parse::<u16>() {
                return (host.to_string(), port);
            }
        }
    }
    (value.to_string(), default_port)
}

fn security_of(use_ssl: bool, use_tls: bool) -> Security {
    if use_ssl {
        Security::Tls
    } else if use_tls {
        Security::StartTls
    } else {
        Security::None
    }
}

fn color_for(provider: Provider) -> String {
    match provider {
        Provider::Google => "#3584e4".to_string(),
        Provider::Generic => "#e5a50a".to_string(),
    }
}

fn goa_error(error: zbus::Error) -> Error {
    Error::Network(format!("GNOME Online Accounts: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_optional_port() {
        assert_eq!(
            split_host("imap.gmail.com", 993),
            ("imap.gmail.com".to_string(), 993)
        );
        assert_eq!(
            split_host("imap.example.com:1993", 993),
            ("imap.example.com".to_string(), 1993)
        );
        assert_eq!(split_host("[::1]:143", 993), ("[::1]".to_string(), 143));
        assert_eq!(split_host("", 465), (String::new(), 465));
    }

    #[test]
    fn maps_security_flags() {
        assert_eq!(security_of(true, false), Security::Tls);
        assert_eq!(security_of(false, true), Security::StartTls);
        assert_eq!(security_of(false, false), Security::None);
    }

    #[tokio::test]
    async fn reconcile_never_errors_without_goa() {
        let db = crate::cache::Db::open_in_memory().unwrap();
        // With no GOA service this returns Ok(0); where GOA exists it may import
        // accounts, but reconciliation must never fail the launch.
        assert!(reconcile(&db).await.is_ok());
    }
}
