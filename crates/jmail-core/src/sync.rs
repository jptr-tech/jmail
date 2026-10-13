//! Background sync: one task per account keeps the cache in step with the
//! server via IMAP IDLE plus a periodic poll.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::mpsc;
use time::OffsetDateTime;

use crate::account::AccountId;
use crate::cache::{Db, OpPayload, PendingOp};
use crate::credentials::{self, Credential, Purpose};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::folder::{FolderId, SpecialUse};
use crate::imap::{ImapSession, ImapSettings, TlsTrust};
use crate::message::{Attachment, Flags, MessageId};
use crate::outgoing::{DraftRef, OutgoingAttachment, OutgoingMessage};
use crate::smtp::{self, SmtpSettings};

/// How many of the newest Inbox messages to warm in the background.
const PREFETCH_LIMIT: u32 = 100;
/// Messages fetched per background prefetch batch.
const PREFETCH_CHUNK: usize = 5;
/// Pause between background prefetch batches so user actions take priority.
const PREFETCH_COOLDOWN: Duration = Duration::from_secs(5);
/// Let the initial sync settle before background prefetch starts.
const PREFETCH_START_DELAY: Duration = Duration::from_secs(5);
/// How long a queued operation keeps retrying transient failures before the
/// engine rolls it back locally.
const OP_FAIL_WINDOW: Duration = Duration::from_secs(60);

/// Where a conversation should be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTarget {
    /// A folder with the given special use (Inbox/Sent/Trash/…).
    Special(SpecialUse),
    /// A specific folder chosen by the user.
    Folder(FolderId),
}

/// Commands the UI can send to an account's sync worker.
#[derive(Debug, Clone)]
pub enum SyncCommand {
    /// Stop the worker (account removed or disabled).
    Shutdown,
    /// Re-list folders and re-sync every folder.
    Resync,
    /// Drop the account's cached mail and rebuild every folder from the server.
    Rebuild,
    /// Push new flags to the server and cache.
    SetFlags { message: MessageId, flags: Flags },
    /// Move a message to a special-use folder on the server.
    Move { message: MessageId, to: SpecialUse },
    /// Set `\Seen` on every message in the conversation identified by `message`.
    SetThreadSeen { message: MessageId, seen: bool },
    /// Set `\Flagged` on every message in the conversation identified by `message`.
    SetThreadFlagged { message: MessageId, flagged: bool },
    /// Move every message of the conversation (those in the representative's
    /// folder) to another folder.
    MoveThread { message: MessageId, to: MoveTarget },
    /// Apply any queued (optimistic) operations for this account now.
    FlushOutbox,
    /// Fetch and cache message bodies (batched per folder).
    FetchBodies { messages: Vec<MessageId> },
    /// Fetch a message and save one of its MIME attachments to a local path.
    /// `open` asks the UI to hand the file to the default viewer instead of
    /// treating it as a "saved" copy.
    FetchAttachment {
        message: MessageId,
        part: u32,
        destination: PathBuf,
        open: bool,
    },
    /// Send a composed message and append a copy to Sent.
    Send {
        id: u64,
        message: Box<OutgoingMessage>,
        delete_draft: Option<DraftRef>,
    },
    /// Append a draft to the Drafts folder (replacing `replace` if given).
    SaveDraft {
        id: u64,
        message: Box<OutgoingMessage>,
        replace: Option<DraftRef>,
    },
}

/// Background sync engine. Cloneable handle; the event receiver is returned by
/// [`SyncEngine::start`].
#[derive(Clone)]
pub struct SyncEngine {
    db: Db,
    events: mpsc::UnboundedSender<Event>,
    routing: Arc<StdMutex<HashMap<AccountId, mpsc::UnboundedSender<SyncCommand>>>>,
    poll_interval: Duration,
}

impl std::fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEngine").finish_non_exhaustive()
    }
}

impl SyncEngine {
    /// Create the engine and its event receiver. Spawn account workers onto
    /// the current tokio runtime with [`SyncEngine::start_account`].
    pub fn start(db: Db) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let poll_interval = db
            .get_setting("sync.poll_interval")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(60));
        let engine = Self {
            db,
            events: events_tx,
            routing: Arc::new(StdMutex::new(HashMap::new())),
            poll_interval,
        };
        (engine, events_rx)
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Send a command to one account's worker (no-op if it is not running).
    pub fn send(&self, account: AccountId, command: SyncCommand) {
        let sender = self.routing.lock().unwrap().get(&account).cloned();
        match sender {
            Some(sender) => {
                if sender.send(command).is_err() {
                    self.routing.lock().unwrap().remove(&account);
                    tracing::warn!(
                        account = account.0,
                        "sync worker has stopped; dropping command"
                    );
                }
            }
            None => tracing::warn!(
                account = account.0,
                "no sync worker for account; dropping command"
            ),
        }
    }

    /// Whether a sync worker is currently registered for the account.
    pub fn is_running(&self, account: AccountId) -> bool {
        self.routing.lock().unwrap().contains_key(&account)
    }

    /// Tell the UI that the account list or an account's enabled state changed.
    pub fn notify_accounts_changed(&self) {
        self.events.send(Event::AccountsChanged).ok();
    }

    /// Send a command to every running worker.
    pub fn send_all(&self, command: SyncCommand) {
        for sender in self.routing.lock().unwrap().values() {
            sender.send(command.clone()).ok();
        }
    }

    /// Spawn the worker task for an account.
    pub async fn start_account(&self, account: AccountId) -> Result<()> {
        let Some(account) = self.db.account(account)? else {
            return Err(Error::AccountNotFound(account.0));
        };
        let account_id = account.id;

        // Reserve the routing slot *before* the first await. Otherwise two
        // concurrent calls (e.g. initial bootstrap racing a window-focus
        // refresh) can both pass the running check and spawn two workers, and
        // therefore two IMAP connections, for the same account.
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        {
            let mut routing = self.routing.lock().unwrap();
            if routing.contains_key(&account_id) {
                return Ok(());
            }
            routing.insert(account_id, command_tx);
        }

        if let Err(error) = credentials::for_account(&self.db, &account, Purpose::Imap).await {
            self.routing.lock().unwrap().remove(&account_id);
            tracing::warn!(account = account_id.0, %error, "cannot resolve account credentials");
            let message = format!("cannot authenticate {}: {error}", account.email);
            self.events
                .send(Event::Error {
                    account: account_id,
                    message,
                })
                .ok();
            return Err(Error::Authentication(account_id.0));
        }

        let db = self.db.clone();
        let events = self.events.clone();
        let poll_interval = self.poll_interval;
        let settings = ImapSettings {
            host: account.imap_host.clone(),
            port: account.imap_port,
            security: account.imap_security,
            username: account.username.clone(),
        };
        let smtp = SmtpSettings {
            host: account.smtp_host.clone(),
            port: account.smtp_port,
            security: account.smtp_security,
            username: account.username.clone(),
            accept_invalid_certs: account.accept_invalid_certs,
        };
        let account_id = account.id;

        tokio::spawn(async move {
            worker(
                db,
                account_id,
                settings,
                smtp,
                command_rx,
                events,
                poll_interval,
            )
            .await;
        });
        Ok(())
    }

    /// Stop the worker for an account and forget its routing entry.
    pub fn stop_account(&self, account: AccountId) {
        if let Some(sender) = self.routing.lock().unwrap().remove(&account) {
            sender.send(SyncCommand::Shutdown).ok();
        }
    }

}

async fn worker(
    db: Db,
    account: AccountId,
    settings: ImapSettings,
    smtp: SmtpSettings,
    mut commands: mpsc::UnboundedReceiver<SyncCommand>,
    events: mpsc::UnboundedSender<Event>,
    poll_interval: Duration,
) {
    let mut backoff = Duration::from_secs(2);
    loop {
        events.send(Event::SyncStarted { account }).ok();
        match run_session(
            &db,
            account,
            &settings,
            &smtp,
            &mut commands,
            &events,
            poll_interval,
        )
        .await
        {
            Ok(()) => {
                events.send(Event::SyncFinished { account: account }).ok();
                return;
            }
            Err(error) => {
                tracing::warn!(account = %account.0, %error, "sync session failed; retrying");
                events
                    .send(Event::Error {
                        account: account,
                        message: error.to_string(),
                    })
                    .ok();
                events
                    .send(Event::SyncFinished { account: account })
                    .ok();
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

/// Run one connection for as long as it stays healthy. Returns `Ok(())` on
/// clean shutdown, `Err` when the connection should be re-established.
async fn run_session(
    db: &Db,
    account: AccountId,
    settings: &ImapSettings,
    smtp: &SmtpSettings,
    commands: &mut mpsc::UnboundedReceiver<SyncCommand>,
    events: &mpsc::UnboundedSender<Event>,
    poll_interval: Duration,
) -> Result<()> {
    let account_row = db
        .account(account)?
        .ok_or(Error::AccountNotFound(account.0))?;
    let trust = TlsTrust::for_account(&account_row)?;
    let credential = credentials::for_account(db, &account_row, Purpose::Imap).await?;
    let mut session = ImapSession::connect(settings.clone(), credential, &trust).await?;
    let supports_move = session.supports_move();
    tracing::info!(
        account = account.0,
        email = %account_row.email,
        host = %settings.host,
        "connected; syncing folders"
    );

    // 1. Folders + 2. initial header sync for every folder.
    let (mut folder_ids, mut inbox_path) = sync_all_folders(db, &mut session, account, events).await?;
    tracing::info!(
        account = account.0,
        folders = folder_ids.len(),
        "initial sync complete; entering IDLE"
    );

    // 3. Idle loop over the Inbox.
    let Some(mut inbox_path) = inbox_path.take() else {
        return Err(Error::Protocol("server has no INBOX".into()));
    };

    let mut session = Some(session);
    let mut first_idle = true;
    // Warm the newest Inbox bodies in the background, a small batch at a time,
    // so opening recent mail is instant (and the list gets snippets).
    let mut prefetch: VecDeque<MessageId> = prefetch_queue(db, account).into();
    if !prefetch.is_empty() {
        tracing::debug!(
            account = account.0,
            queued = prefetch.len(),
            "queueing background body prefetch"
        );
    }
    let mut next_prefetch = tokio::time::Instant::now() + PREFETCH_START_DELAY;
    loop {
        let Some(current) = session.take() else { return Ok(()) };
        let unsolicited = current.unsolicited_receiver();
        let mut handle = current.into_idle();
        handle.init().await.map_err(protocol)?;
        let deadline = tokio::time::Instant::now() + poll_interval;
        let mut prefetch_now = false;
        let mut outbox_now = false;
        // Queued (optimistic) operations take priority: wake immediately if one
        // is due, otherwise sleep until the next scheduled retry.
        let pending = db.pending_ops(account).unwrap_or_default();
        let now_secs = OffsetDateTime::now_utc().unix_timestamp();
        let outbox_due = pending.iter().any(|op| op.next_attempt <= now_secs);
        let outbox_wait = pending
            .iter()
            .filter(|op| op.next_attempt > now_secs)
            .map(|op| (op.next_attempt - now_secs) as u64)
            .min();
        let got_command = if first_idle {
            // Mail that arrived between the initial sync and the IDLE
            // subscription would be missed until the next poll, so resync once
            // right after entering IDLE.
            first_idle = false;
            None
        } else if outbox_due {
            outbox_now = true;
            None
        } else {
            loop {
                let (wait, _stop) = handle.wait_with_timeout(Duration::from_secs(29 * 60));
                tokio::pin!(wait);
                tokio::select! {
                    result = &mut wait => {
                        match result {
                            // Server keepalive, EXISTS/EXPUNGE, or idle timeout.
                            Ok(_) => break None,
                            Err(error) => return Err(protocol(error)),
                        }
                    }
                    command = commands.recv() => break command,
                    response = unsolicited.recv() => {
                        if matches!(
                            response,
                            Ok(async_imap::types::UnsolicitedResponse::Exists(_))
                                | Ok(async_imap::types::UnsolicitedResponse::Expunge(_))
                        ) {
                            break None;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => break None, // poll tick
                    _ = tokio::time::sleep_until(next_prefetch), if !prefetch.is_empty() => {
                        prefetch_now = true;
                        break None;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(outbox_wait.unwrap_or(3600))), if outbox_wait.is_some() => {
                        outbox_now = true;
                        break None;
                    }
                }
            }
        };
        let raw = handle.done().await.map_err(protocol)?;
        let mut conn = ImapSession::from_session(raw, settings.clone(), supports_move);

        // Confirm or roll back queued operations before anything slower.
        if !pending.is_empty()
            || outbox_now
            || matches!(&got_command, Some(SyncCommand::FlushOutbox))
        {
            if let Err(error) = flush_outbox(db, &mut conn, account, events).await {
                tracing::warn!(account = account.0, %error, "outbox flush failed");
            }
            if outbox_now {
                session = Some(conn);
                continue;
            }
        }

        if prefetch_now {
            if db.has_pending_ops(account).unwrap_or(false) {
                // Let queued user actions go first; try prefetch again later.
                next_prefetch = tokio::time::Instant::now() + PREFETCH_COOLDOWN;
                session = Some(conn);
                continue;
            }
            let count = prefetch.len().min(PREFETCH_CHUNK);
            let chunk: Vec<MessageId> = prefetch.drain(..count).collect();
            if let Err(error) = apply_fetch_bodies(db, &mut conn, account, chunk, events).await {
                tracing::warn!(account = account.0, %error, "background prefetch failed");
            }
            next_prefetch = tokio::time::Instant::now() + PREFETCH_COOLDOWN;
            session = Some(conn);
            continue;
        }

        match got_command {
            Some(SyncCommand::Shutdown) => return Ok(()),
            Some(SyncCommand::Rebuild) => {
                db.clear_account_messages(account)?;
                db.reset_folder_sync_state(account)?;
                let (ids, inbox) = sync_all_folders(db, &mut conn, account, events).await?;
                folder_ids = ids;
                if let Some(path) = inbox {
                    inbox_path = path;
                }
            }
            Some(SyncCommand::Resync) => {
                let folders: Vec<(String, crate::folder::FolderId)> = folder_ids
                    .iter()
                    .map(|(path, id)| (path.clone(), *id))
                    .collect();
                for (path, folder_id) in folders {
                    let last_uid = db
                        .folder(folder_id)?
                        .map(|folder| folder.last_uid)
                        .unwrap_or(0);
                    sync_folder_inner(db, &mut conn, account, folder_id, &path, last_uid, false)
                        .await?;
                    events
                        .send(Event::MessagesChanged {
                            account,
                            folder: folder_id,
                        })
                        .ok();
                }
                // One rebuild for the whole pass, after every folder is in.
                db.rethread_account(account)?;
            }
            Some(SyncCommand::SetFlags { message, flags }) => {
                apply_flags(db, &mut conn, account, message, flags, events).await?;
            }
            Some(SyncCommand::Move { message, to }) => {
                apply_move(db, &mut conn, account, message, to, events).await?;
            }
            Some(SyncCommand::SetThreadSeen { message, seen }) => {
                apply_thread_flags(db, &mut conn, message, ThreadFlagChange::Seen(seen), events)
                    .await?;
            }
            Some(SyncCommand::SetThreadFlagged { message, flagged }) => {
                apply_thread_flags(
                    db,
                    &mut conn,
                    message,
                    ThreadFlagChange::Flagged(flagged),
                    events,
                )
                .await?;
            }
            Some(SyncCommand::MoveThread { message, to }) => {
                apply_move_thread(db, &mut conn, message, to, events).await?;
            }
            // Already flushed above, before prefetch.
            Some(SyncCommand::FlushOutbox) => {}
            Some(SyncCommand::FetchBodies { messages }) => {
                apply_fetch_bodies(db, &mut conn, account, messages, events).await?;
            }
            Some(SyncCommand::FetchAttachment {
                message,
                part,
                destination,
                open,
            }) => {
                apply_fetch_attachment(db, &mut conn, message, part, destination, open, events)
                    .await?;
            }
            Some(SyncCommand::Send {
                id,
                message,
                delete_draft,
            }) => {
                tracing::info!(account = account.0, id, "sending composed message");
                match credentials::for_account(db, &account_row, Purpose::Smtp).await {
                    Ok(credential) => {
                        send_message(
                            db,
                            &mut conn,
                            smtp,
                            &credential,
                            account,
                            id,
                            *message,
                            delete_draft,
                            events,
                        )
                        .await;
                    }
                    Err(error) => {
                        events
                            .send(Event::SendResult {
                                id,
                                account,
                                message_id: message.message_id.clone(),
                                error: Some(error.to_string()),
                            })
                            .ok();
                    }
                }
            }
            Some(SyncCommand::SaveDraft {
                id,
                message,
                replace,
            }) => {
                save_draft(db, &mut conn, account, id, *message, replace, events).await;
            }
            None => {
                // Poll timeout or new data: refresh the Inbox.
                let folder_id = folder_ids[&inbox_path];
                let last_uid = db.folder(folder_id)?.map(|f| f.last_uid).unwrap_or(0);
                sync_folder(db, &mut conn, account, folder_id, &inbox_path, last_uid).await?;
                events
                    .send(Event::MessagesChanged { account, folder: folder_id })
                    .ok();
            }
        }
        session = Some(conn);
    }
}

/// The newest Inbox messages that still need bodies, newest first, capped at
/// [`PREFETCH_LIMIT`].
fn prefetch_queue(db: &Db, account: AccountId) -> Vec<MessageId> {
    let Ok(folders) = db.folders(account) else {
        return Vec::new();
    };
    let Some(inbox) = folders
        .into_iter()
        .find(|folder| folder.special_use == SpecialUse::Inbox)
    else {
        return Vec::new();
    };
    let Ok(messages) = db.messages_in_folder(inbox.id, PREFETCH_LIMIT) else {
        return Vec::new();
    };
    messages
        .into_iter()
        .filter(|message| matches!(db.body_loaded(message.id), Ok(false)))
        .map(|message| message.id)
        .take(PREFETCH_LIMIT as usize)
        .collect()
}

/// List folders, upsert them into the cache, and fetch headers for each from
/// the beginning. Returns the folder id map and the Inbox path.
async fn sync_all_folders(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(HashMap<String, crate::folder::FolderId>, Option<String>)> {
    let server_folders = session.list_folders().await?;
    let mut folder_ids: HashMap<String, crate::folder::FolderId> = HashMap::new();
    let mut inbox_path = None;
    for server_folder in &server_folders {
        let folder = crate::folder::Folder {
            id: crate::folder::FolderId(0),
            account_id: account,
            name: server_folder
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&server_folder.path)
                .to_string(),
            path: server_folder.path.clone(),
            special_use: server_folder.special_use,
            unread: 0,
            total: 0,
            uidvalidity: 0,
            uidnext: 0,
            last_uid: 0,
        };
        let id = db.upsert_folder(&folder)?;
        folder_ids.insert(server_folder.path.clone(), id);
        if server_folder.special_use == SpecialUse::Inbox {
            inbox_path = Some(server_folder.path.clone());
        }
    }
    events.send(Event::FoldersChanged { account }).ok();

    // Drop cached folders the server no longer offers (for example Gmail's
    // virtual All Mail / Important / Starred, which we no longer sync) so their
    // duplicate messages stop showing up.
    let keep: Vec<String> = server_folders
        .iter()
        .map(|folder| folder.path.clone())
        .collect();
    match db.retain_folders(account, &keep) {
        Ok(removed) if removed > 0 => {
            tracing::info!(account = account.0, removed, "pruned stale mailboxes");
            db.rethread_account(account)?;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(account = account.0, %error, "could not prune stale mailboxes");
        }
    }

    for server_folder in &server_folders {
        let Some(folder_id) = folder_ids.get(&server_folder.path) else {
            continue;
        };
        if let Err(error) =
            sync_folder_inner(db, session, account, *folder_id, &server_folder.path, 0, false).await
        {
            // One bad mailbox (e.g. a race with a server-side deletion) should
            // not abort the whole account's first sync.
            tracing::warn!(
                account = account.0,
                folder = %server_folder.path,
                %error,
                "skipping folder after sync error"
            );
            continue;
        }
        events
            .send(Event::MessagesChanged {
                account,
                folder: *folder_id,
            })
            .ok();
    }
    db.rethread_account(account)?;
    events.send(Event::SyncFinished { account }).ok();
    Ok((folder_ids, inbox_path))
}

/// Fetch new headers for a folder and write them to the cache.
async fn sync_folder(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    folder_id: crate::folder::FolderId,
    path: &str,
    since_uid: u32,
) -> Result<()> {
    sync_folder_inner(db, session, account, folder_id, path, since_uid, true).await
}

/// Like [`sync_folder`], but `rethread` can be switched off when the caller is
/// batching several folders and rebuilds conversations once at the end (the
/// initial sync). Rebuilding after every folder is O(messages) each time and
/// holds the shared cache lock, which is what makes the UI stutter during
/// startup sync; one rebuild per pass is enough.
async fn sync_folder_inner(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    folder_id: crate::folder::FolderId,
    path: &str,
    since_uid: u32,
    rethread: bool,
) -> Result<()> {
    let mailbox = session.select(path).await?;
    let cached = db.folder(folder_id)?;
    let (old_validity, last_uid) = cached
        .map(|folder| (folder.uidvalidity, folder.last_uid))
        .unwrap_or((0, 0));

    // A UIDVALIDITY change invalidates every cached UID in the folder.
    if old_validity != 0 && old_validity != mailbox.uid_validity {
        db.clear_folder_messages(folder_id)?;
    }

    let since = if since_uid == 0 { last_uid } else { since_uid };
    let messages = if mailbox.exists == 0 {
        Vec::new()
    } else {
        session.fetch_headers(path, since).await?
    };

    let mut highest = since;
    let mut inserted = false;
    for message in &messages {
        // `UID FETCH n:*` may re-report the last message we already have.
        if message.uid <= since {
            continue;
        }
        let mut new_message = message.new.clone();
        new_message.account_id = account;
        new_message.folder_id = folder_id;
        db.upsert_message(&new_message)?;
        highest = highest.max(message.uid);
        inserted = true;
    }
    // New headers can link previously-separate conversations, so rebuild the
    // account's thread assignment whenever anything changed. Callers batching
    // folders pass `rethread = false` and rebuild once at the end.
    if inserted && rethread {
        db.rethread_account(account)?;
    }
    let unread = count_unread(db, folder_id)?;
    db.set_folder_state(
        folder_id,
        mailbox.uid_validity,
        mailbox.uid_next,
        highest,
        mailbox.exists,
        unread,
    )?;
    Ok(())
}

fn count_unread(db: &Db, folder_id: crate::folder::FolderId) -> Result<u32> {
    Ok(db
        .messages_in_folder(folder_id, 10_000)?
        .iter()
        .filter(|message| !message.flags.seen)
        .count() as u32)
}

async fn apply_flags(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    message: MessageId,
    flags: Flags,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let Some((_, path, folder_id, uid, _)) = db.message_location(message)? else {
        return Ok(());
    };
    session.set_flags(&path, uid, flags).await?;
    db.set_flags(message, flags)?;
    events
        .send(Event::MessagesChanged { account, folder: folder_id })
        .ok();
    Ok(())
}

async fn apply_move(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    message: MessageId,
    to: SpecialUse,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let Some((_, path, folder_id, uid, _)) = db.message_location(message)? else {
        return Ok(());
    };
    let destination = db
        .folders(account)?
        .into_iter()
        .find(|folder| folder.special_use == to)
        .map(|folder| folder.path);
    match destination {
        Some(destination) => {
            session.move_message(&path, uid, &destination).await?;
            db.delete_message(message)?;
        }
        None => {
            session
                .set_flags(&path, uid, Flags {
                    seen: true,
                    ..Flags::default()
                })
                .await?;
        }
    }
    events
        .send(Event::MessagesChanged { account, folder: folder_id })
        .ok();
    Ok(())
}

/// Which conversation-wide flag bit to apply.
#[derive(Clone, Copy)]
enum ThreadFlagChange {
    Seen(bool),
    Flagged(bool),
}

/// Apply a `\Seen`/`\Flagged` change to every message in a conversation.
async fn apply_thread_flags(
    db: &Db,
    session: &mut ImapSession,
    identify: MessageId,
    change: ThreadFlagChange,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let Some(message) = db.get_message(identify)? else {
        return Ok(());
    };
    let account = message.account_id;
    let folder = message.folder_id;
    let locations = db.thread_locations(account, &message.thread_id)?;

    let mut changed = false;
    for (id, _folder, path, uid, flags) in &locations {
        let mut new_flags = *flags;
        let dirty = match change {
            ThreadFlagChange::Seen(seen) => {
                let dirty = new_flags.seen != seen;
                new_flags.seen = seen;
                dirty
            }
            ThreadFlagChange::Flagged(flagged) => {
                let dirty = new_flags.flagged != flagged;
                new_flags.flagged = flagged;
                dirty
            }
        };
        if !dirty {
            continue;
        }
        session.set_flags(path, *uid, new_flags).await?;
        db.set_flags(*id, new_flags)?;
        changed = true;
    }
    if changed {
        events.send(Event::MessagesChanged { account, folder }).ok();
    }
    Ok(())
}

/// Move every message of a conversation that lives in the representative's
/// folder (so archiving a thread does not drag along its Sent copies).
async fn apply_move_thread(
    db: &Db,
    session: &mut ImapSession,
    identify: MessageId,
    to: MoveTarget,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let Some(message) = db.get_message(identify)? else {
        return Ok(());
    };
    let account = message.account_id;
    let folder_id = message.folder_id;
    let folders = db.folders(account)?;
    let destination = match to {
        MoveTarget::Special(special) => folders
            .iter()
            .find(|folder| folder.special_use == special)
            .map(|folder| folder.path.clone()),
        MoveTarget::Folder(id) => folders
            .iter()
            .find(|folder| folder.id == id)
            .map(|folder| folder.path.clone()),
    };
    let Some(destination) = destination else {
        if let MoveTarget::Folder(id) = to {
            tracing::warn!(account = account.0, folder = id.0, "move target folder not found");
            return Ok(());
        }
        // No special-use destination (e.g. archiving on a server without an
        // Archive mailbox): fall back to marking the conversation seen.
        let locations = db.thread_locations(account, &message.thread_id)?;
        for (id, folder, path, uid, flags) in &locations {
            if *folder != folder_id {
                continue;
            }
            let mut new_flags = *flags;
            new_flags.seen = true;
            session.set_flags(path, *uid, new_flags).await?;
            db.set_flags(*id, new_flags)?;
        }
        events
            .send(Event::MessagesChanged {
                account,
                folder: folder_id,
            })
            .ok();
        return Ok(());
    };
    let locations = db.thread_locations(account, &message.thread_id)?;

    for (id, folder, path, uid, _) in &locations {
        if *folder != folder_id {
            continue;
        }
        session.move_message(path, *uid, &destination).await?;
        db.delete_message(*id)?;
    }
    events
        .send(Event::MessagesChanged {
            account,
            folder: folder_id,
        })
        .ok();
    Ok(())
}

/// Backoff (seconds) before retrying a transiently failed queued op.
fn op_backoff(attempts: i64) -> i64 {
    match attempts {
        0 => 1,
        1 => 2,
        2 => 5,
        3 => 15,
        _ => 30,
    }
}

/// Whether a failure is permanent (roll back now rather than keep retrying).
fn is_hard_error(error: &Error) -> bool {
    matches!(
        error,
        Error::FolderNotFound(_)
            | Error::MessageNotFound(_)
            | Error::Config(_)
            | Error::Authentication(_)
            | Error::Unimplemented
    )
}

/// Apply the queued (optimistically cached) operations for `account`, retrying
/// transient failures and rolling back anything that ultimately fails. This is
/// what makes archive/delete/move feel instant: the cache already reflects them,
/// so the only work left is confirming them on the server.
async fn flush_outbox(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    for op in db.pending_ops(account)? {
        if op.next_attempt > now {
            continue;
        }
        let identify = op
            .payload
            .messages()
            .first()
            .map(|message| MessageId(message.message_id));
        match apply_queued_op(db, session, account, &op).await {
            Ok(folders) => {
                db.complete_op(op.id)?;
                for folder in folders {
                    events
                        .send(Event::MessagesChanged { account, folder })
                        .ok();
                }
            }
            Err(error) => {
                let expired = now - op.created_at >= OP_FAIL_WINDOW.as_secs() as i64;
                if is_hard_error(&error) || expired {
                    tracing::warn!(op = op.id, %error, "queued operation failed; rolling back");
                    db.rollback_op(op.id)?;
                    if let Some(message) = identify {
                        events
                            .send(Event::OpFailed {
                                account,
                                message,
                                error: error.to_string(),
                            })
                            .ok();
                    }
                } else {
                    let next = now + op_backoff(op.attempts);
                    tracing::debug!(op = op.id, %error, "queued operation will retry");
                    db.fail_op(op.id, next, &error.to_string())?;
                }
            }
        }
    }
    Ok(())
}

/// Perform one queued operation on the server. Returns the folders whose lists
/// changed (for `MessagesChanged`).
async fn apply_queued_op(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    op: &PendingOp,
) -> Result<Vec<FolderId>> {
    let mut folders = Vec::new();
    match &op.payload {
        OpPayload::Move { target, messages } => match target {
            Some(id) => {
                let destination_id = FolderId(*id);
                let destination = db
                    .folders(account)?
                    .into_iter()
                    .find(|folder| folder.id == destination_id)
                    .ok_or(Error::FolderNotFound(*id))?;
                for message in messages {
                    folders.push(FolderId(message.folder_id));
                    session
                        .move_message(&message.path, message.uid, &destination.path)
                        .await?;
                }
                // Re-fetch the destination so the moved messages pick up their
                // new UIDs (our optimistic rows still hold the source UID).
                let last_uid = db
                    .folder(destination_id)?
                    .map(|folder| folder.last_uid)
                    .unwrap_or(0);
                if let Err(error) =
                    sync_folder(db, session, account, destination_id, &destination.path, last_uid)
                        .await
                {
                    tracing::warn!(
                        account = account.0,
                        folder = %destination.path,
                        %error,
                        "could not resync destination after move"
                    );
                }
                folders.push(destination_id);
            }
            None => {
                for message in messages {
                    folders.push(FolderId(message.folder_id));
                    let mut flags = Flags::from_i64(message.flags);
                    flags.seen = true;
                    session.set_flags(&message.path, message.uid, flags).await?;
                }
            }
        },
        OpPayload::Flags { bit, value, messages } => {
            for message in messages {
                folders.push(FolderId(message.folder_id));
                let mut flags = Flags::from_i64(message.flags);
                if *bit == Flags::SEEN {
                    flags.seen = *value;
                } else if *bit == Flags::FLAGGED {
                    flags.flagged = *value;
                }
                session.set_flags(&message.path, message.uid, flags).await?;
            }
        }
    }
    folders.sort_by_key(|folder| folder.0);
    folders.dedup_by_key(|folder| folder.0);
    Ok(folders)
}

/// Fetch and cache the bodies of several messages, batched per folder so a
/// whole conversation costs one round trip per folder instead of one per
/// message.
async fn apply_fetch_bodies(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    messages: Vec<MessageId>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let mut by_folder: HashMap<String, Vec<(MessageId, u32)>> = HashMap::new();
    for message in messages {
        let Some((_account, path, _folder, uid, _)) = db.message_location(message)? else {
            continue;
        };
        if db.body_loaded(message)? {
            continue;
        }
        by_folder.entry(path).or_default().push((message, uid));
    }

    for (path, items) in by_folder {
        let uids: Vec<u32> = items.iter().map(|(_, uid)| *uid).collect();
        let bodies = match session.fetch_full_many(&path, &uids).await {
            Ok(bodies) => bodies,
            Err(error) => {
                tracing::warn!(account = account.0, folder = %path, %error, "could not fetch message bodies");
                continue;
            }
        };
        for (uid, raw) in bodies {
            let Some((message, _)) = items.iter().find(|(_, item_uid)| *item_uid == uid) else {
                continue;
            };
            if let Err(error) = store_fetched_body(db, account, *message, &raw, events) {
                tracing::warn!(account = account.0, %error, "could not store message body");
            }
        }
    }
    Ok(())
}

/// Parse a fetched message, cache its text/HTML and attachment metadata, and
/// tell the UI the body is ready.
fn store_fetched_body(
    db: &Db,
    account: AccountId,
    message: MessageId,
    raw: &[u8],
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let parsed = mail_parser::MessageParser::default().parse(raw);
    let raw_text = parsed
        .as_ref()
        .and_then(|mail| mail.body_text(0))
        .map(|text| text.to_string());
    let html = parsed
        .as_ref()
        .and_then(|mail| mail.body_html(0))
        .map(|html| html.to_string());
    let (text, html) = prepare_body(raw_text, html);
    let snippet = snippet_of(text.as_deref().unwrap_or(""));
    db.set_body(message, text.as_deref(), html.as_deref(), &snippet)?;

    // Attachment metadata (contents are not cached).
    let attachments = parsed
        .as_ref()
        .map(|mail| {
            use mail_parser::MimeHeaders;
            mail.attachments()
                .enumerate()
                .map(|(index, part)| {
                    let mime = match part.content_type() {
                        Some(content_type) => match content_type.subtype() {
                            Some(subtype) => {
                                format!("{}/{}", content_type.ctype(), subtype)
                            }
                            None => content_type.ctype().to_string(),
                        },
                        None => String::new(),
                    };
                    let inline = part
                        .content_disposition()
                        .map(|disposition| disposition.is_inline())
                        .unwrap_or_else(|| part.content_id().is_some());
                    Attachment {
                        filename: part.attachment_name().unwrap_or("").to_string(),
                        mime,
                        size: part.contents().len() as u64,
                        content_id: part.content_id().map(str::to_string),
                        inline,
                        part: index.to_string(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    db.set_attachments(message, &attachments)?;
    events
        .send(Event::BodyLoaded { account, message })
        .ok();
    Ok(())
}

/// Fetch a message's raw form and write one MIME attachment to `destination`.
/// Attachment bytes are not cached, so this re-downloads the message; failures
/// are reported through [`Event::AttachmentSaved`] rather than the worker.
async fn apply_fetch_attachment(
    db: &Db,
    session: &mut ImapSession,
    message: MessageId,
    part: u32,
    destination: PathBuf,
    open: bool,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let report = |error: Option<String>| {
        events
            .send(Event::AttachmentSaved {
                message,
                part,
                path: destination.clone(),
                open,
                error,
            })
            .ok();
    };
    let Some((_account, path, _folder, uid, _)) = db.message_location(message)? else {
        report(Some("message not found".to_string()));
        return Ok(());
    };
    let raw = match session.fetch_full(&path, uid).await {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            report(Some("message is no longer on the server".to_string()));
            return Ok(());
        }
        Err(error) => {
            report(Some(error.to_string()));
            return Ok(());
        }
    };
    let Some(parsed) = mail_parser::MessageParser::default().parse(&raw) else {
        report(Some("could not parse message".to_string()));
        return Ok(());
    };
    let Some(attachment) = parsed.attachments().nth(part as usize) else {
        report(Some("attachment not found".to_string()));
        return Ok(());
    };
    if let Err(error) = std::fs::write(&destination, attachment.contents()) {
        report(Some(error.to_string()));
        return Ok(());
    }
    tracing::info!(message = message.0, part, path = %destination.display(), "saved attachment");
    report(None);
    Ok(())
}

/// Deliver a composed message: resolve forwarded attachments, send via SMTP,
/// append a copy to Sent, and report the outcome.
#[allow(clippy::too_many_arguments)]
async fn send_message(
    db: &Db,
    session: &mut ImapSession,
    smtp_settings: &SmtpSettings,
    credential: &Credential,
    account: AccountId,
    id: u64,
    mut message: OutgoingMessage,
    delete_draft: Option<DraftRef>,
    events: &mpsc::UnboundedSender<Event>,
) {
    let message_id = message.message_id.clone();
    let error = deliver(
        db,
        session,
        smtp_settings,
        credential,
        account,
        &mut message,
        delete_draft,
        events,
    )
    .await
    .err()
    .map(|error| error.to_string());
    events
        .send(Event::SendResult {
            id,
            account,
            message_id,
            error,
        })
        .ok();
}

#[allow(clippy::too_many_arguments)]
async fn deliver(
    db: &Db,
    session: &mut ImapSession,
    smtp_settings: &SmtpSettings,
    credential: &Credential,
    account: AccountId,
    message: &mut OutgoingMessage,
    delete_draft: Option<DraftRef>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    if let Some(original) = message.forward_of.take() {
        message
            .attachments
            .extend(forward_attachments(db, session, original).await?);
    }

    let built = message.build_message()?;
    let raw = built.formatted();
    smtp::send(smtp_settings, credential, built).await?;

    if let Some(draft) = delete_draft {
        // Best-effort: the message is already sent.
        let _ = session.delete_message(&draft.folder, draft.uid).await;
        if let Ok(Some(local)) = db.message_by_message_id(account, &draft.message_id) {
            let _ = db.delete_message(local.id);
            events
                .send(Event::MessagesChanged {
                    account,
                    folder: local.folder_id,
                })
                .ok();
        }
    }

    // Append to Sent; a missing Sent folder shouldn't fail the send.
    if let Some((path, folder_id)) = special_folder(db, account, SpecialUse::Sent)? {
        session.append(&path, "(\\Seen)", &raw).await?;
        let last_uid = db.folder(folder_id)?.map(|f| f.last_uid).unwrap_or(0);
        sync_folder(db, session, account, folder_id, &path, last_uid).await?;
        events
            .send(Event::MessagesChanged {
                account,
                folder: folder_id,
            })
            .ok();
    }

    mark_answered(db, session, account, message).await;
    Ok(())
}

/// Flag the message a reply answers as `\Answered`, best-effort.
async fn mark_answered(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    message: &OutgoingMessage,
) {
    if message.in_reply_to.is_empty() {
        return;
    }
    let Ok(Some(parent)) = db.message_by_message_id(account, &message.in_reply_to) else {
        return;
    };
    let mut flags = parent.flags;
    if flags.answered {
        return;
    }
    flags.answered = true;
    if let Ok(Some((_, path, _, uid, _))) = db.message_location(parent.id) {
        if session.set_flags(&path, uid, flags).await.is_ok() {
            let _ = db.set_flags(parent.id, flags);
        }
    }
}

/// Save a draft to the Drafts folder, replacing `replace` when given.
async fn save_draft(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    id: u64,
    message: OutgoingMessage,
    replace: Option<DraftRef>,
    events: &mpsc::UnboundedSender<Event>,
) {
    match store_draft(db, session, account, &message, replace, events).await {
        Ok(draft) => {
            events
                .send(Event::DraftSaved {
                    id,
                    account,
                    draft: Some(draft),
                    error: None,
                })
                .ok();
        }
        Err(error) => {
            events
                .send(Event::DraftSaved {
                    id,
                    account,
                    draft: None,
                    error: Some(error.to_string()),
                })
                .ok();
        }
    }
}

async fn store_draft(
    db: &Db,
    session: &mut ImapSession,
    account: AccountId,
    message: &OutgoingMessage,
    replace: Option<DraftRef>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<DraftRef> {
    let Some((path, folder_id)) = special_folder(db, account, SpecialUse::Drafts)? else {
        return Err(Error::Config(
            "this account has no Drafts folder".to_string(),
        ));
    };
    if let Some(draft) = replace {
        let _ = session.delete_message(&draft.folder, draft.uid).await;
        if let Ok(Some(local)) = db.message_by_message_id(account, &draft.message_id) {
            let _ = db.delete_message(local.id);
        }
    }
    let built = message.build_draft()?;
    let raw = built.formatted();
    session.append(&path, "(\\Draft)", &raw).await?;
    let last_uid = db.folder(folder_id)?.map(|f| f.last_uid).unwrap_or(0);
    sync_folder(db, session, account, folder_id, &path, last_uid).await?;
    let found = db
        .message_by_message_id(account, &message.message_id)?
        .ok_or_else(|| Error::Protocol("saved draft not found after sync".to_string()))?;
    events
        .send(Event::MessagesChanged {
            account,
            folder: folder_id,
        })
        .ok();
    Ok(DraftRef {
        folder: path,
        uid: found.uid,
        message_id: message.message_id.clone(),
    })
}

/// Re-fetch an original message and turn its MIME parts into attachments.
async fn forward_attachments(
    db: &Db,
    session: &mut ImapSession,
    original: MessageId,
) -> Result<Vec<OutgoingAttachment>> {
    let Some((_, path, _, uid, _)) = db.message_location(original)? else {
        return Ok(Vec::new());
    };
    let Some(raw) = session.fetch_full(&path, uid).await? else {
        return Ok(Vec::new());
    };
    let Some(mail) = mail_parser::MessageParser::default().parse(&raw) else {
        return Ok(Vec::new());
    };
    use mail_parser::MimeHeaders;
    let attachments = mail
        .attachments()
        .map(|part| {
            let mime = match part.content_type() {
                Some(content_type) => match content_type.subtype() {
                    Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
                    None => content_type.ctype().to_string(),
                },
                None => "application/octet-stream".to_string(),
            };
            let content_id = part.content_id().map(str::to_string);
            let inline = part
                .content_disposition()
                .map(|disposition| disposition.is_inline())
                .unwrap_or(false)
                || (part.content_disposition().is_none() && content_id.is_some());
            OutgoingAttachment {
                filename: part.attachment_name().unwrap_or("attachment").to_string(),
                mime,
                data: part.contents().to_vec(),
                inline: inline && content_id.is_some(),
                content_id,
            }
        })
        .collect();
    Ok(attachments)
}

fn special_folder(
    db: &Db,
    account: AccountId,
    special: SpecialUse,
) -> Result<Option<(String, crate::folder::FolderId)>> {
    Ok(db
        .folders(account)?
        .into_iter()
        .find(|folder| folder.special_use == special)
        .map(|folder| (folder.path, folder.id)))
}

fn snippet_of(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut snippet: String = collapsed.chars().take(160).collect();
    if snippet.len() < collapsed.len() {
        snippet.push('…');
    }
    snippet
}

/// Normalise the parsed `text/plain` and `text/html` parts.
///
/// Some senders put their HTML in the text part: flatten that to readable text
/// for the preview and search, and when there is no real HTML part, promote it
/// so the reading pane renders it instead of showing the tags literally.
fn prepare_body(
    raw_text: Option<String>,
    mut html: Option<String>,
) -> (Option<String>, Option<String>) {
    let text = raw_text.map(|text| {
        if looks_like_html(&text) {
            if html.is_none() {
                html = Some(text.clone());
            }
            html_to_text(&text)
        } else {
            text
        }
    });
    (text, html)
}

/// Whether a `text/plain` body is really HTML. Some mailers ship their HTML in
/// the text part; this is deliberately conservative so ordinary plain text that
/// merely contains `<` is left alone.
fn looks_like_html(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const MARKERS: [&str; 10] = [
        "<html", "<body", "<head", "<table", "<div", "<style", "<!doctype", "</p>", "<br", "<p>",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Flatten an HTML fragment into readable plain text: comments and the contents
/// of `<script>`/`<style>` are dropped, tags become whitespace, and common HTML
/// entities are decoded.
fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    while cursor < html.len() {
        let Some(rel) = lower[cursor..].find('<') else {
            out.push_str(&html[cursor..]);
            break;
        };
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        // Comments: skip to the end marker.
        if lower[start..].starts_with("<!--") {
            cursor = lower[start..]
                .find("-->")
                .map(|rel| start + rel + 3)
                .unwrap_or(html.len());
            continue;
        }
        let Some(gt) = lower[start..].find('>').map(|rel| start + rel + 1) else {
            break;
        };
        // `<script>`/`<style>`: drop the element and everything inside it.
        if let Some(name) = raw_text_tag(&lower[start..gt]) {
            let close = format!("</{name}");
            cursor = lower[gt..]
                .find(&close)
                .and_then(|rel| lower[gt + rel..].find('>').map(|gt2| gt + rel + gt2 + 1))
                .unwrap_or(gt);
            out.push(' ');
            continue;
        }
        // Any other tag is a boundary; its name carries no readable text.
        out.push(' ');
        cursor = gt;
    }
    decode_entities(&out)
}

/// If `tag` (the lowercased slice from `<` through `>`) opens a raw-text element
/// whose contents must be discarded, return its name.
fn raw_text_tag(tag: &str) -> Option<&'static str> {
    let name = tag
        .trim_start_matches('<')
        .trim_start_matches('/')
        .split(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/')
        .next()
        .unwrap_or("");
    match name {
        "script" => Some("script"),
        "style" => Some("style"),
        _ => None,
    }
}

/// Decode the HTML entities that commonly appear in previewed text.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        if let Some(end) = rest.find(';').filter(|end| *end <= 12) {
            if let Some(decoded) = decode_entity(&rest[1..end]) {
                out.push_str(&decoded);
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    let named = match entity {
        "nbsp" => " ",
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "mdash" => "\u{2014}",
        "ndash" => "\u{2013}",
        "hellip" => "\u{2026}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "ldquo" => "\u{201c}",
        "rdquo" => "\u{201d}",
        "copy" => "\u{a9}",
        _ => return decode_numeric_entity(entity),
    };
    Some(named.to_string())
}

fn decode_numeric_entity(entity: &str) -> Option<String> {
    let digits = entity.strip_prefix('#')?;
    let code = if let Some(hex) = digits.strip_prefix('x').or_else(|| digits.strip_prefix('X')) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        digits.parse::<u32>().ok()?
    };
    char::from_u32(code).map(String::from)
}

fn protocol(error: impl std::fmt::Display) -> Error {
    Error::Protocol(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_html_in_text_parts() {
        assert!(looks_like_html("<p></p><p>hello</p>"));
        assert!(looks_like_html("<table><tr><td>x</td></tr></table>"));
        assert!(!looks_like_html("Just a plain message.\n\nRegards,\nLuke"));
        assert!(!looks_like_html("Cost is 3 < 5 dollars"));
    }

    #[test]
    fn flattens_html_bodies_to_text() {
        let html = "<p></p>\n\
            <p> <!--[if mso]><xml><o:PixelsPerInch>96</o:PixelsPerInch></xml><![endif]--></p>\n\
            <style>\n* { box-sizing: border-box; }\nbody { margin: 0; }\n</style>\n\
            <p>Hello &amp; welcome, Luke&nbsp;!</p><br><div>Second line</div><!-- a comment -->";
        let text = html_to_text(html);
        assert!(!text.contains('<'), "tags should be gone: {text:?}");
        assert!(!text.contains("box-sizing"), "style contents: {text:?}");
        assert!(!text.contains("PixelsPerInch"), "comments: {text:?}");
        assert!(text.contains("Hello & welcome, Luke !"), "{text:?}");
        assert!(text.contains("Second line"), "{text:?}");

        let snippet = snippet_of(&text);
        assert!(!snippet.contains('<'));
        assert!(snippet.contains("Hello & welcome, Luke !"));
    }

    #[test]
    fn decodes_entities_in_flattened_text() {
        assert_eq!(html_to_text("a&amp;b"), "a&b");
        assert_eq!(html_to_text("&#39;quoted&#39;"), "'quoted'");
        assert_eq!(html_to_text("&#x2014;"), "\u{2014}");
        // An '&' that is not an entity, or an unknown one, is left alone.
        assert_eq!(html_to_text("A & B"), "A & B");
        assert_eq!(html_to_text("caf&eacute;"), "caf&eacute;");
    }

    #[test]
    fn promotes_html_in_text_when_there_is_no_html_part() {
        // No HTML part: the mislabelled text is promoted and flattened.
        let (text, html) = prepare_body(Some("<p>Hi</p>".to_string()), None);
        assert_eq!(html.as_deref(), Some("<p>Hi</p>"));
        assert_eq!(text.as_deref().map(str::trim), Some("Hi"));

        // A real HTML part wins; the text part is still cleaned.
        let (text, html) = prepare_body(
            Some("<p>Hi</p>".to_string()),
            Some("<b>Body</b>".to_string()),
        );
        assert_eq!(html.as_deref(), Some("<b>Body</b>"));
        assert_eq!(text.as_deref().map(str::trim), Some("Hi"));

        // Plain text is left byte-for-byte.
        let (text, html) = prepare_body(Some("plain  text".to_string()), None);
        assert_eq!(text.as_deref(), Some("plain  text"));
        assert!(html.is_none());
    }
}
