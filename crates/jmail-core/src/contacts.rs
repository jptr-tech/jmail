//! Contact suggestions for compose.
//!
//! Two sources are merged, in this order of authority:
//!
//! 1. The GNOME address books, read from Evolution Data Server over D-Bus.
//!    Only cards with an `EMAIL` property are useful to us.
//! 2. Recipients from every Sent folder in the local cache, with usage and
//!    recency statistics from [`crate::cache::Db::sent_recipients`].
//!
//! Address-book names win over the names we saw on outgoing mail, but a Sent
//! address that is not in any address book is still suggested. The index lives
//! entirely in memory: contact data is never written to the mail cache or
//! logged.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::cache::Db;
use crate::error::{Error, Result};
use crate::message::Address;

/// One recipient seen in the user's Sent mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientStat {
    pub address: Address,
    /// How many messages in Sent included this address.
    pub uses: u32,
    /// Unix timestamp of the newest such message.
    pub last_used: i64,
}

/// Everything recipient autocomplete draws from.
#[derive(Debug, Clone, Default)]
pub struct ContactIndex {
    /// Recipients of messages in Sent folders, newest use first.
    pub history: Vec<RecipientStat>,
    /// Every email address in the GNOME address books.
    pub book: Vec<Address>,
}

impl ContactIndex {
    /// Load the Sent-recipient half of the index from the cache.
    pub fn load_history(db: &Db) -> Result<Vec<RecipientStat>> {
        db.sent_recipients()
    }

    /// Rank suggestions for the token currently being typed.
    ///
    /// `query` is matched against the display name, the full address and the
    /// local part. Every whitespace-separated term must match somewhere. The
    /// lower-cased addresses in `exclude` are never returned (they are already
    /// in the field).
    pub fn suggest(&self, query: &str, exclude: &[String], limit: usize) -> Vec<Address> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let terms: Vec<&str> = query.split_whitespace().collect();
        let excluded: HashSet<String> = exclude
            .iter()
            .map(|email| email.trim().to_lowercase())
            .filter(|email| !email.is_empty())
            .collect();

        struct Candidate {
            address: Address,
            uses: u32,
            last_used: i64,
            score: u32,
        }

        let mut merged: HashMap<String, Candidate> = HashMap::new();
        let mut insert = |address: Address, uses: u32, last_used: i64, from_book: bool| {
            let key = address.email.trim().to_lowercase();
            if key.is_empty() || excluded.contains(&key) {
                return;
            }
            match merged.get_mut(&key) {
                Some(existing) => {
                    // Address-book names are authoritative; otherwise only fill
                    // in a name the other source did not have.
                    if from_book {
                        if !address.name.trim().is_empty() {
                            existing.address.name = address.name;
                        }
                    } else if existing.address.name.trim().is_empty()
                        && !address.name.trim().is_empty()
                    {
                        existing.address.name = address.name;
                    }
                    existing.uses = existing.uses.max(uses);
                    existing.last_used = existing.last_used.max(last_used);
                }
                None => {
                    merged.insert(
                        key,
                        Candidate {
                            address,
                            uses,
                            last_used,
                            score: 0,
                        },
                    );
                }
            }
        };

        for address in &self.book {
            insert(address.clone(), 0, 0, true);
        }
        for stat in &self.history {
            insert(stat.address.clone(), stat.uses, stat.last_used, false);
        }

        let mut candidates: Vec<Candidate> = merged
            .into_values()
            .filter_map(|mut candidate| {
                candidate.score = match_score(&candidate.address, &query, &terms)?;
                Some(candidate)
            })
            .collect();
        candidates.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.uses.cmp(&a.uses))
                .then_with(|| b.last_used.cmp(&a.last_used))
                .then_with(|| {
                    a.address
                        .email
                        .to_lowercase()
                        .cmp(&b.address.email.to_lowercase())
                })
        });
        candidates
            .into_iter()
            .take(limit)
            .map(|candidate| candidate.address)
            .collect()
    }
}

fn match_score(address: &Address, query: &str, terms: &[&str]) -> Option<u32> {
    let name = address.name.to_lowercase();
    let email = address.email.to_lowercase();
    if !terms
        .iter()
        .all(|term| name.contains(term) || email.contains(term))
    {
        return None;
    }
    let local = email.split('@').next().unwrap_or("");
    let score = if email == query {
        4000
    } else if email.starts_with(query) {
        3200
    } else if name.starts_with(query) {
        2800
    } else if name.split_whitespace().any(|word| word.starts_with(query)) {
        2400
    } else if local.starts_with(query) {
        1800
    } else if name.contains(query) {
        1000
    } else if email.contains(query) {
        700
    } else {
        200
    };
    Some(score)
}

/// A recipient field token, in character (not byte) offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientToken {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// The comma/semicolon-separated token around `caret`.
///
/// Leading and trailing whitespace is trimmed from the returned range, so the
/// caller can replace just the address being typed.
pub fn recipient_token(text: &str, caret: usize) -> RecipientToken {
    let chars: Vec<char> = text.chars().collect();
    let caret = caret.min(chars.len());
    let is_separator = |ch: char| ch == ',' || ch == ';';

    let start = chars[..caret]
        .iter()
        .rposition(|&ch| is_separator(ch))
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = chars[caret..]
        .iter()
        .position(|&ch| is_separator(ch))
        .map(|index| caret + index)
        .unwrap_or(chars.len());

    let mut trimmed_start = start;
    while trimmed_start < end && chars[trimmed_start].is_whitespace() {
        trimmed_start += 1;
    }
    let mut trimmed_end = end;
    while trimmed_end > trimmed_start && chars[trimmed_end - 1].is_whitespace() {
        trimmed_end -= 1;
    }

    RecipientToken {
        text: chars[trimmed_start..trimmed_end].iter().collect(),
        start: trimmed_start,
        end: trimmed_end,
    }
}

/// Replace the token around `caret` with `value` and return the new text and
/// caret position. The caret always ends up after the separator, ready for the
/// next recipient; a `, ` is added when the token was last in the field.
pub fn accept_suggestion(text: &str, caret: usize, value: &str) -> (String, usize) {
    let token = recipient_token(text, caret);
    let chars: Vec<char> = text.chars().collect();
    let value_len = value.chars().count();
    let mut out: Vec<char> = Vec::with_capacity(chars.len() + value_len + 2);
    out.extend_from_slice(&chars[..token.start]);
    out.extend(value.chars());
    let after_value = out.len();

    let mut separator = token.end;
    while separator < chars.len() && chars[separator].is_whitespace() {
        separator += 1;
    }
    let has_separator = chars
        .get(separator)
        .is_some_and(|ch| *ch == ',' || *ch == ';');
    if has_separator {
        // Keep the existing separator and whatever follows it.
        out.extend_from_slice(&chars[token.end..]);
        // Skip the separator and the spaces after it, so the next keystroke
        // starts a new recipient rather than extending the one just accepted.
        let mut caret = separator + 1;
        while caret < chars.len() && chars[caret].is_whitespace() {
            caret += 1;
        }
        let caret = after_value + (caret - token.end);
        (out.into_iter().collect(), caret)
    } else {
        // The token was last in the field: start a fresh slot and drop any
        // trailing whitespace.
        out.push(',');
        out.push(' ');
        let caret = out.len();
        (out.into_iter().collect(), caret)
    }
}

/// How an address should be written into a recipient field. Names containing
/// separators would be split by [`crate::outgoing::parse_address_list`], so we
/// fall back to the bare address in that case.
pub fn insertable_address(address: &Address) -> String {
    let name = address.name.trim();
    let name_has_separator = name
        .chars()
        .any(|ch| matches!(ch, ',' | ';' | '<' | '>'));
    if name.is_empty() || name_has_separator {
        address.email.clone()
    } else {
        format!("{name} <{}>", address.email)
    }
}

/// Parse a batch of vCards (as returned by EDS) into addresses.
pub fn parse_vcards(cards: &[String]) -> Vec<Address> {
    cards
        .iter()
        .flat_map(|card| parse_vcard(card))
        .collect()
}

/// Parse one vCard into one [`Address`] per `EMAIL` property.
///
/// The function is deliberately forgiving: EDS address books contain cards
/// written by many clients (vCard 2.1, 3.0, 4.0), with folded lines, group
/// prefixes and quoted-printable values.
pub fn parse_vcard(card: &str) -> Vec<Address> {
    let normalized = card.replace("\r\n", "\n").replace('\r', "\n");
    let mut logical: Vec<String> = Vec::new();
    for line in normalized.split('\n') {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(previous) = logical.last_mut() {
                // A folded quoted-printable line ends in `=` as a soft break.
                // Drop it while joining, otherwise the first characters of the
                // continuation could be mistaken for a `=XX` escape.
                if previous.ends_with('=') && is_quoted_printable(previous) {
                    previous.pop();
                }
                previous.push_str(&line[1..]);
            }
        } else if !line.is_empty() {
            logical.push(line.to_string());
        }
    }

    let mut formatted_name = String::new();
    let mut structured_name = String::new();
    let mut organization = String::new();
    let mut emails: Vec<String> = Vec::new();

    for line in &logical {
        let Some((head, value)) = split_property(line) else {
            continue;
        };
        let property = head.rsplit('.').next().unwrap_or(head);
        let (name, params) = match property.split_once(';') {
            Some((name, params)) => (name, params),
            None => (property, ""),
        };
        let value = decode_quoted_printable(value, params);
        match name.to_ascii_uppercase().as_str() {
            "FN" if formatted_name.is_empty() => formatted_name = unescape(&value),
            "N" if structured_name.is_empty() => structured_name = format_structured_name(&value),
            "ORG" if organization.is_empty() => {
                organization = unescape(
                    split_escaped(&value, ';')
                        .first()
                        .copied()
                        .unwrap_or("")
                        .trim(),
                )
            }
            "EMAIL" => {
                let email = unescape(value.trim())
                    .trim_start_matches("mailto:")
                    .trim()
                    .to_string();
                if is_email(&email)
                    && !emails
                        .iter()
                        .any(|seen| seen.eq_ignore_ascii_case(&email))
                {
                    emails.push(email);
                }
            }
            _ => {}
        }
    }

    let name = if !formatted_name.trim().is_empty() {
        formatted_name.trim().to_string()
    } else if !structured_name.trim().is_empty() {
        structured_name.trim().to_string()
    } else {
        organization.trim().to_string()
    };
    emails
        .into_iter()
        .map(|email| Address::new(name.clone(), email))
        .collect()
}

/// Split a vCard content line at the first unquoted `:`. Quoted parameter
/// values may legally contain colons.
fn split_property(line: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            ':' if !quoted => return Some((&line[..index], &line[index + 1..])),
            _ => {}
        }
    }
    None
}

fn format_structured_name(value: &str) -> String {
    let parts = split_escaped(value, ';');
    let family = parts.first().copied().unwrap_or("");
    let given = parts.get(1).copied().unwrap_or("");
    let additional = parts.get(2).copied().unwrap_or("");
    let prefix = parts.get(3).copied().unwrap_or("");
    let suffix = parts.get(4).copied().unwrap_or("");
    [prefix, given, additional, family, suffix]
        .iter()
        .map(|part| unescape(part.trim()))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_escaped(input: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            ch if ch == separator => {
                parts.push(&input[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn is_quoted_printable(line: &str) -> bool {
    line.split(':')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
        .contains("ENCODING=QUOTED-PRINTABLE")
}

fn unescape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// vCard 2.1 uses quoted-printable for non-ASCII names. Decode `=XX` and drop
/// soft line breaks; leave the value untouched when the parameter is absent.
fn decode_quoted_printable(value: &str, params: &str) -> String {
    if !params
        .to_ascii_uppercase()
        .split(';')
        .any(|param| param.trim() == "ENCODING=QUOTED-PRINTABLE")
    {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'=' {
            if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                index += 2;
                continue;
            }
            if index + 2 < bytes.len() {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    out.push(high * 16 + low);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !value.chars().any(|ch| ch.is_whitespace() || ch == '<' || ch == '>')
}

// ---- Evolution Data Server ------------------------------------------------

const SOURCES_DEST: &str = "org.gnome.evolution.dataserver.Sources5";
const SOURCES_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";
const SOURCE_INTERFACE: &str = "org.gnome.evolution.dataserver.Source";
const FACTORY_DEST: &str = "org.gnome.evolution.dataserver.AddressBook10";
const FACTORY_PATH: &str = "/org/gnome/evolution/dataserver/AddressBookFactory";
const FACTORY_INTERFACE: &str = "org.gnome.evolution.dataserver.AddressBookFactory";
const BOOK_INTERFACE: &str = "org.gnome.evolution.dataserver.AddressBook";
/// Match every contact that has any field; we filter for `EMAIL` while parsing.
const ANY_FIELD_QUERY: &str = "(contains \"x-evolution-any-field\" \"\")";

/// No single address book may hold up composing for longer than this.
const BOOK_TIMEOUT: Duration = Duration::from_secs(6);
/// Hard ceiling for the whole EDS read, so a hanging backend cannot stall the
/// background refresh forever.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(18);
/// The source registry should answer immediately; never hang startup on it.
const SOURCES_TIMEOUT: Duration = Duration::from_secs(6);

type ManagedObjects = HashMap<
    OwnedObjectPath,
    HashMap<String, HashMap<String, OwnedValue>>,
>;

/// Read every enabled GNOME address book.
///
/// Each book runs in its own short-lived timeout. Books that are unavailable
/// (not configured, authentication required, backend still starting) are
/// skipped; the caller still gets the contacts from the books that answered.
pub async fn gnome_contacts() -> Result<Vec<Address>> {
    let connection = tokio::time::timeout(SOURCES_TIMEOUT, zbus::Connection::session())
        .await
        .map_err(|_| Error::Network("GNOME address book: session bus timed out".into()))?
        .map_err(eds_error)?;
    let uids = tokio::time::timeout(SOURCES_TIMEOUT, address_book_sources(&connection))
        .await
        .map_err(|_| Error::Network("GNOME address book: source manager timed out".into()))??;
    let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;
    let mut contacts = Vec::new();
    for uid in uids {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::debug!("contact refresh deadline reached; skipping remaining books");
            break;
        }
        match tokio::time::timeout(
            remaining.min(BOOK_TIMEOUT),
            read_book(&connection, &uid),
        )
        .await
        {
            Ok(Ok(mut addresses)) => contacts.append(&mut addresses),
            Ok(Err(error)) => {
                tracing::debug!(book = %uid, %error, "skipping address book");
            }
            Err(_) => {
                tracing::debug!(book = %uid, "address book timed out");
            }
        }
    }
    Ok(contacts)
}

async fn address_book_sources(connection: &zbus::Connection) -> Result<Vec<String>> {
    let manager = zbus::Proxy::new(
        connection,
        SOURCES_DEST,
        SOURCES_PATH,
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    .map_err(eds_error)?;
    let objects: ManagedObjects = manager
        .call("GetManagedObjects", &())
        .await
        .map_err(eds_error)?;

    let mut uids = Vec::new();
    for (_path, interfaces) in objects {
        let Some(properties) = interfaces.get(SOURCE_INTERFACE) else {
            continue;
        };
        let Some(uid) = string_property(properties, "UID") else {
            continue;
        };
        let Some(data) = string_property(properties, "Data") else {
            continue;
        };
        if source_is_address_book(&data) {
            uids.push(uid);
        }
    }
    Ok(uids)
}

fn source_is_address_book(data: &str) -> bool {
    let mut section = "";
    let mut enabled = true;
    let mut address_book = false;
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            if section == "Address Book" {
                address_book = true;
            }
            continue;
        }
        if section == "Data Source" {
            if let Some(value) = line.strip_prefix("Enabled=") {
                enabled = !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no"
                );
            }
        }
    }
    address_book && enabled
}

fn string_property(properties: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = properties.get(key)?;
    if let Ok(text) = String::try_from(value.clone()) {
        return Some(text);
    }
    Vec::<u8>::try_from(value.clone())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

async fn read_book(connection: &zbus::Connection, uid: &str) -> Result<Vec<Address>> {
    let factory = zbus::Proxy::new(
        connection,
        FACTORY_DEST,
        FACTORY_PATH,
        FACTORY_INTERFACE,
    )
    .await
    .map_err(eds_error)?;
    let (path, bus): (String, String) = factory
        .call("OpenAddressBook", &(uid,))
        .await
        .map_err(eds_error)?;
    let book =
        zbus::Proxy::new(connection, bus.as_str(), path.as_str(), BOOK_INTERFACE)
            .await
            .map_err(eds_error)?;

    // The backend subprocess can take a moment to export the object after the
    // factory returns. Retry only "not there yet" errors; a real failure (such
    // as missing credentials) should be reported immediately.
    let mut last_error = None;
    for attempt in 0..10 {
        let result: zbus::Result<Vec<String>> = book.call("Open", &()).await;
        match result {
            Ok(_) => {
                last_error = None;
                break;
            }
            Err(error) if attempt < 9 && retryable(&error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => {
                last_error = Some(error);
                break;
            }
        }
    }
    if let Some(error) = last_error {
        return Err(eds_error(error));
    }

    let cards: Vec<String> = book
        .call("GetContactList", &(ANY_FIELD_QUERY,))
        .await
        .map_err(eds_error)?;
    let _: std::result::Result<(), _> = book.call("Close", &()).await;
    Ok(parse_vcards(&cards))
}

fn retryable(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => {
            let name = name.as_str();
            name.ends_with("UnknownObject")
                || name.ends_with("UnknownMethod")
                || name.ends_with("ServiceUnknown")
                || name.ends_with("NameHasNoOwner")
        }
        _ => false,
    }
}

fn eds_error(error: zbus::Error) -> Error {
    Error::Network(format!("GNOME address book: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(name: &str, email: &str) -> Address {
        Address::new(name, email)
    }

    #[test]
    fn token_is_the_address_around_the_caret() {
        let token = recipient_token("bob@example.com, al", 22);
        assert_eq!(token.text, "al");
        assert_eq!((token.start, token.end), (17, 19));

        let token = recipient_token("  bob@example.com  ", 5);
        assert_eq!(token.text, "bob@example.com");
        assert_eq!((token.start, token.end), (2, 17));

        let token = recipient_token("a@x;b@y", 3);
        assert_eq!(token.text, "a@x");
        assert_eq!((token.start, token.end), (0, 3));
    }

    #[test]
    fn accepting_the_last_token_starts_a_new_slot() {
        let (text, caret) = accept_suggestion("", 0, "Ada <ada@example.org>");
        assert_eq!(text, "Ada <ada@example.org>, ");
        assert_eq!(caret, text.chars().count());
    }

    #[test]
    fn accepting_replaces_only_the_typed_prefix() {
        let (text, caret) = accept_suggestion("bob@x, al", 9, "Al <al@x>");
        assert_eq!(text, "bob@x, Al <al@x>, ");
        assert_eq!(caret, text.chars().count());

        let (text, caret) = accept_suggestion("bob@x, al, carol@x", 9, "Al <al@x>");
        assert_eq!(text, "bob@x, Al <al@x>, carol@x");
        assert_eq!(caret, 18);
    }

    #[test]
    fn suggestions_rank_address_book_and_usage() {
        let index = ContactIndex {
            history: vec![
                RecipientStat {
                    address: address("Alice", "alice@example.com"),
                    uses: 8,
                    last_used: 100,
                },
                RecipientStat {
                    address: address("", "al@work.example"),
                    uses: 2,
                    last_used: 50,
                },
            ],
            book: vec![
                address("Alice Smith", "alice@example.com"),
                address("Al Brown", "abrown@example.org"),
            ],
        };
        let found = index.suggest("al", &[], 10);
        let emails: Vec<&str> = found.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["alice@example.com", "al@work.example", "abrown@example.org"]);
        // The address book supplied the name, the history the usage.
        assert_eq!(found[0].name, "Alice Smith");

        // Already-typed recipients are filtered case-insensitively.
        let found = index.suggest("al", &["ALICE@example.com".to_string()], 10);
        assert!(found.iter().all(|a| a.email != "alice@example.com"));
    }

    #[test]
    fn matches_every_term_in_name_or_address() {
        let index = ContactIndex {
            book: vec![address("Grace Brewster Hopper", "grace@navy.example")],
            ..ContactIndex::default()
        };
        assert_eq!(index.suggest("brewster hopper", &[], 5).len(), 1);
        assert!(index.suggest("brewster missing", &[], 5).is_empty());
        assert_eq!(index.suggest("navy", &[], 5).len(), 1);
    }

    #[test]
    fn inserts_a_bare_address_when_the_name_would_split() {
        assert_eq!(
            insertable_address(&address("Doe, Jane", "jane@example.com")),
            "jane@example.com"
        );
        assert_eq!(
            insertable_address(&address("Jane Doe", "jane@example.com")),
            "Jane Doe <jane@example.com>"
        );
    }

    #[test]
    fn parses_vcard_30_with_multiple_emails_and_folding() {
        let card = "BEGIN:VCARD\r\n\
                    VERSION:3.0\r\n\
                    N:Doe;John;;;\r\n\
                    FN:John \r\n Doe\r\n\
                    EMAIL;TYPE=HOME,PREF:john@example.com\r\n\
                    EMAIL;TYPE=WORK:jdoe@work.example\r\n\
                    END:VCARD";
        let addresses = parse_vcard(card);
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0], address("John Doe", "john@example.com"));
        assert_eq!(addresses[1], address("John Doe", "jdoe@work.example"));
    }

    #[test]
    fn falls_back_to_structured_name_or_organization() {
        let card = "N:Doe;Jane;;;\nEMAIL:jane@example.com";
        assert_eq!(parse_vcard(card)[0].name, "Jane Doe");

        let card = "ORG:Example Corp;Engineering\nEMAIL:team@example.com";
        assert_eq!(parse_vcard(card)[0].name, "Example Corp");
    }

    #[test]
    fn handles_group_prefixes_quoted_printable_and_escapes() {
        let card = "item1.EMAIL;TYPE=INTERNET:foo@example.com\n\
                    FN;ENCODING=QUOTED-PRINTABLE;CHARSET=UTF-8:J=C3=B6rg\n\
                    N:Example\\; Inc.;J=C3=B6rg;;;";
        let addresses = parse_vcard(card);
        assert_eq!(addresses[0].email, "foo@example.com");
        assert_eq!(addresses[0].name, "Jörg");
        // N fallback keeps an escaped semicolon intact.
        let card = "N:Example\\; Inc.;Anna;;;\nEMAIL:anna@example.com";
        assert_eq!(parse_vcard(card)[0].name, "Anna Example; Inc.");
    }

    #[test]
    fn joins_folded_quoted_printable_without_corrupting_escapes() {
        let card = "FN;ENCODING=QUOTED-PRINTABLE:J=C3=B6rg=20=\n Smith\n\
                    EMAIL:joerg@example.com";
        let addresses = parse_vcard(card);
        assert_eq!(addresses[0].name, "Jörg Smith");
        assert_eq!(addresses[0].email, "joerg@example.com");
    }

    #[test]
    fn ignores_cards_without_a_valid_email() {
        let address_book = "BEGIN:VCARD\nVERSION:3.0\nFN:No Mail\nTEL:+123\nEND:VCARD";
        assert!(parse_vcard(address_book).is_empty());
        let bad = "EMAIL:not-an-address\nFN:Bad";
        assert!(parse_vcard(bad).is_empty());
    }

    #[test]
    fn source_keyfile_must_be_an_enabled_address_book() {
        let data = "[Data Source]\nEnabled=true\n[Address Book]\nBackendName=local";
        assert!(source_is_address_book(data));
        let disabled = "[Data Source]\nEnabled=false\n[Address Book]\nBackendName=local";
        assert!(!source_is_address_book(disabled));
        let calendar = "[Data Source]\nEnabled=true\n[Calendar]\nBackendName=local";
        assert!(!source_is_address_book(calendar));
        let default_enabled = "[Address Book]\nBackendName=local";
        assert!(source_is_address_book(default_enabled));
    }

    #[tokio::test]
    #[ignore = "requires a GNOME session with Evolution Data Server"]
    async fn reads_the_live_address_books() {
        let contacts = gnome_contacts().await.expect("read GNOME contacts");
        eprintln!("{} contacts with email addresses", contacts.len());
    }
}
