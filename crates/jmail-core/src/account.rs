/// Stable identifier for an account, matching the primary key in the cache DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub i64);

/// How an account authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// Username + password (generic IMAP/SMTP).
    Password,
    /// OAuth2 (currently Google) using SASL XOAUTH2.
    OAuth2,
}

impl AuthKind {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::Password => 0,
            Self::OAuth2 => 1,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::OAuth2,
            _ => Self::Password,
        }
    }
}

/// Transport security for an IMAP/SMTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Implicit TLS (IMAPS/SMTPS), the usual choice on port 993/465.
    Tls,
    /// Plain connection upgraded with STARTTLS.
    StartTls,
    /// No encryption. Only for the local mock server / development.
    None,
}

impl Security {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::Tls => 0,
            Self::StartTls => 1,
            Self::None => 2,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::StartTls,
            2 => Self::None,
            _ => Self::Tls,
        }
    }
}

/// Well-known provider profiles that tweak folder/flag behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    Generic,
}

impl Provider {
    pub fn to_i64(self) -> i64 {
        match self {
            Self::Google => 0,
            Self::Generic => 1,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Google,
            _ => Self::Generic,
        }
    }
}

/// A configured mail account.
#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub email: String,
    pub display_name: String,
    pub provider: Provider,
    pub auth: AuthKind,
    pub username: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    /// Accent colour (hex, e.g. `#3584e4`) used for the account chip in the UI.
    pub color: String,
    pub enabled: bool,
}

impl Account {
    /// Sensible defaults for a Google account.
    pub fn google(email: impl Into<String>) -> Self {
        let email = email.into();
        Self {
            id: AccountId(0),
            name: "Google".to_string(),
            display_name: String::new(),
            email: email.clone(),
            provider: Provider::Google,
            auth: AuthKind::OAuth2,
            username: email,
            imap_host: "imap.gmail.com".to_string(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.gmail.com".to_string(),
            smtp_port: 465,
            smtp_security: Security::Tls,
            color: "#3584e4".to_string(),
            enabled: true,
        }
    }

    /// Sensible defaults for a generic IMAP/SMTP account.
    pub fn generic(email: impl Into<String>) -> Self {
        let email = email.into();
        Self {
            id: AccountId(0),
            name: email.clone(),
            display_name: String::new(),
            email: email.clone(),
            provider: Provider::Generic,
            auth: AuthKind::Password,
            username: email,
            imap_host: String::new(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: String::new(),
            smtp_port: 465,
            smtp_security: Security::Tls,
            color: "#e5a50a".to_string(),
            enabled: true,
        }
    }
}
