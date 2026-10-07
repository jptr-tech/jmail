//! SMTP sending via [`lettre`]'s async transport.

use std::fmt;
use std::time::Duration;

use lettre::message::Message;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::account::Security;
use crate::error::{Error, Result};

/// Connection settings for an account's outgoing server.
#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
}

/// Send a built message. `password` is the account secret (password now, an
/// OAuth2 token once M5 lands).
pub async fn send(settings: &SmtpSettings, password: &str, message: Message) -> Result<()> {
    if settings.host.trim().is_empty() {
        return Err(Error::Config(
            "no SMTP server configured for this account".to_string(),
        ));
    }

    let tls = match settings.security {
        Security::Tls => {
            Tls::Wrapper(TlsParameters::new(settings.host.clone()).map_err(smtp_error)?)
        }
        Security::StartTls => {
            Tls::Required(TlsParameters::new(settings.host.clone()).map_err(smtp_error)?)
        }
        Security::None => Tls::None,
    };

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(settings.host.clone())
        .port(settings.port)
        .tls(tls)
        .timeout(Some(Duration::from_secs(30)));
    if !settings.username.trim().is_empty() {
        builder = builder.credentials(Credentials::new(
            settings.username.clone(),
            password.to_string(),
        ));
    }

    builder
        .build()
        .send(message)
        .await
        .map(|_| ())
        .map_err(|error| Error::Send(format!("{}:{}: {error}", settings.host, settings.port)))
}

fn smtp_error(error: impl fmt::Display) -> Error {
    Error::Send(error.to_string())
}
