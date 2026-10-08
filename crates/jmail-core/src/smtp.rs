//! SMTP sending via [`lettre`]'s async transport.

use std::fmt;
use std::time::Duration;

use lettre::message::Message;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::account::Security;
use crate::credentials::Credential;
use crate::error::{Error, Result};

/// Connection settings for an account's outgoing server.
#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    /// Accept invalid TLS certificates (user-confirmed, per account).
    pub accept_invalid_certs: bool,
}

/// Build the async transport for an account.
fn transport(
    settings: &SmtpSettings,
    credential: &Credential,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let tls = match settings.security {
        Security::Tls => Tls::Wrapper(tls_parameters(&settings.host, settings.accept_invalid_certs)?),
        Security::StartTls => {
            Tls::Required(tls_parameters(&settings.host, settings.accept_invalid_certs)?)
        }
        Security::None => Tls::None,
    };

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(settings.host.clone())
        .port(settings.port)
        .tls(tls)
        .timeout(Some(Duration::from_secs(30)));
    if !settings.username.trim().is_empty() {
        match credential {
            Credential::Password(password) => {
                builder = builder.credentials(Credentials::new(
                    settings.username.clone(),
                    password.clone(),
                ));
            }
            Credential::Bearer { token, .. } => {
                builder = builder
                    .authentication(vec![Mechanism::Xoauth2])
                    .credentials(Credentials::new(settings.username.clone(), token.clone()));
            }
        }
    }
    Ok(builder.build())
}

fn tls_parameters(host: &str, accept_invalid_certs: bool) -> Result<TlsParameters> {
    let mut builder = TlsParameters::builder(host.to_string());
    if accept_invalid_certs {
        builder = builder
            .dangerous_accept_invalid_certs(true)
            .dangerous_accept_invalid_hostnames(true);
    }
    builder.build().map_err(smtp_error)
}

/// Send a built message. The credential is either a password or an OAuth2
/// bearer token; the latter is sent with the `XOAUTH2` SASL mechanism.
pub async fn send(settings: &SmtpSettings, credential: &Credential, message: Message) -> Result<()> {
    if settings.host.trim().is_empty() {
        return Err(Error::Config(
            "no SMTP server configured for this account".to_string(),
        ));
    }
    transport(settings, credential)?
        .send(message)
        .await
        .map(|_| ())
        .map_err(|error| Error::Send(format!("{}:{}: {error}", settings.host, settings.port)))
}

/// Connect and authenticate without sending, to validate the settings.
pub async fn verify(settings: &SmtpSettings, credential: &Credential) -> Result<()> {
    if settings.host.trim().is_empty() {
        return Err(Error::Config(
            "no SMTP server configured for this account".to_string(),
        ));
    }
    let connected = transport(settings, credential)?
        .test_connection()
        .await
        .map_err(|error| Error::Send(format!("{}:{}: {error}", settings.host, settings.port)))?;
    if connected {
        Ok(())
    } else {
        Err(Error::Send(format!(
            "{}:{}: server did not accept the connection",
            settings.host, settings.port
        )))
    }
}

fn smtp_error(error: impl fmt::Display) -> Error {
    Error::Send(error.to_string())
}
