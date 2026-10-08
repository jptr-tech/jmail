//! End-to-end test of the GNOME Online Accounts client against a mock
//! `org.gnome.OnlineAccounts` service.
//!
//! Needs a real session bus, so it is `#[ignore]`d by default. Run it with:
//!
//! ```sh
//! dbus-run-session -- cargo test -p jmail-core --test goa_mock -- --ignored
//! ```

use jmail_core::{goa, Credential, Db, Purpose};
use zbus::interface;

const MANAGER_PATH: &str = "/org/gnome/OnlineAccounts";
const OAUTH_PATH: &str = "/org/gnome/OnlineAccounts/Accounts/account_1";
const PASSWORD_PATH: &str = "/org/gnome/OnlineAccounts/Accounts/account_2";

struct FakeAccount {
    provider: &'static str,
}

#[interface(name = "org.gnome.OnlineAccounts.Account")]
impl FakeAccount {
    #[zbus(property)]
    fn provider_type(&self) -> &str {
        self.provider
    }
}

struct FakeMail {
    email: &'static str,
    name: &'static str,
    imap_host: &'static str,
    smtp_host: &'static str,
}

#[interface(name = "org.gnome.OnlineAccounts.Mail")]
impl FakeMail {
    #[zbus(property)]
    fn email_address(&self) -> &str {
        self.email
    }
    #[zbus(property)]
    fn name(&self) -> &str {
        self.name
    }
    #[zbus(property)]
    fn imap_supported(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn imap_host(&self) -> &str {
        self.imap_host
    }
    #[zbus(property)]
    fn imap_use_ssl(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn imap_use_tls(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn imap_user_name(&self) -> &str {
        self.email
    }
    #[zbus(property)]
    fn smtp_supported(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn smtp_host(&self) -> &str {
        self.smtp_host
    }
    #[zbus(property)]
    fn smtp_use_ssl(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn smtp_use_tls(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn smtp_user_name(&self) -> &str {
        self.email
    }
}

struct FakeOAuth;

#[interface(name = "org.gnome.OnlineAccounts.OAuth2Based")]
impl FakeOAuth {
    async fn get_access_token(&self) -> (String, i32) {
        ("oauth-token-123".to_string(), 3600)
    }
}

struct FakePassword;

#[interface(name = "org.gnome.OnlineAccounts.PasswordBased")]
impl FakePassword {
    async fn get_password(&self, id: String) -> String {
        format!("secret-for-{id}")
    }
}

#[tokio::test]
#[ignore = "requires a session bus; run under dbus-run-session"]
async fn lists_reconciles_and_fetches_credentials() {
    let connection = zbus::Connection::session()
        .await
        .expect("session bus (run under dbus-run-session)");
    connection
        .request_name("org.gnome.OnlineAccounts")
        .await
        .expect("own the GOA name");
    let server = connection.object_server();

    server
        .at(
            OAUTH_PATH,
            FakeAccount {
                provider: "google",
            },
        )
        .await
        .unwrap();
    server
        .at(
            OAUTH_PATH,
            FakeMail {
                email: "you@gmail.com",
                name: "You Person",
                imap_host: "imap.gmail.com",
                smtp_host: "smtp.gmail.com",
            },
        )
        .await
        .unwrap();
    server.at(OAUTH_PATH, FakeOAuth).await.unwrap();

    server
        .at(
            PASSWORD_PATH,
            FakeAccount {
                provider: "imap_smtp",
            },
        )
        .await
        .unwrap();
    server
        .at(
            PASSWORD_PATH,
            FakeMail {
                email: "you@example.com",
                name: "You Example",
                imap_host: "imap.example.com:1993",
                smtp_host: "smtp.example.com",
            },
        )
        .await
        .unwrap();
    server
        .at(PASSWORD_PATH, FakePassword)
        .await
        .unwrap();

    // Register the object manager after its children.
    server
        .at(MANAGER_PATH, zbus::fdo::ObjectManager)
        .await
        .unwrap();

    let accounts = goa::list_mail_accounts().await.expect("list accounts");
    assert_eq!(accounts.len(), 2);

    let oauth = accounts
        .iter()
        .find(|account| account.account.email == "you@gmail.com")
        .expect("oauth account");
    assert_eq!(oauth.account.provider, jmail_core::Provider::Google);
    assert_eq!(oauth.account.auth, jmail_core::AuthKind::OAuth2);
    assert_eq!(oauth.account.imap_host, "imap.gmail.com");
    assert_eq!(oauth.account.imap_port, 993);
    assert_eq!(oauth.account.display_name, "You Person");

    let password = accounts
        .iter()
        .find(|account| account.account.email == "you@example.com")
        .expect("password account");
    assert_eq!(password.account.auth, jmail_core::AuthKind::Password);
    // The `host:port` override is honoured.
    assert_eq!(password.account.imap_host, "imap.example.com");
    assert_eq!(password.account.imap_port, 1993);

    match goa::credential(OAUTH_PATH, &oauth.account, Purpose::Imap)
        .await
        .expect("oauth credential")
    {
        Credential::Bearer { token, .. } => assert_eq!(token, "oauth-token-123"),
        other => panic!("expected bearer, got {other:?}"),
    }
    match goa::credential(PASSWORD_PATH, &password.account, Purpose::Smtp)
        .await
        .expect("password credential")
    {
        Credential::Password(pw) => assert_eq!(pw, "secret-for-smtp-password"),
        other => panic!("expected password, got {other:?}"),
    }

    // Reconciliation imports both and keeps them enabled by default.
    let db = Db::open_in_memory().unwrap();
    let imported = goa::reconcile(&db).await.expect("reconcile");
    assert_eq!(imported, 2);
    let rows = db.accounts().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|account| account.enabled));
    assert!(rows
        .iter()
        .all(|account| account.goa_object_path.is_some()));
}
