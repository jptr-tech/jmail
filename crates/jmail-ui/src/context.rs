//! Shared application services used by the UI (and the settings dialog).

use std::sync::OnceLock;

use jmail_core::{Db, SyncEngine};

/// Handles the UI needs to talk to the sync engine.
#[derive(Clone)]
pub struct AppContext {
    pub db: Db,
    pub engine: SyncEngine,
    pub runtime: tokio::runtime::Handle,
}

static CONTEXT: OnceLock<AppContext> = OnceLock::new();

/// Install the global context (called once from `Ui::new`).
pub fn set(context: AppContext) {
    CONTEXT.set(context).ok();
}

/// Access the global context, if the main window has been built.
pub fn get() -> Option<&'static AppContext> {
    CONTEXT.get()
}
