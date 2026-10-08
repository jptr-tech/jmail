//! Shared application services used by the UI (and the settings dialog).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use jmail_core::{Db, SyncEngine};

/// Handles the UI needs to talk to the sync engine.
#[derive(Clone)]
pub struct AppContext {
    pub db: Db,
    pub engine: SyncEngine,
    pub runtime: tokio::runtime::Handle,
}

thread_local! {
    /// Re-renders the reading pane after a reading-related setting changes.
    /// GTK is single-threaded, so a thread-local is the right home for it (and
    /// keeps [`AppContext`] `Send + Sync` for the `OnceLock`).
    static REFRESH_READING: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Install the reading-pane refresh hook (called once from `Ui::new`).
pub fn set_refresh_reading(hook: Rc<dyn Fn()>) {
    REFRESH_READING.with(|cell| *cell.borrow_mut() = Some(hook));
}

/// Re-render the reading pane, if a hook is installed.
pub fn refresh_reading() {
    REFRESH_READING.with(|cell| {
        let hook = cell.borrow().clone();
        if let Some(hook) = hook {
            hook();
        }
    });
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
