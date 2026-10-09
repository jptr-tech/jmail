//! Typed access to persisted UI preferences, backed by `Db::settings`.
//!
//! Booleans are stored as `"1"`/`"0"` so the values stay readable in the cache
//! and the core can read the ones it needs (e.g. the poll interval) without
//! depending on the UI.

use jmail_core::Db;

pub const NOTIFICATIONS_ENABLED: &str = "notifications.enabled";
pub const NOTIFICATIONS_INBOX_ONLY: &str = "notifications.inbox_only";
pub const NOTIFICATIONS_GROUP: &str = "notifications.group_by_account";
pub const REMOTE_IMAGES: &str = "reading.load_remote_images";
pub const READABLE_COLORS: &str = "reading.force_readable_colors";
pub const POLL_INTERVAL: &str = "sync.poll_interval";

pub fn get_bool(db: &Db, key: &str, default: bool) -> bool {
    match db.get_setting(key) {
        Ok(Some(value)) => value != "0" && value != "false",
        _ => default,
    }
}

pub fn set_bool(db: &Db, key: &str, value: bool) {
    if let Err(error) = db.set_setting(key, if value { "1" } else { "0" }) {
        tracing::error!(%error, key, "could not save setting");
    }
}

pub fn get_u64(db: &Db, key: &str, default: u64) -> u64 {
    db.get_setting(key)
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub fn set_u64(db: &Db, key: &str, value: u64) {
    if let Err(error) = db.set_setting(key, &value.to_string()) {
        tracing::error!(%error, key, "could not save setting");
    }
}
