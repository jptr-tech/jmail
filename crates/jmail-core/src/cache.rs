//! Local SQLite cache.
//!
//! The cache is the single source of truth for the UI: the sync engine writes
//! folders and messages into it and the UI reads from it, so the frontend never
//! blocks on the network.
//!
//! Access is serialised through a `Mutex` around a single connection. Reads are
//! indexed and writes are small, so this stays off the critical path; WAL mode
//! keeps concurrent readers happy once the sync engine lands.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::account::{Account, AccountId, AuthKind, Provider, Security};
use crate::error::{Error, Result};
use crate::folder::{Folder, FolderId, SpecialUse};
use crate::message::{Address, Attachment, Flags, Message, MessageBody, MessageId, NewMessage};

/// Bump when the schema changes and add a matching migration step.
const SCHEMA_VERSION: i64 = 4;

/// A conversation as shown in the unified inbox: the latest message plus
/// aggregate counts.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub message: Message,
    pub count: u32,
    pub unread: u32,
    pub starred: u32,
}

/// Handle to the on-disk SQLite cache. Cheap to clone; all clones share one
/// connection.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Open (or create) the cache at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(storage)?;
        Self::from_connection(conn)
    }

    /// Open a transient in-memory cache (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(storage)?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL is unsupported for in-memory databases; ignore that failure.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(storage)?;
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| Error::Storage("database lock poisoned".into()))
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock()?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(storage)?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1).map_err(storage)?;
        }
        if version < 2 {
            conn.execute_batch(SCHEMA_V2).map_err(storage)?;
        }
        if version < 3 {
            conn.execute_batch(SCHEMA_V3).map_err(storage)?;
        }
        if version < 4 {
            conn.execute_batch(SCHEMA_V4).map_err(storage)?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(storage)?;
        Ok(())
    }

    // ---- Accounts ----------------------------------------------------------

    pub fn insert_account(&self, account: &Account) -> Result<AccountId> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO accounts
                (name, email, display_name, provider, auth, username,
                 imap_host, imap_port, imap_security, smtp_host, smtp_port,
                 smtp_security, color, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                account.name,
                account.email,
                account.display_name,
                account.provider.to_i64(),
                account.auth.to_i64(),
                account.username,
                account.imap_host,
                account.imap_port as i64,
                account.imap_security.to_i64(),
                account.smtp_host,
                account.smtp_port as i64,
                account.smtp_security.to_i64(),
                account.color,
                account.enabled as i64,
            ],
        )
        .map_err(storage)?;
        Ok(AccountId(conn.last_insert_rowid()))
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, email, display_name, provider, auth, username,
                        imap_host, imap_port, imap_security, smtp_host, smtp_port,
                        smtp_security, color, enabled
                 FROM accounts ORDER BY id",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map([], map_account)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    pub fn account_count(&self) -> Result<i64> {
        let conn = self.lock()?;
        conn.query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .map_err(storage)
    }

    // ---- Folders -----------------------------------------------------------

    pub fn upsert_folder(&self, folder: &Folder) -> Result<FolderId> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO folders
                (account_id, name, path, special_use, unread, total, uidvalidity, uidnext, last_uid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(account_id, path) DO UPDATE SET
                name = excluded.name,
                special_use = excluded.special_use,
                unread = excluded.unread,
                total = excluded.total,
                uidvalidity = excluded.uidvalidity,
                uidnext = excluded.uidnext",
            params![
                folder.account_id.0,
                folder.name,
                folder.path,
                folder.special_use.to_i64(),
                folder.unread as i64,
                folder.total as i64,
                folder.uidvalidity as i64,
                folder.uidnext as i64,
                folder.last_uid as i64,
            ],
        )
        .map_err(storage)?;
        let id: i64 = conn
            .query_row(
                "SELECT id FROM folders WHERE account_id = ?1 AND path = ?2",
                params![folder.account_id.0, folder.path],
                |row| row.get(0),
            )
            .map_err(storage)?;
        Ok(FolderId(id))
    }

    pub fn folders(&self, account: AccountId) -> Result<Vec<Folder>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, account_id, name, path, special_use, unread, total, uidvalidity, uidnext, last_uid
                 FROM folders WHERE account_id = ?1
                 ORDER BY special_use, name COLLATE NOCASE",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![account.0], map_folder)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// Update sync bookkeeping for a folder after selecting/fetching it.
    pub fn set_folder_state(
        &self,
        folder: FolderId,
        uidvalidity: u32,
        uidnext: u32,
        last_uid: u32,
        total: u32,
        unread: u32,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE folders SET uidvalidity = ?1, uidnext = ?2, last_uid = ?3, total = ?4, unread = ?5
             WHERE id = ?6",
            params![
                uidvalidity as i64,
                uidnext as i64,
                last_uid as i64,
                total as i64,
                unread as i64,
                folder.0
            ],
        )
        .map_err(storage)?;
        Ok(())
    }

    pub fn folder(&self, id: FolderId) -> Result<Option<Folder>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, account_id, name, path, special_use, unread, total, uidvalidity, uidnext, last_uid
                 FROM folders WHERE id = ?1",
            )
            .map_err(storage)?;
        let folder = stmt
            .query_map(params![id.0], map_folder)
            .map_err(storage)?
            .next()
            .transpose()
            .map_err(storage)?;
        Ok(folder)
    }

    // ---- Messages ----------------------------------------------------------

    pub fn upsert_message(&self, message: &NewMessage) -> Result<MessageId> {
        let conn = self.lock()?;
        let to_json = serde_json::to_string(&message.to).unwrap_or_else(|_| "[]".into());
        let cc_json = serde_json::to_string(&message.cc).unwrap_or_else(|_| "[]".into());
        let refs = message.references.join(" ");
        conn.execute(
            "INSERT INTO messages
                (account_id, folder_id, uid, message_id, thread_id, subject,
                 from_name, from_email, to_json, cc_json, date, flags, snippet,
                 has_attachments, body_text, body_html, in_reply_to, refs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(folder_id, uid) DO UPDATE SET
                message_id = excluded.message_id,
                thread_id = excluded.thread_id,
                in_reply_to = excluded.in_reply_to,
                refs = excluded.refs,
                subject = excluded.subject,
                from_name = excluded.from_name,
                from_email = excluded.from_email,
                to_json = excluded.to_json,
                cc_json = excluded.cc_json,
                date = excluded.date,
                flags = excluded.flags,
                snippet = excluded.snippet,
                has_attachments = excluded.has_attachments,
                body_text = COALESCE(excluded.body_text, messages.body_text),
                body_html = COALESCE(excluded.body_html, messages.body_html)",
            params![
                message.account_id.0,
                message.folder_id.0,
                message.uid as i64,
                message.message_id,
                message.thread_id,
                message.subject,
                message.from.name,
                message.from.email,
                to_json,
                cc_json,
                message.date.unix_timestamp(),
                message.flags.to_i64(),
                message.snippet,
                message.has_attachments as i64,
                message.body_text,
                message.body_html,
                message.in_reply_to,
                refs,
            ],
        )
        .map_err(storage)?;
        let id: i64 = conn
            .query_row(
                "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
                params![message.folder_id.0, message.uid],
                |row| row.get(0),
            )
            .map_err(storage)?;
        Ok(MessageId(id))
    }

    pub fn messages_in_folder(&self, folder: FolderId, limit: u32) -> Result<Vec<Message>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.folder_id = ?1
                 ORDER BY m.date DESC, m.id DESC LIMIT ?2"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![folder.0, limit], map_message)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// Every account's Inbox, newest first.
    pub fn unified_inbox(&self, limit: u32) -> Result<Vec<Message>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages m
                 JOIN folders f ON f.id = m.folder_id
                 WHERE f.special_use = ?1
                 ORDER BY m.date DESC, m.id DESC LIMIT ?2"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![SpecialUse::Inbox.to_i64(), limit], map_message)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// One row per conversation in the unified inbox: the latest message plus
    /// aggregate counts.
    pub fn unified_inbox_threads(&self, limit: u32) -> Result<Vec<ThreadSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "WITH ranked AS (
                    SELECT {MESSAGE_COLUMNS},
                           ROW_NUMBER() OVER (PARTITION BY m.account_id, m.thread_id
                                              ORDER BY m.date DESC, m.id DESC) AS rn,
                           COUNT(*) OVER (PARTITION BY m.account_id, m.thread_id) AS total,
                           SUM(CASE WHEN (m.flags & 1) = 0 THEN 1 ELSE 0 END)
                               OVER (PARTITION BY m.account_id, m.thread_id) AS unread,
                           SUM(CASE WHEN (m.flags & 4) != 0 THEN 1 ELSE 0 END)
                               OVER (PARTITION BY m.account_id, m.thread_id) AS starred
                    FROM messages m
                    JOIN folders f ON f.id = m.folder_id
                    WHERE f.special_use = ?1
                 )
                 SELECT * FROM ranked WHERE rn = 1 ORDER BY date DESC, id DESC LIMIT ?2"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![SpecialUse::Inbox.to_i64(), limit], |row| {
                Ok(ThreadSummary {
                    message: map_message(row)?,
                    count: row.get::<_, i64>(18)? as u32,
                    unread: row.get::<_, i64>(19)? as u32,
                    starred: row.get::<_, i64>(20)? as u32,
                })
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// Every message in a conversation with its server location:
    /// `(id, folder, path, uid, flags)`, oldest first.
    pub fn thread_locations(
        &self,
        account: AccountId,
        thread_id: &str,
    ) -> Result<Vec<(MessageId, FolderId, String, u32, Flags)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT m.id, m.folder_id, f.path, m.uid, m.flags
                 FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE m.account_id = ?1 AND m.thread_id = ?2
                 ORDER BY m.date ASC, m.id ASC",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![account.0, thread_id], |row| {
                Ok((
                    MessageId(row.get(0)?),
                    FolderId(row.get(1)?),
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? as u32,
                    Flags::from_i64(row.get(4)?),
                ))
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// Set or clear `\Seen` on every message in a conversation. Returns the
    /// affected ids.
    pub fn set_thread_seen(
        &self,
        account: AccountId,
        thread_id: &str,
        seen: bool,
    ) -> Result<Vec<MessageId>> {
        self.set_thread_bit(account, thread_id, Flags::SEEN, seen)
    }

    /// Set or clear `\Flagged` on every message in a conversation.
    pub fn set_thread_flagged(
        &self,
        account: AccountId,
        thread_id: &str,
        flagged: bool,
    ) -> Result<Vec<MessageId>> {
        self.set_thread_bit(account, thread_id, Flags::FLAGGED, flagged)
    }

    fn set_thread_bit(
        &self,
        account: AccountId,
        thread_id: &str,
        bit: i64,
        value: bool,
    ) -> Result<Vec<MessageId>> {
        let ids: Vec<MessageId> = {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare("SELECT id FROM messages WHERE account_id = ?1 AND thread_id = ?2")
                .map_err(storage)?;
            let rows = stmt
                .query_map(params![account.0, thread_id], |row| {
                    Ok(MessageId(row.get(0)?))
                })
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            rows
        };
        if ids.is_empty() {
            return Ok(ids);
        }
        let sql = if value {
            format!("UPDATE messages SET flags = flags | {bit} WHERE id = ?1")
        } else {
            format!("UPDATE messages SET flags = flags & ~{bit} WHERE id = ?1")
        };
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql).map_err(storage)?;
        for id in &ids {
            stmt.execute(params![id.0]).map_err(storage)?;
        }
        Ok(ids)
    }

    /// Full-text search across subject, sender and body.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<Message>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        if let Some(match_query) = fts_query(query) {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages m
                     JOIN messages_fts fts ON fts.rowid = m.id
                     WHERE messages_fts MATCH ?1
                     ORDER BY m.date DESC, m.id DESC LIMIT ?2"
                ))
                .map_err(storage)?;
            if let Ok(rows) = stmt
                .query_map(params![match_query, limit], map_message)
                .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
            {
                return Ok(rows);
            }
        }
        // Fallback: simple LIKE over subject and sender.
        let like = format!("%{query}%");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages m
                 WHERE m.subject LIKE ?1 OR m.from_name LIKE ?1 OR m.from_email LIKE ?1
                 ORDER BY m.date DESC, m.id DESC LIMIT ?2"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![like, limit], map_message)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    pub fn set_flags(&self, id: MessageId, flags: Flags) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE messages SET flags = ?1 WHERE id = ?2",
            params![flags.to_i64(), id.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    pub fn delete_message(&self, id: MessageId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM messages WHERE id = ?1", params![id.0])
            .map_err(storage)?;
        Ok(())
    }

    /// All messages in a conversation, oldest first (Gmail-style order).
    pub fn thread_messages(
        &self,
        account: AccountId,
        thread_id: &str,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages m
                 WHERE m.account_id = ?1 AND m.thread_id = ?2
                 ORDER BY m.date ASC, m.id ASC LIMIT ?3"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![account.0, thread_id, limit], map_message)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// Recompute `thread_id` for every message in an account from the cached
    /// `Message-ID`/`In-Reply-To`/`References` graph. Returns the number of rows
    /// whose conversation changed.
    pub fn rethread_account(&self, account: AccountId) -> Result<usize> {
        let inputs: Vec<crate::threading::ThreadInput> = {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, message_id, in_reply_to, refs, subject
                     FROM messages WHERE account_id = ?1",
                )
                .map_err(storage)?;
            let rows = stmt
                .query_map(params![account.0], |row| {
                    let refs: String = row.get(3)?;
                    Ok(crate::threading::ThreadInput {
                        id: row.get(0)?,
                        message_id: row.get(1)?,
                        in_reply_to: row.get(2)?,
                        references: refs.split_whitespace().map(str::to_string).collect(),
                        subject: row.get(4)?,
                    })
                })
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            rows
        };
        if inputs.is_empty() {
            return Ok(0);
        }

        let assignment = crate::threading::assign_threads(&inputs);
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("UPDATE messages SET thread_id = ?1 WHERE id = ?2 AND thread_id <> ?1")
            .map_err(storage)?;
        let mut changed = 0usize;
        for input in &inputs {
            let Some(thread) = assignment.get(&input.id) else {
                continue;
            };
            changed += stmt.execute(params![thread, input.id]).map_err(storage)?;
        }
        Ok(changed)
    }

    /// Recompute conversations for every account. Cheap when nothing changed
    /// (only rows whose thread actually moved are written), so it is safe to
    /// call on startup to heal caches written before threading existed.
    pub fn rethread_all(&self) -> Result<usize> {
        let accounts = self.accounts()?;
        let mut changed = 0;
        for account in accounts {
            changed += self.rethread_account(account.id)?;
        }
        Ok(changed)
    }

    /// Fetch the cached body of a message, if present.
    pub fn message_body(&self, id: MessageId) -> Result<Option<MessageBody>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT body_text, body_html FROM messages WHERE id = ?1",
            params![id.0],
            |row| {
                Ok(MessageBody {
                    text: row.get(0)?,
                    html: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(storage)
    }

    pub fn move_message(&self, id: MessageId, folder: FolderId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE messages SET folder_id = ?1 WHERE id = ?2",
            params![folder.0, id.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    // ---- Settings ----------------------------------------------------------

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(storage)?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)
    }

    // ---- Account / message lookups (used by the sync engine) ---------------

    pub fn account(&self, id: AccountId) -> Result<Option<Account>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, email, display_name, provider, auth, username,
                        imap_host, imap_port, imap_security, smtp_host, smtp_port,
                        smtp_security, color, enabled
                 FROM accounts WHERE id = ?1",
            )
            .map_err(storage)?;
        let account = stmt
            .query_map(params![id.0], map_account)
            .map_err(storage)?
            .next()
            .transpose()
            .map_err(storage)?;
        Ok(account)
    }

    pub fn remove_account(&self, id: AccountId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM accounts WHERE id = ?1", params![id.0])
            .map_err(storage)?;
        Ok(())
    }

    /// Where a cached message lives on the server: `(account, folder path, folder id, uid, flags)`.
    pub fn message_location(&self, id: MessageId) -> Result<Option<(AccountId, String, FolderId, u32, Flags)>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.account_id, f.path, m.folder_id, m.uid, m.flags
                 FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1",
                params![id.0],
                |row| {
                    Ok((
                        AccountId(row.get(0)?),
                        row.get(1)?,
                        FolderId(row.get(2)?),
                        row.get::<_, i64>(3)? as u32,
                        Flags::from_i64(row.get(4)?),
                    ))
                },
            )
            .optional()
            .map_err(storage)?;
        Ok(row)
    }

    pub fn get_message(&self, id: MessageId) -> Result<Option<Message>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.id = ?1"
            ))
            .map_err(storage)?;
        let message = stmt
            .query_map(params![id.0], map_message)
            .map_err(storage)?
            .next()
            .transpose()
            .map_err(storage)?;
        Ok(message)
    }

    /// Store a fetched body (and derived snippet) for a cached message.
    pub fn set_body(
        &self,
        id: MessageId,
        text: Option<&str>,
        html: Option<&str>,
        snippet: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE messages SET body_text = ?1, body_html = ?2, snippet = ?3 WHERE id = ?4",
            params![text, html, snippet, id.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    /// Replace the cached attachment metadata for a message and update its
    /// `has_attachments` flag.
    pub fn set_attachments(&self, id: MessageId, attachments: &[Attachment]) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM attachments WHERE message_id = ?1",
            params![id.0],
        )
        .map_err(storage)?;
        for attachment in attachments {
            conn.execute(
                "INSERT INTO attachments
                    (message_id, filename, mime, size, content_id, inline, part)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id.0,
                    attachment.filename,
                    attachment.mime,
                    attachment.size as i64,
                    attachment.content_id,
                    attachment.inline as i64,
                    attachment.part,
                ],
            )
            .map_err(storage)?;
        }
        conn.execute(
            "UPDATE messages SET has_attachments = ?1 WHERE id = ?2",
            params![(!attachments.is_empty()) as i64, id.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    /// Cached attachment metadata for a message, in part order.
    pub fn attachments(&self, id: MessageId) -> Result<Vec<Attachment>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT filename, mime, size, content_id, inline, part
                 FROM attachments WHERE message_id = ?1 ORDER BY part, id",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![id.0], |row| {
                Ok(Attachment {
                    filename: row.get(0)?,
                    mime: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    content_id: row.get(3)?,
                    inline: row.get::<_, i64>(4)? != 0,
                    part: row.get(5)?,
                })
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    /// Drop cached messages for a folder whose UIDVALIDITY changed.
    pub fn clear_folder_messages(&self, folder: FolderId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM messages WHERE folder_id = ?1",
            params![folder.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    /// Drop every cached message for an account (FTS index kept in sync by triggers).
    pub fn clear_account_messages(&self, account: AccountId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM messages WHERE account_id = ?1",
            params![account.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    /// Zero per-folder sync bookkeeping so the next sync re-fetches everything.
    pub fn reset_folder_sync_state(&self, account: AccountId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE folders
             SET uidvalidity = 0, uidnext = 0, last_uid = 0, total = 0, unread = 0
             WHERE account_id = ?1",
            params![account.0],
        )
        .map_err(storage)?;
        Ok(())
    }

    // ---- Secrets (dev plaintext fallback; real creds live in the keyring) --

    pub fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO secrets (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(storage)?;
        Ok(())
    }

    pub fn get_secret(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT value FROM secrets WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)
    }

    pub fn delete_secret(&self, key: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM secrets WHERE key = ?1", params![key])
            .map_err(storage)?;
        Ok(())
    }

    // ---- Development seed --------------------------------------------------

    /// Populate a couple of fake accounts so the UI has data without a server.
    /// No-op if any account already exists.
    pub fn seed_demo(&self) -> Result<()> {
        if self.account_count()? > 0 {
            return Ok(());
        }

        let google = self.insert_account(&Account::google("you@gmail.com"))?;
        let generic = {
            let mut account = Account::generic("you@example.com");
            account.name = "Example Mail".to_string();
            account.imap_host = "imap.example.com".to_string();
            account.smtp_host = "smtp.example.com".to_string();
            self.insert_account(&account)?
        };

        let inbox_google = self.seed_folders(google, true)?;
        let inbox_generic = self.seed_folders(generic, false)?;

        let now = OffsetDateTime::now_utc();
        let google_messages = [
            ("Ada Lovelace", "ada@example.org", "Re: Welcome to Jmail", true, false, 1),
            ("Grace Hopper", "grace@example.org", "Compiler notes for next week", true, false, 3),
            ("Margaret Hamilton", "margaret@example.org", "Apollo guidance review", false, true, 26),
            ("Linus Torvalds", "linus@example.org", "Re: kernel patch", false, false, 50),
        ];
        for (i, (name, email, subject, unread, starred, hours)) in google_messages.iter().enumerate() {
            self.seed_message(
                google,
                inbox_google,
                (i + 1) as u32,
                name,
                email,
                subject,
                now - time::Duration::hours(*hours),
                *unread,
                *starred,
            )?;
        }

        let generic_messages = [
            ("Team Standup", "standup@example.com", "Notes from Monday standup", true, false, 5),
            ("Billing", "billing@example.com", "Your invoice is ready", false, false, 30),
            ("Rust Weekly", "news@example.com", "This week in Rust", false, false, 72),
        ];
        for (i, (name, email, subject, unread, starred, hours)) in generic_messages.iter().enumerate() {
            self.seed_message(
                generic,
                inbox_generic,
                (i + 1) as u32,
                name,
                email,
                subject,
                now - time::Duration::hours(*hours),
                *unread,
                *starred,
            )?;
        }
        Ok(())
    }

    fn seed_folders(&self, account: AccountId, google: bool) -> Result<FolderId> {
        let folders = [
            ("Inbox", SpecialUse::Inbox),
            ("Starred", SpecialUse::None),
            ("Sent", SpecialUse::Sent),
            ("Drafts", SpecialUse::Drafts),
            ("Archive", SpecialUse::Archive),
            ("Trash", SpecialUse::Trash),
            ("Spam", SpecialUse::Spam),
        ];
        let mut inbox = FolderId(0);
        for (name, special_use) in folders {
            let path = if google {
                format!("[Gmail]/{}", if special_use == SpecialUse::Inbox { "Inbox" } else { name })
            } else {
                name.to_string()
            };
            let id = self.upsert_folder(&Folder {
                id: FolderId(0),
                account_id: account,
                name: name.to_string(),
                path,
                special_use,
                unread: 0,
                total: 0,
                uidvalidity: 1,
                uidnext: 1,
                last_uid: 0,
            })?;
            if special_use == SpecialUse::Inbox {
                inbox = id;
            }
        }
        Ok(inbox)
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_message(
        &self,
        account: AccountId,
        folder: FolderId,
        uid: u32,
        name: &str,
        email: &str,
        subject: &str,
        date: OffsetDateTime,
        unread: bool,
        starred: bool,
    ) -> Result<MessageId> {
        self.upsert_message(&NewMessage {
            account_id: account,
            folder_id: folder,
            uid,
            message_id: format!("seed-{}-{uid}@jmail", account.0),
            thread_id: format!("seed-thread-{uid}"),
            in_reply_to: String::new(),
            references: Vec::new(),
            subject: subject.to_string(),
            from: Address::new(name, email),
            to: vec![Address::new("", "you@example.com")],
            cc: Vec::new(),
            date,
            flags: Flags {
                seen: !unread,
                flagged: starred,
                ..Flags::default()
            },
            snippet: format!("A short preview of “{subject}”."),
            has_attachments: false,
            body_text: Some(format!("{name} wrote:\n\n{subject}\n\nThis is seeded demo content.")),
            body_html: None,
        })
    }
}

/// Build an FTS5 MATCH expression: each token becomes a quoted prefix term.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"*")
        })
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn storage(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}

const MESSAGE_COLUMNS: &str = "m.id, m.account_id, m.folder_id, m.uid, m.message_id, \
    m.thread_id, m.subject, m.from_name, m.from_email, m.to_json, m.cc_json, \
    m.date, m.flags, m.snippet, m.has_attachments, m.in_reply_to, m.refs";

fn map_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: AccountId(row.get(0)?),
        name: row.get(1)?,
        email: row.get(2)?,
        display_name: row.get(3)?,
        provider: Provider::from_i64(row.get(4)?),
        auth: AuthKind::from_i64(row.get(5)?),
        username: row.get(6)?,
        imap_host: row.get(7)?,
        imap_port: row.get::<_, i64>(8)? as u16,
        imap_security: Security::from_i64(row.get(9)?),
        smtp_host: row.get(10)?,
        smtp_port: row.get::<_, i64>(11)? as u16,
        smtp_security: Security::from_i64(row.get(12)?),
        color: row.get(13)?,
        enabled: row.get::<_, i64>(14)? != 0,
    })
}

fn map_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: FolderId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        name: row.get(2)?,
        path: row.get(3)?,
        special_use: SpecialUse::from_i64(row.get(4)?),
        unread: row.get::<_, i64>(5)? as u32,
        total: row.get::<_, i64>(6)? as u32,
        uidvalidity: row.get::<_, i64>(7)? as u32,
        uidnext: row.get::<_, i64>(8)? as u32,
        last_uid: row.get::<_, i64>(9)? as u32,
    })
}

fn map_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let to_json: String = row.get(9)?;
    let cc_json: String = row.get(10)?;
    let date: i64 = row.get(11)?;
    let refs: String = row.get(16)?;
    Ok(Message {
        id: MessageId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        folder_id: FolderId(row.get(2)?),
        uid: row.get::<_, i64>(3)? as u32,
        message_id: row.get(4)?,
        thread_id: row.get(5)?,
        subject: row.get(6)?,
        from: Address {
            name: row.get(7)?,
            email: row.get(8)?,
        },
        to: serde_json::from_str(&to_json).unwrap_or_default(),
        cc: serde_json::from_str(&cc_json).unwrap_or_default(),
        date: OffsetDateTime::from_unix_timestamp(date).unwrap_or(OffsetDateTime::UNIX_EPOCH),
        flags: Flags::from_i64(row.get(12)?),
        snippet: row.get(13)?,
        has_attachments: row.get::<_, i64>(14)? != 0,
        in_reply_to: row.get(15)?,
        references: refs.split_whitespace().map(str::to_string).collect(),
    })
}

const SCHEMA_V1: &str = r#"
CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL,
    email         TEXT NOT NULL,
    display_name  TEXT NOT NULL DEFAULT '',
    provider      INTEGER NOT NULL DEFAULT 1,
    auth          INTEGER NOT NULL DEFAULT 0,
    username      TEXT NOT NULL,
    imap_host     TEXT NOT NULL,
    imap_port     INTEGER NOT NULL DEFAULT 993,
    imap_security INTEGER NOT NULL DEFAULT 0,
    smtp_host     TEXT NOT NULL,
    smtp_port     INTEGER NOT NULL DEFAULT 465,
    smtp_security INTEGER NOT NULL DEFAULT 0,
    color         TEXT NOT NULL DEFAULT '#3584e4',
    enabled       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE folders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    path        TEXT NOT NULL,
    special_use INTEGER NOT NULL DEFAULT 7,
    unread      INTEGER NOT NULL DEFAULT 0,
    total       INTEGER NOT NULL DEFAULT 0,
    uidvalidity INTEGER NOT NULL DEFAULT 0,
    uidnext     INTEGER NOT NULL DEFAULT 0,
    UNIQUE(account_id, path)
);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id       INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid             INTEGER NOT NULL,
    message_id      TEXT NOT NULL DEFAULT '',
    thread_id       TEXT NOT NULL DEFAULT '',
    subject         TEXT NOT NULL DEFAULT '',
    from_name       TEXT NOT NULL DEFAULT '',
    from_email      TEXT NOT NULL DEFAULT '',
    to_json         TEXT NOT NULL DEFAULT '[]',
    cc_json         TEXT NOT NULL DEFAULT '[]',
    date            INTEGER NOT NULL DEFAULT 0,
    flags           INTEGER NOT NULL DEFAULT 0,
    snippet         TEXT NOT NULL DEFAULT '',
    has_attachments INTEGER NOT NULL DEFAULT 0,
    body_text       TEXT,
    body_html       TEXT,
    UNIQUE(folder_id, uid)
);

CREATE INDEX idx_messages_folder ON messages(folder_id, date DESC);
CREATE INDEX idx_messages_account_date ON messages(account_id, date DESC);
CREATE INDEX idx_messages_thread ON messages(thread_id);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject, from_name, from_email, body_text,
    content='messages', content_rowid='id'
);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, subject, from_name, from_email, body_text)
    VALUES (new.id, new.subject, new.from_name, new.from_email, new.body_text);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_email, body_text)
    VALUES ('delete', old.id, old.subject, old.from_name, old.from_email, old.body_text);
END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_email, body_text)
    VALUES ('delete', old.id, old.subject, old.from_name, old.from_email, old.body_text);
    INSERT INTO messages_fts(rowid, subject, from_name, from_email, body_text)
    VALUES (new.id, new.subject, new.from_name, new.from_email, new.body_text);
END;

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const SCHEMA_V2: &str = r#"
ALTER TABLE folders ADD COLUMN last_uid INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS secrets (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Threading headers: `References` is stored as space-separated message-ids
/// (the column is named `refs` because `references` is a SQL keyword).
const SCHEMA_V3: &str = r#"
ALTER TABLE messages ADD COLUMN in_reply_to TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN refs TEXT NOT NULL DEFAULT '';
"#;

/// Attachment *metadata* (not the contents, which are re-fetched on demand).
const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS attachments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename   TEXT NOT NULL DEFAULT '',
    mime       TEXT NOT NULL DEFAULT '',
    size       INTEGER NOT NULL DEFAULT 0,
    content_id TEXT,
    inline     INTEGER NOT NULL DEFAULT 0,
    part       TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_attachments_message ON attachments(message_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().expect("open in-memory db")
    }

    #[test]
    fn migrates_and_seeds() {
        let db = db();
        assert_eq!(db.account_count().unwrap(), 0);
        db.seed_demo().unwrap();
        assert_eq!(db.account_count().unwrap(), 2);
        // Seeding twice is a no-op.
        db.seed_demo().unwrap();
        assert_eq!(db.account_count().unwrap(), 2);
    }

    #[test]
    fn unified_inbox_is_sorted_newest_first() {
        let db = db();
        db.seed_demo().unwrap();
        let inbox = db.unified_inbox(100).unwrap();
        assert_eq!(inbox.len(), 7);
        for pair in inbox.windows(2) {
            assert!(pair[0].date >= pair[1].date);
        }
    }

    #[test]
    fn upsert_message_is_idempotent() {
        let db = db();
        db.seed_demo().unwrap();
        let before = db.unified_inbox(100).unwrap().len();
        let account = db.accounts().unwrap()[0].id;
        let folder = db.folders(account).unwrap()[0].id;
        let message = NewMessage {
            account_id: account,
            folder_id: folder,
            uid: 999,
            message_id: "m".into(),
            thread_id: "t".into(),
            in_reply_to: String::new(),
            references: Vec::new(),
            subject: "Hello world".into(),
            from: Address::new("Sender", "sender@example.org"),
            to: vec![],
            cc: vec![],
            date: OffsetDateTime::now_utc(),
            flags: Flags::default(),
            snippet: "hi".into(),
            has_attachments: false,
            body_text: None,
            body_html: None,
        };
        let first = db.upsert_message(&message).unwrap();
        let second = db.upsert_message(&message).unwrap();
        assert_eq!(first, second);
        assert_eq!(db.unified_inbox(100).unwrap().len(), before + 1);
    }

    #[test]
    fn rethreads_replies_into_one_conversation() {
        let db = db();
        db.seed_demo().unwrap();
        let account = db.accounts().unwrap()[0].id;
        let folder = db
            .folders(account)
            .unwrap()
            .into_iter()
            .find(|f| f.special_use == SpecialUse::Inbox)
            .unwrap()
            .id;
        let make = |uid: u32, message_id: &str, in_reply_to: &str, references: Vec<String>, subject: &str| {
            NewMessage {
                account_id: account,
                folder_id: folder,
                uid,
                message_id: message_id.into(),
                thread_id: String::new(),
                in_reply_to: in_reply_to.into(),
                references,
                subject: subject.into(),
                from: Address::new("Sender", "sender@example.org"),
                to: vec![],
                cc: vec![],
                date: OffsetDateTime::now_utc(),
                flags: Flags::default(),
                snippet: "hi".into(),
                has_attachments: false,
                body_text: None,
                body_html: None,
            }
        };
        let root = db
            .upsert_message(&make(900, "root@t", "", vec![], "Thread subject"))
            .unwrap();
        let reply = db
            .upsert_message(&make(
                901,
                "reply@t",
                "root@t",
                vec!["root@t".to_string()],
                "Re: Thread subject",
            ))
            .unwrap();

        let changed = db.rethread_account(account).unwrap();
        assert!(changed >= 2, "both new rows should be (re)assigned");
        assert_eq!(db.get_message(root).unwrap().unwrap().thread_id, "root@t");
        assert_eq!(db.get_message(reply).unwrap().unwrap().thread_id, "root@t");

        let thread = db.thread_messages(account, "root@t", 10).unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].id, root);
        assert_eq!(thread[1].id, reply);
    }

    #[test]
    fn unified_inbox_groups_conversations_and_flags_them() {
        let db = db();
        db.seed_demo().unwrap();
        let account = db.accounts().unwrap()[0].id;
        let folder = db
            .folders(account)
            .unwrap()
            .into_iter()
            .find(|f| f.special_use == SpecialUse::Inbox)
            .unwrap()
            .id;
        let make = |uid: u32, message_id: &str, in_reply_to: &str, seen: bool| NewMessage {
            account_id: account,
            folder_id: folder,
            uid,
            message_id: message_id.into(),
            thread_id: String::new(),
            in_reply_to: in_reply_to.into(),
            references: if in_reply_to.is_empty() {
                Vec::new()
            } else {
                vec![in_reply_to.to_string()]
            },
            subject: "Shared subject".into(),
            from: Address::new("Sender", "sender@example.org"),
            to: vec![],
            cc: vec![],
            date: OffsetDateTime::now_utc(),
            flags: Flags {
                seen,
                ..Flags::default()
            },
            snippet: "hi".into(),
            has_attachments: false,
            body_text: None,
            body_html: None,
        };
        db.upsert_message(&make(910, "conv-root@t", "", false)).unwrap();
        db.upsert_message(&make(911, "conv-reply@t", "conv-root@t", true)).unwrap();
        db.rethread_account(account).unwrap();

        let thread = db
            .unified_inbox_threads(200)
            .unwrap()
            .into_iter()
            .find(|t| t.message.thread_id == "conv-root@t")
            .expect("conversation row");
        assert_eq!(thread.count, 2);
        assert_eq!(thread.unread, 1);
        assert_eq!(thread.starred, 0);

        assert_eq!(
            db.set_thread_seen(account, "conv-root@t", true).unwrap().len(),
            2
        );
        let thread = db
            .unified_inbox_threads(200)
            .unwrap()
            .into_iter()
            .find(|t| t.message.thread_id == "conv-root@t")
            .unwrap();
        assert_eq!(thread.unread, 0);

        db.set_thread_flagged(account, "conv-root@t", true).unwrap();
        let thread = db
            .unified_inbox_threads(200)
            .unwrap()
            .into_iter()
            .find(|t| t.message.thread_id == "conv-root@t")
            .unwrap();
        assert_eq!(thread.starred, 2);
    }

    #[test]
    fn attachment_metadata_round_trips() {
        let db = db();
        db.seed_demo().unwrap();
        let message = db.unified_inbox(1).unwrap().remove(0);
        assert!(db.attachments(message.id).unwrap().is_empty());
        assert!(!db.get_message(message.id).unwrap().unwrap().has_attachments);

        let attachments = vec![
            Attachment {
                filename: "report.pdf".into(),
                mime: "application/pdf".into(),
                size: 1234,
                content_id: None,
                inline: false,
                part: "0".into(),
            },
            Attachment {
                filename: "logo.png".into(),
                mime: "image/png".into(),
                size: 42,
                content_id: Some("logo@x".into()),
                inline: true,
                part: "1".into(),
            },
        ];
        db.set_attachments(message.id, &attachments).unwrap();
        assert_eq!(db.attachments(message.id).unwrap(), attachments);
        assert!(db.get_message(message.id).unwrap().unwrap().has_attachments);

        db.set_attachments(message.id, &[]).unwrap();
        assert!(db.attachments(message.id).unwrap().is_empty());
        assert!(!db.get_message(message.id).unwrap().unwrap().has_attachments);
    }

    #[test]
    fn search_finds_by_subject() {
        let db = db();
        db.seed_demo().unwrap();
        let results = db.search("compiler", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].subject.contains("Compiler"));
    }

    #[test]
    fn flags_round_trip() {
        let db = db();
        db.seed_demo().unwrap();
        let message = db.unified_inbox(1).unwrap().remove(0);
        let flags = Flags {
            seen: true,
            answered: true,
            flagged: true,
            draft: false,
        };
        db.set_flags(message.id, flags).unwrap();
        let reloaded = db
            .messages_in_folder(message.folder_id, 100)
            .unwrap()
            .into_iter()
            .find(|m| m.id == message.id)
            .unwrap();
        assert_eq!(reloaded.flags, flags);
    }

    #[test]
    fn delete_and_move() {
        let db = db();
        db.seed_demo().unwrap();
        let account = db.accounts().unwrap()[0].id;
        let folders = db.folders(account).unwrap();
        let trash = folders
            .iter()
            .find(|f| f.special_use == SpecialUse::Trash)
            .unwrap()
            .id;
        let message = db.unified_inbox(1).unwrap().remove(0);
        db.move_message(message.id, trash).unwrap();
        assert!(db
            .messages_in_folder(trash, 10)
            .unwrap()
            .iter()
            .any(|m| m.id == message.id));
        db.delete_message(message.id).unwrap();
        assert!(!db
            .messages_in_folder(trash, 10)
            .unwrap()
            .iter()
            .any(|m| m.id == message.id));
    }

    #[test]
    fn settings_round_trip() {
        let db = db();
        assert_eq!(db.get_setting("theme").unwrap(), None);
        db.set_setting("theme", "dark").unwrap();
        assert_eq!(db.get_setting("theme").unwrap().as_deref(), Some("dark"));
    }
}
