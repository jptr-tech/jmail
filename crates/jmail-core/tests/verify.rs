//! Connection verification against the mock IMAP/SMTP server.

use jmail_core::{Account, Credential, Security};
use jmail_mock_imap::{MockConfig, MockMailbox, MockServer};

fn account_for(server: &MockServer) -> Account {
    let mut account = Account::generic("you@example.com");
    account.imap_host = "127.0.0.1".to_string();
    account.imap_port = server.addr().port();
    account.imap_security = Security::None;
    account.smtp_host = "127.0.0.1".to_string();
    account.smtp_port = server.smtp_addr().port();
    account.smtp_security = Security::None;
    account.username = "you@example.com".to_string();
    account
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verifies_imap_and_smtp() {
    let server = MockServer::start(
        MockConfig::new("you@example.com", "hunter2"),
        MockMailbox::demo(),
    )
    .await
    .expect("start mock server");
    let account = account_for(&server);

    jmail_core::verify::account(&account, &Credential::Password("hunter2".into()))
        .await
        .expect("both halves connect");

    // A bad password fails and the error names the protocol.
    let error = jmail_core::verify::account(&account, &Credential::Password("wrong".into()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("IMAP"), "{error}");

    // SMTP is skipped entirely when no SMTP server is configured.
    let mut imap_only = account.clone();
    imap_only.smtp_host = String::new();
    jmail_core::verify::account(&imap_only, &Credential::Password("hunter2".into()))
        .await
        .expect("imap-only verify");

    // A dead SMTP endpoint is reported as an SMTP failure.
    let mut dead_smtp = account.clone();
    dead_smtp.smtp_port = 1;
    let error = jmail_core::verify::account(&dead_smtp, &Credential::Password("hunter2".into()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SMTP"), "{error}");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_certificate_is_detected_and_can_be_accepted() {
    let server = MockServer::start(
        MockConfig::new("you@example.com", "hunter2").with_tls(),
        MockMailbox::demo(),
    )
    .await
    .expect("start mock server");

    let mut account = Account::generic("you@example.com");
    account.imap_host = "127.0.0.1".to_string();
    account.imap_port = server.addr().port();
    account.imap_security = Security::StartTls;
    account.smtp_host = String::new();
    account.username = "you@example.com".to_string();

    let credential = Credential::Password("hunter2".into());

    // The self-signed certificate is rejected under system trust.
    assert!(
        jmail_core::verify::account(&account, &credential).await.is_err(),
        "expected the untrusted certificate to fail verification"
    );

    // Accepting invalid certificates (after user confirmation) succeeds.
    let mut insecure = account.clone();
    insecure.accept_invalid_certs = true;
    jmail_core::verify::account(&insecure, &credential)
        .await
        .expect("insecure verify");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verifies_with_oauth2_bearer_after_greeting() {
    // Regression test for the Gmail stall: the server sends an unsolicited
    // greeting before the SASL exchange, and async-imap's `authenticate` used
    // to consume it as the first handshake response and then block forever.
    // Draining the greeting first makes XOAUTH2 complete.
    let server = MockServer::start(
        MockConfig::new("you@example.com", "oauth-token"),
        MockMailbox::demo(),
    )
    .await
    .expect("start mock server");

    let mut account = Account::generic("you@example.com");
    account.imap_host = "127.0.0.1".to_string();
    account.imap_port = server.addr().port();
    account.imap_security = Security::None;
    account.smtp_host = String::new();
    account.username = "you@example.com".to_string();

    let credential = Credential::Bearer {
        token: "oauth-token".to_string(),
        expires_in: None,
    };
    jmail_core::verify::account(&account, &credential)
        .await
        .expect("oauth2 bearer verify succeeds");

    // A rejected token still reports an error rather than hanging.
    let bad = Credential::Bearer {
        token: "wrong".to_string(),
        expires_in: None,
    };
    assert!(
        jmail_core::verify::account(&account, &bad).await.is_err(),
        "expected the bad bearer token to fail"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verifies_smtp_with_oauth2_bearer() {
    // Gmail SMTP authenticates with XOAUTH2, the same as its IMAP. This covers
    // the outgoing half of a Google account.
    use jmail_core::smtp::{self, SmtpSettings};

    let server = jmail_mock_imap::MockSmtpServer::start_on(0)
        .await
        .expect("start mock smtp server");
    let settings = SmtpSettings {
        host: "127.0.0.1".to_string(),
        port: server.addr().port(),
        security: Security::None,
        username: "you@gmail.com".to_string(),
        accept_invalid_certs: false,
    };
    let credential = Credential::Bearer {
        token: "oauth-token".to_string(),
        expires_in: None,
    };
    smtp::verify(&settings, &credential)
        .await
        .expect("smtp xoauth2 verify succeeds");

    server.shutdown().await;
}
