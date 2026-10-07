//! End-to-end tests against the in-process mock IMAP server.

use std::time::Duration;

use jmail_core::imap::{ImapSession, ImapSettings, TlsTrust};
use jmail_core::message::Flags;
use jmail_core::{Account, Db, Security, SpecialUse, SyncCommand, SyncEngine};
use jmail_mock_imap::{MockConfig, MockMailbox, MockMessage, MockServer};

fn settings(port: u16, security: Security) -> ImapSettings {
    ImapSettings {
        host: "127.0.0.1".to_string(),
        port,
        security,
        username: "you@example.com".to_string(),
    }
}

async fn start_server(with_tls: bool) -> MockServer {
    let config = if with_tls {
        MockConfig::new("you@example.com", "hunter2").with_tls()
    } else {
        MockConfig::new("you@example.com", "hunter2")
    };
    MockServer::start(config, MockMailbox::demo())
        .await
        .expect("start mock server")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_over_starttls() {
    let server = start_server(true).await;
    let cert = server.cert_der().expect("cert").to_vec();
    let trust = TlsTrust::with_pinned(&cert).expect("trust");

    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::StartTls),
        "hunter2",
        &trust,
    )
    .await
    .expect("connect over STARTTLS");

    let folders = session.list_folders().await.expect("list");
    assert!(folders.iter().any(|folder| folder.special_use == SpecialUse::Inbox));
    assert!(folders.iter().any(|folder| folder.special_use == SpecialUse::Sent));

    let inbox = folders
        .iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .expect("inbox");
    let messages = session.fetch_headers(&inbox.path, 0).await.expect("fetch");
    assert_eq!(messages.len(), 5);
    assert!(messages.iter().any(|message| message.new.subject.contains("Welcome")));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syncs_headers_and_flags() {
    let server = start_server(true).await;
    let cert = server.cert_der().unwrap().to_vec();
    let trust = TlsTrust::with_pinned(&cert).unwrap();

    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::StartTls),
        "hunter2",
        &trust,
    )
    .await
    .unwrap();
    let folders = session.list_folders().await.unwrap();
    let sent = folders
        .iter()
        .find(|folder| folder.special_use == SpecialUse::Sent)
        .unwrap();

    let headers = session.fetch_headers(&sent.path, 0).await.unwrap();
    assert_eq!(headers.len(), 1);
    assert!(headers[0].flags.seen);

    // Flag it and observe the server state change.
    session
        .set_flags(&sent.path, headers[0].uid, Flags { flagged: true, ..Flags::default() })
        .await
        .unwrap();
    let refetched = session.fetch_headers(&sent.path, 0).await.unwrap();
    assert!(refetched[0].flags.flagged);

    // Full body fetch parses out of the raw message.
    let body = session
        .fetch_full(&sent.path, headers[0].uid)
        .await
        .unwrap()
        .expect("body");
    assert!(String::from_utf8_lossy(&body).contains("It landed"));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parses_html_alternative_body() {
    let server = start_server(false).await;

    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::None),
        "hunter2",
        &TlsTrust::system().unwrap(),
    )
    .await
    .unwrap();
    let folders = session.list_folders().await.unwrap();
    let inbox = folders
        .iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let headers = session.fetch_headers(&inbox.path, 0).await.unwrap();
    let welcome = headers
        .iter()
        .find(|message| message.new.subject == "Welcome to Jmail")
        .unwrap();

    let raw = session
        .fetch_full(&inbox.path, welcome.uid)
        .await
        .unwrap()
        .expect("body");
    let parsed = mail_parser::MessageParser::default()
        .parse(&raw)
        .expect("parse");
    assert!(parsed
        .body_html(0)
        .is_some_and(|html| html.contains("Welcome to Jmail")));
    assert!(parsed
        .body_text(0)
        .is_some_and(|text| text.contains("first message")));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_syncs_into_cache_and_applies_moves() {
    let server = start_server(false).await;
    let db = Db::open_in_memory().unwrap();
    let account = db
        .insert_account(&{
            let mut account = Account::generic("you@example.com");
            account.imap_host = "127.0.0.1".to_string();
            account.imap_port = server.addr().port();
            account.imap_security = Security::None;
            account.username = "you@example.com".to_string();
            account
        })
        .unwrap();
    jmail_core::secrets::store_password(&db, account, "hunter2").unwrap();

    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();

    // Wait for the initial sync to finish.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::events::Event::SyncFinished { .. }) {
            break;
        }
    }

    let inbox = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let messages = db.messages_in_folder(inbox.id, 100).unwrap();
    assert_eq!(messages.len(), 5);

    // Replies reconstruct their conversation from References/In-Reply-To: the
    // demo pairs m1<-m2 and m3<-m4.
    let welcome = messages
        .iter()
        .find(|m| m.subject == "Welcome to Jmail")
        .unwrap();
    let welcome_reply = messages
        .iter()
        .find(|m| m.subject == "Re: Welcome to Jmail")
        .unwrap();
    assert!(!welcome.thread_id.is_empty());
    assert_eq!(welcome.thread_id, welcome_reply.thread_id);
    let report = messages
        .iter()
        .find(|m| m.subject == "Quarterly report")
        .unwrap();
    assert_ne!(welcome.thread_id, report.thread_id);
    // The Sent reply belongs to the same conversation (across folders).
    let sent = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|f| f.special_use == SpecialUse::Sent)
        .unwrap();
    let sent_message = db.messages_in_folder(sent.id, 10).unwrap().remove(0);
    assert_eq!(sent_message.thread_id, welcome.thread_id);

    // Move one to Trash via the engine; the cache reflects it.
    let message = messages[0].id;
    engine
        .send(account, SyncCommand::Move { message, to: SpecialUse::Trash });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if let jmail_core::events::Event::MessagesChanged { folder, .. } = event {
            if folder == inbox.id && db.messages_in_folder(inbox.id, 100).unwrap().len() == 4 {
                break;
            }
        }
    }
    let remaining = db.messages_in_folder(inbox.id, 100).unwrap();
    assert_eq!(remaining.len(), 4);
    let trash = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Trash)
        .unwrap();
    assert_eq!(db.messages_in_folder(trash.id, 100).unwrap().len(), 1);

    engine.stop_account(account);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_applies_thread_wide_flags() {
    let server = start_server(false).await;
    let db = Db::open_in_memory().unwrap();
    let account = db
        .insert_account(&{
            let mut account = Account::generic("you@example.com");
            account.imap_host = "127.0.0.1".to_string();
            account.imap_port = server.addr().port();
            account.imap_security = Security::None;
            account.username = "you@example.com".to_string();
            account
        })
        .unwrap();
    jmail_core::secrets::store_password(&db, account, "hunter2").unwrap();

    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::events::Event::SyncFinished { .. }) {
            break;
        }
    }

    let inbox = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let welcome = db
        .messages_in_folder(inbox.id, 100)
        .unwrap()
        .into_iter()
        .find(|m| m.subject == "Welcome to Jmail")
        .unwrap();
    let other = db
        .messages_in_folder(inbox.id, 100)
        .unwrap()
        .into_iter()
        .find(|m| m.subject == "Re: Welcome to Jmail")
        .unwrap();
    assert_eq!(welcome.thread_id, other.thread_id);
    assert!(!welcome.flags.flagged);

    engine.send(
        account,
        SyncCommand::SetThreadFlagged {
            message: welcome.id,
            flagged: true,
        },
    );
    wait_until(&mut events, Duration::from_secs(10), || {
        [&welcome.id, &other.id].iter().all(|id| {
            db.get_message(**id)
                .unwrap()
                .is_some_and(|m| m.flags.flagged)
        })
    })
    .await;
    // The server sees the change too.
    let refetched = {
        let mut session = ImapSession::connect(
            settings(server.addr().port(), Security::None),
            "hunter2",
            &TlsTrust::system().unwrap(),
        )
        .await
        .unwrap();
        session.fetch_headers(&inbox.path, 0).await.unwrap()
    };
    assert!(refetched
        .iter()
        .filter(|m| m.new.subject.contains("Welcome to Jmail"))
        .all(|m| m.flags.flagged));

    engine.send(
        account,
        SyncCommand::SetThreadSeen {
            message: welcome.id,
            seen: true,
        },
    );
    wait_until(&mut events, Duration::from_secs(10), || {
        [&welcome.id, &other.id].iter().all(|id| {
            db.get_message(**id)
                .unwrap()
                .is_some_and(|m| m.flags.seen)
        })
    })
    .await;

    engine.stop_account(account);
    server.shutdown().await;
}

/// Poll a condition, draining sync events until it holds or the deadline
/// passes.
async fn wait_until(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<jmail_core::events::Event>,
    timeout: Duration,
    condition: impl Fn() -> bool,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("condition before deadline");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_extracts_attachment_metadata() {
    let server = start_server(false).await;
    let db = Db::open_in_memory().unwrap();
    let account = db
        .insert_account(&{
            let mut account = Account::generic("you@example.com");
            account.imap_host = "127.0.0.1".to_string();
            account.imap_port = server.addr().port();
            account.imap_security = Security::None;
            account.username = "you@example.com".to_string();
            account
        })
        .unwrap();
    jmail_core::secrets::store_password(&db, account, "hunter2").unwrap();

    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::events::Event::SyncFinished { .. }) {
            break;
        }
    }

    let inbox = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let report = db
        .messages_in_folder(inbox.id, 100)
        .unwrap()
        .into_iter()
        .find(|m| m.subject == "Quarterly report")
        .unwrap();
    assert!(!report.has_attachments, "attachments are only known after the body fetch");

    engine.send(
        account,
        SyncCommand::FetchBody {
            message: report.id,
        },
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let attachments = loop {
        let fetched = db.attachments(report.id).unwrap();
        if !fetched.is_empty() {
            break fetched;
        }
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::events::Event::MessagesChanged { .. }) {
            continue;
        }
    };
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename, "quarterly-report.pdf");
    assert_eq!(attachments[0].mime, "application/pdf");
    assert!(!attachments[0].inline);
    assert!(attachments[0].size > 0);
    assert!(db.get_message(report.id).unwrap().unwrap().has_attachments);
    // The text part inside the multipart/mixed body is still extracted.
    let body = db.message_body(report.id).unwrap().unwrap();
    assert!(body
        .text
        .as_deref()
        .unwrap_or("")
        .contains("Attached is the quarterly report."));

    engine.stop_account(account);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_receives_idle_push() {
    let server = start_server(false).await;
    let db = Db::open_in_memory().unwrap();
    let account = db
        .insert_account(&{
            let mut account = Account::generic("you@example.com");
            account.imap_host = "127.0.0.1".to_string();
            account.imap_port = server.addr().port();
            account.imap_security = Security::None;
            account.username = "you@example.com".to_string();
            account
        })
        .unwrap();
    jmail_core::secrets::store_password(&db, account, "hunter2").unwrap();

    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();

    // Skip to the first SyncFinished.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
        if matches!(event, jmail_core::events::Event::SyncFinished { .. }) {
            break;
        }
    }

    // Push a message while the engine idles.
    server
        .push(
            "INBOX",
            MockMessage::new(
                "Pushed while idling",
                "Ada Lovelace",
                "ada@example.org",
                &[("You", "you@example.com")],
                "push-test@example.org",
                "Mon, 05 Oct 2026 12:00:00 +0000",
                "Arrived during IDLE.",
                &[],
            ),
        )
        .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let inbox = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("idle push event before deadline")
            .expect("channel open");
        if let jmail_core::events::Event::MessagesChanged { folder, .. } = event {
            if folder == inbox.id
                && db.messages_in_folder(inbox.id, 100).unwrap().len() == 6
            {
                break;
            }
        }
    }
    let inbox = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let messages = db.messages_in_folder(inbox.id, 100).unwrap();
    assert_eq!(messages.len(), 6);
    assert!(messages.iter().any(|message| message.subject.contains("Pushed while idling")));

    engine.stop_account(account);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_clears_and_refetches_from_server() {
    let server = start_server(false).await;
    let db = Db::open_in_memory().unwrap();
    let account = db
        .insert_account(&{
            let mut account = Account::generic("you@example.com");
            account.imap_host = "127.0.0.1".to_string();
            account.imap_port = server.addr().port();
            account.imap_security = Security::None;
            account.username = "you@example.com".to_string();
            account
        })
        .unwrap();
    jmail_core::secrets::store_password(&db, account, "hunter2").unwrap();

    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::events::Event::SyncFinished { .. }) {
            break;
        }
    }

    let inbox = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let before = db.messages_in_folder(inbox.id, 100).unwrap();
    assert_eq!(before.len(), 5);
    let sample_id = before[0].id;
    let sample_uid = before[0].uid;

    engine.send(account, SyncCommand::Rebuild);

    // After a rebuild the rows are deleted and re-inserted, so the cached row
    // id for the same server UID must change.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = db.messages_in_folder(inbox.id, 100).unwrap();
        if now
            .iter()
            .any(|message| message.uid == sample_uid && message.id != sample_id)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "rebuild did not re-fetch messages"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(db.messages_in_folder(inbox.id, 100).unwrap().len(), 5);

    engine.stop_account(account);
    server.shutdown().await;
}
