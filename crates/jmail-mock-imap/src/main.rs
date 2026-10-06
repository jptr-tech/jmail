//! Demo server: serves the canned mailbox on port 1143 (optionally with TLS
//! and a delayed push to exercise IDLE).

use std::time::Duration;

use jmail_mock_imap::{MockConfig, MockMailbox, MockMessage, MockServer};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let with_tls = std::env::args().any(|arg| arg == "--tls");
    let port = std::env::args()
        .position(|arg| arg == "--port")
        .and_then(|index| std::env::args().nth(index + 1))
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(1143);
    let push_after = std::env::args()
        .position(|arg| arg == "--push-after")
        .and_then(|index| std::env::args().nth(index + 1))
        .and_then(|value| value.parse::<u64>().ok());

    let config = {
        let config = MockConfig::new("you@example.com", "hunter2");
        if with_tls {
            config.with_tls()
        } else {
            config
        }
    };

    let server = MockServer::start_on(port, config, MockMailbox::demo())
        .await
        .expect("start mock server");
    println!("Mock IMAP listening on 127.0.0.1:{} (user you@example.com / hunter2)", server.addr().port());
    if let Some(cert) = server.cert_der() {
        println!("TLS enabled; cert sha256 present ({} bytes)", cert.len());
    } else {
        println!("TLS disabled (STARTTLS unavailable)");
    }

    if let Some(seconds) = push_after {
        let pusher = server.pusher();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let message = MockMessage::new(
                "Live push while you idled",
                "Ada Lovelace",
                "ada@example.org",
                &[("You", "you@example.com")],
                &format!("push-{now}@example.org"),
                "Mon, 05 Oct 2026 12:00:00 +0000",
                "This message arrived while the client was idling.",
                &[],
            );
            let uid = pusher.push("INBOX", message).await;
            println!("Pushed new message into INBOX (uid {uid})");
        });
    }

    println!("Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await.ok();
}
