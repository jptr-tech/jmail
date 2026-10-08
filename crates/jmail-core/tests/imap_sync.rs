//! End-to-end tests against the in-process mock IMAP server.

use std::time::Duration;

use jmail_core::imap::{ImapSession, ImapSettings, TlsTrust};
use jmail_core::message::Flags;
use jmail_core::{Account, Db, MoveTarget, Security, SpecialUse, SyncCommand, SyncEngine};
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
async fn skips_non_selectable_mailboxes() {
    // Gmail advertises container mailboxes such as `[Gmail]` with `\Noselect`.
    // They cannot be SELECTed, so they must not be returned as syncable
    // folders (doing so used to abort the whole account's first sync).
    let mut mailbox = MockMailbox::demo();
    mailbox.add_folder("Container", &["\\HasChildren", "\\Noselect"]);
    let server = MockServer::start(MockConfig::new("you@example.com", "hunter2"), mailbox)
        .await
        .expect("start mock server");

    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::None),
        "hunter2",
        &TlsTrust::system().unwrap(),
    )
    .await
    .expect("connect");

    let folders = session.list_folders().await.expect("list");
    assert!(
        !folders.iter().any(|folder| folder.path == "Container"),
        "non-selectable mailbox should be skipped: {folders:?}"
    );
    assert!(folders.iter().any(|folder| folder.special_use == SpecialUse::Inbox));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skips_virtual_duplicate_mailboxes() {
    // Gmail's Starred / Important re-list messages already held in real folders.
    // All Mail is kept, because it is the only home of archived unlabelled mail.
    let mut mailbox = MockMailbox::demo();
    mailbox.add_folder("All Mail", &["\\All", "\\HasNoChildren"]);
    mailbox.add_folder("Starred", &["\\Flagged", "\\HasNoChildren"]);
    mailbox.add_folder("Important", &["\\Important", "\\HasNoChildren"]);
    let server = MockServer::start(MockConfig::new("you@example.com", "hunter2"), mailbox)
        .await
        .expect("start mock server");

    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::None),
        "hunter2",
        &TlsTrust::system().unwrap(),
    )
    .await
    .expect("connect");

    let folders = session.list_folders().await.expect("list");
    for name in ["Starred", "Important"] {
        assert!(
            !folders.iter().any(|folder| folder.path == name),
            "virtual mailbox {name} should be skipped: {folders:?}"
        );
    }
    assert!(
        folders.iter().any(|folder| folder.path == "All Mail"),
        "All Mail must be kept so archived unlabelled mail is not lost"
    );
    assert!(folders.iter().any(|folder| folder.special_use == SpecialUse::Inbox));

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
async fn fetches_multiple_bodies_in_one_batch() {
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
    assert_eq!(headers.len(), 5);
    let uids: Vec<u32> = headers.iter().map(|message| message.uid).collect();

    let bodies = session.fetch_full_many(&inbox.path, &uids).await.unwrap();
    assert_eq!(bodies.len(), 5, "one batch should return every body");

    let welcome_uid = headers
        .iter()
        .find(|message| message.new.subject == "Welcome to Jmail")
        .map(|message| message.uid)
        .unwrap();
    let raw = bodies
        .iter()
        .find(|(uid, _)| *uid == welcome_uid)
        .map(|(_, body)| body)
        .unwrap();
    assert!(String::from_utf8_lossy(raw).contains("first message"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_prefetch_warms_inbox_bodies() {
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

    // Wait for the initial sync.
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

    // Prefetch starts a few seconds after entering IDLE; the whole 5-message
    // Inbox fits in the first batch.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let loaded = db
            .messages_in_folder(inbox.id, 100)
            .unwrap()
            .iter()
            .all(|message| db.body_loaded(message.id).unwrap());
        if loaded {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "background prefetch did not warm the Inbox in time"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

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
async fn engine_moves_thread_to_named_folder() {
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

    let folders = db.folders(account).unwrap();
    let inbox = folders
        .iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let projects = folders
        .iter()
        .find(|folder| folder.name == "Projects")
        .expect("mock has a Projects folder");
    let report = db
        .messages_in_folder(inbox.id, 100)
        .unwrap()
        .into_iter()
        .find(|message| message.subject == "Quarterly report")
        .expect("quarterly report in inbox");

    engine.send(
        account,
        SyncCommand::MoveThread {
            message: report.id,
            to: MoveTarget::Folder(projects.id),
        },
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::events::Event::MessagesChanged { .. })
            && db.get_message(report.id).unwrap().is_none()
        {
            break;
        }
    }
    assert!(
        db.get_message(report.id).unwrap().is_none(),
        "moved message should leave the inbox cache"
    );

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
        SyncCommand::FetchBodies {
            messages: vec![report.id],
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
async fn engine_saves_attachment_to_disk() {
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
        .find(|message| message.subject == "Quarterly report")
        .unwrap();

    let destination = std::env::temp_dir().join(format!(
        "jmail-attachment-{}.pdf",
        std::process::id()
    ));
    engine.send(
        account,
        SyncCommand::FetchAttachment {
            message: report.id,
            part: 0,
            destination: destination.clone(),
        },
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("event before deadline")
            .expect("channel open");
        if let jmail_core::events::Event::AttachmentSaved { error, .. } = event {
            assert!(error.is_none(), "save failed: {error:?}");
            break;
        }
    }
    let bytes = std::fs::read(&destination).unwrap();
    assert_eq!(bytes, b"%PDF-1.4 mock quarterly report\n");
    std::fs::remove_file(&destination).ok();

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

/// Insert a generic account wired to the mock's IMAP and SMTP listeners.
async fn account_with_smtp(server: &MockServer) -> (Db, jmail_core::AccountId) {
    let db = Db::open_in_memory().unwrap();
    let account = db
        .insert_account(&{
            let mut account = Account::generic("you@example.com");
            account.imap_host = "127.0.0.1".to_string();
            account.imap_port = server.addr().port();
            account.imap_security = Security::None;
            account.smtp_host = "127.0.0.1".to_string();
            account.smtp_port = server.smtp_addr().port();
            account.smtp_security = Security::None;
            account.username = "you@example.com".to_string();
            account
        })
        .unwrap();
    jmail_core::secrets::store_password(&db, account, "hunter2").unwrap();
    (db, account)
}

async fn wait_for_sync_finished(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<jmail_core::Event>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("sync finished before deadline")
            .expect("channel open");
        if matches!(event, jmail_core::Event::SyncFinished { .. }) {
            return;
        }
    }
}

async fn wait_send_result(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<jmail_core::Event>,
    id: u64,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("send result before deadline")
            .expect("channel open");
        if let jmail_core::Event::SendResult { id: got, error, .. } = event {
            if got == id {
                return error;
            }
        }
    }
}

async fn wait_draft_saved(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<jmail_core::Event>,
    id: u64,
) -> Option<jmail_core::DraftRef> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("draft saved before deadline")
            .expect("channel open");
        if let jmail_core::Event::DraftSaved { id: got, draft, .. } = event {
            if got == id {
                return draft;
            }
        }
    }
}

fn sample_outgoing(subject: &str, markdown: &str) -> jmail_core::OutgoingMessage {
    jmail_core::OutgoingMessage {
        message_id: jmail_core::OutgoingMessage::generate_message_id("you@example.com"),
        from: jmail_core::Address::new("You", "you@example.com"),
        to: vec![jmail_core::Address::new("Ada", "ada@example.org")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        markdown: markdown.to_string(),
        in_reply_to: String::new(),
        references: Vec::new(),
        attachments: Vec::new(),
        forward_of: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_sends_appends_and_forwards_attachments() {
    let server = start_server(false).await;
    let (db, account) = account_with_smtp(&server).await;
    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();
    wait_for_sync_finished(&mut events).await;

    // Forward the quarterly report so its attachment is re-fetched and re-sent.
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
        .find(|message| message.subject == "Quarterly report")
        .unwrap();

    let mut message = sample_outgoing("Test send", "# Hello\n\nThis is **bold**.");
    message.forward_of = Some(report.id);

    engine.send(
        account,
        SyncCommand::Send {
            id: 7,
            message: Box::new(message.clone()),
            delete_draft: None,
        },
    );
    let error = wait_send_result(&mut events, 7).await;
    assert!(error.is_none(), "send failed: {error:?}");

    let delivered = server.delivered().await;
    assert_eq!(delivered.len(), 1, "one message delivered over SMTP");
    let raw = String::from_utf8_lossy(&delivered[0]);
    assert!(raw.contains("Subject: Test send"));
    assert!(raw.contains("This is bold"), "plain alternative present");
    assert!(raw.contains("text/html"), "html alternative present");
    assert!(raw.contains("quarterly-report.pdf"), "forwarded attachment");

    // A copy landed in Sent.
    let sent = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Sent)
        .unwrap();
    assert!(db
        .messages_in_folder(sent.id, 100)
        .unwrap()
        .iter()
        .any(|stored| stored.message_id == message.message_id));

    engine.stop_account(account);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_saves_replaces_and_sends_drafts() {
    let server = start_server(false).await;
    let (db, account) = account_with_smtp(&server).await;
    let (engine, mut events) = SyncEngine::start(db.clone());
    engine.start_account(account).await.unwrap();
    wait_for_sync_finished(&mut events).await;

    let draft = sample_outgoing("Draft subject", "# Draft\n\nnotes body");
    engine.send(
        account,
        SyncCommand::SaveDraft {
            id: 11,
            message: Box::new(draft.clone()),
            replace: None,
        },
    );
    let first = wait_draft_saved(&mut events, 11).await.expect("draft saved");

    // The draft is cached in Drafts, flagged \Draft, and reopens as markdown.
    let drafts = db
        .folders(account)
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Drafts)
        .unwrap();
    let stored = db
        .messages_in_folder(drafts.id, 100)
        .unwrap()
        .into_iter()
        .find(|message| message.message_id == draft.message_id)
        .expect("draft cached");
    assert!(stored.flags.draft);

    engine.send(
        account,
        SyncCommand::FetchBodies {
            messages: vec![stored.id],
        },
    );
    wait_until(&mut events, Duration::from_secs(10), || {
        db.message_body(stored.id)
            .unwrap()
            .and_then(|body| body.text)
            .is_some()
    })
    .await;
    let body = db
        .message_body(stored.id)
        .unwrap()
        .and_then(|body| body.text)
        .unwrap();
    assert!(body.contains("# Draft"), "draft reopens as markdown: {body:?}");

    // Re-saving replaces the previous draft rather than piling up.
    let mut revised = draft.clone();
    revised.markdown = "# Draft v2".to_string();
    engine.send(
        account,
        SyncCommand::SaveDraft {
            id: 12,
            message: Box::new(revised.clone()),
            replace: Some(first.clone()),
        },
    );
    let second = wait_draft_saved(&mut events, 12).await.expect("draft replaced");
    assert_eq!(second.message_id, draft.message_id);

    // Sending deletes the stored draft and keeps a copy in Sent.
    engine.send(
        account,
        SyncCommand::Send {
            id: 13,
            message: Box::new(revised.clone()),
            delete_draft: Some(second.clone()),
        },
    );
    assert!(wait_send_result(&mut events, 13).await.is_none());

    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::None),
        "hunter2",
        &TlsTrust::system().unwrap(),
    )
    .await
    .unwrap();
    let drafts_path = drafts.path.clone();
    let server_drafts = session.fetch_headers(&drafts_path, 0).await.unwrap();
    assert!(
        !server_drafts
            .iter()
            .any(|message| message.new.message_id == draft.message_id),
        "draft removed from the server after sending"
    );

    engine.stop_account(account);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_append_lands_in_sent() {
    let server = start_server(false).await;
    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::None),
        "hunter2",
        &TlsTrust::system().unwrap(),
    )
    .await
    .unwrap();
    let full = session.fetch_full("INBOX", 1).await.unwrap();
    assert!(full.is_some());
    session
        .append(
            "Sent",
            "(\\Seen)",
            b"From: you@example.com\r\nTo: ada@example.org\r\nSubject: direct\r\nMessage-ID: <direct@x>\r\n\r\nhello",
        )
        .await
        .expect("append");
    let headers = session.fetch_headers("Sent", 0).await.unwrap();
    assert!(headers.iter().any(|m| m.new.message_id == "direct@x"));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syncs_reply_to_header() {
    let server = start_server(false).await;
    let mut session = ImapSession::connect(
        settings(server.addr().port(), Security::None),
        "hunter2",
        &TlsTrust::system().unwrap(),
    )
    .await
    .unwrap();
    let inbox = session
        .list_folders()
        .await
        .unwrap()
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
        .unwrap();
    let headers = session.fetch_headers(&inbox.path, 0).await.unwrap();
    let reply = headers
        .iter()
        .find(|message| message.new.subject == "Re: Quarterly report")
        .expect("reply message");
    assert_eq!(reply.new.reply_to.len(), 1);
    assert_eq!(reply.new.reply_to[0].email, "reports@example.org");

    // And it round-trips through the cache.
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
    let folder_id = db
        .upsert_folder(&jmail_core::Folder {
            id: jmail_core::FolderId(0),
            account_id: account,
            name: "Inbox".to_string(),
            path: inbox.path.clone(),
            special_use: SpecialUse::Inbox,
            unread: 0,
            total: 0,
            uidvalidity: 1,
            uidnext: 1,
            last_uid: 0,
        })
        .unwrap();
    let id = db
        .upsert_message(&{
            let mut new = reply.new.clone();
            new.account_id = account;
            new.folder_id = folder_id;
            new
        })
        .unwrap();
    let stored = db.get_message(id).unwrap().unwrap();
    assert_eq!(stored.reply_to[0].email, "reports@example.org");

    server.shutdown().await;
}
