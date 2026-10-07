mod actions;
mod compose;
mod context;
mod keys;
mod markdown_editor;
mod message_row;
mod settings;
mod util;
mod window;

use std::cell::RefCell;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use jmail_core::{Db, Event, SyncEngine};

/// Reverse-DNS application id, also used as the Flatpak id and icon name.
pub const APP_ID: &str = "tech.jptr.jmail";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let runtime_handle = runtime.handle().clone();

    let db = open_db();
    let (engine, mut events_rx) = SyncEngine::start(db.clone());

    // Bridge sync events from the tokio runtime to the GLib main thread. GTK
    // widgets are not `Send`, so we forward through a plain channel and drain
    // it from a main-thread timeout.
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
    runtime.spawn(async move {
        while let Some(event) = events_rx.recv().await {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_startup(|app| {
        load_css();
        actions::install_app_actions(app);
    });

    // `connect_activate` is `Fn` and may run more than once; the event
    // receiver can only be consumed once, hence the `RefCell<Option<..>>`.
    let pending = RefCell::new(Some((db, engine, event_rx)));
    app.connect_activate(move |app| {
        let Some((db, engine, event_rx)) = pending.borrow_mut().take() else {
            return;
        };
        let ui = window::Ui::new(app, db, engine, runtime_handle.clone());
        let ui_for_events = ui.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            while let Ok(event) = event_rx.try_recv() {
                ui_for_events.handle_event(event);
            }
            glib::ControlFlow::Continue
        });
        ui.present();
        ui.start_all_accounts();

        if std::env::var("JMAIL_AUTOPREVIEW").as_deref() == Ok("1") {
            let a = ui.clone();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
                if a.selection.n_items() > 0 {
                    a.selection.set_selected(0);
                }
            });
            let b = ui.clone();
            glib::timeout_add_local_once(Duration::from_secs(6), move || {
                if b.selection.n_items() > 1 {
                    b.selection.set_selected(1);
                }
            });
        }
    });

    app.run()
}

/// Open the cache in the app data directory, falling back to memory.
fn open_db() -> Db {
    let dir = glib::user_data_dir().join("jmail");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%error, "could not create data directory");
    }
    let path = dir.join("cache.db");
    let db = match Db::open(&path) {
        Ok(db) => db,
        Err(error) => {
            tracing::error!(%error, "failed to open cache; using an in-memory database");
            Db::open_in_memory().expect("in-memory database")
        }
    };
    if std::env::var("JMAIL_SEED").as_deref() == Ok("1") {
        if let Err(error) = db.seed_demo() {
            tracing::error!(%error, "failed to seed demo data");
        }
    }
    // Heal conversations for caches written before References threading existed.
    if let Err(error) = db.rethread_all() {
        tracing::warn!(%error, "failed to rebuild conversations");
    }
    db
}

/// Load the application stylesheet once the display is available.
fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
