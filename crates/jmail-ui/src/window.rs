use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::BoxedAnyObject;
use gtk::{Align, Orientation};

use jmail_core::{
    AccountId, Db, Event, MessageId, SpecialUse, SyncEngine, ThreadFilter, ThreadSummary,
};

use crate::context::AppContext;
use crate::message_row::MessageRow;

/// The main application window and the widgets the actions operate on.
///
/// All fields are cheaply cloneable (GTK reference-counted handles), so the
/// action and key layers can capture a clone of the whole struct.
#[derive(Clone)]
pub struct Ui {
    pub window: adw::ApplicationWindow,
    /// Presented over the whole window for transient messages (saved files…).
    pub toast_overlay: adw::ToastOverlay,
    /// Kept for future per-pane toggles; the folder pane is always shown.
    #[allow(dead_code)]
    pub folders_split: adw::OverlaySplitView,
    pub list_split: adw::NavigationSplitView,
    pub window_title: adw::WindowTitle,
    pub status_label: gtk::Label,
    pub db: Db,
    pub engine: SyncEngine,
    pub runtime: tokio::runtime::Handle,
    /// Container rebuilt whenever the account/folder list changes.
    pub folder_content: gtk::Box,
    pub store: gio::ListStore,
    pub selection: gtk::SingleSelection,
    pub list_view: gtk::ListView,
    pub search_query: Rc<RefCell<String>>,
    /// While set, selection changes do not trigger a reading-pane preview.
    pub suppress_preview: Rc<Cell<bool>>,
    /// Coalesces bursts of body-arrived events into one reading-pane refresh.
    pub preview_reload_pending: Rc<Cell<bool>>,
    /// Coalesces bursts of `MessagesChanged` events into one list refresh.
    pub messages_reload_pending: Rc<Cell<bool>>,
    /// The main reading pane. Detached message windows build their own, but use
    /// the same `ReadingPane` type so the views are identical.
    pub reading: Rc<crate::actions::ReadingPane>,
    /// Reading panes of open detached windows, kept alive while they are shown.
    pub detached_panes: Rc<RefCell<Vec<Rc<crate::actions::ReadingPane>>>>,
    pub search_bar: gtk::SearchBar,
    pub search_entry: gtk::SearchEntry,
    pub search_toggle: gtk::ToggleButton,
    /// The active folder/conversation filter for the list.
    pub active_view: Rc<Cell<ThreadFilter>>,
    /// Sidebar rows, so the active view can be highlighted.
    folder_rows: Rc<RefCell<Vec<(ThreadFilter, adw::ActionRow)>>>,
    /// Whether HTML bodies may load remote images (reading setting).
    pub remote_images: Rc<Cell<bool>>,
    /// Whether message bodies ignore the sender's colours and follow the app
    /// theme instead (reading setting).
    pub readable_colors: Rc<Cell<bool>>,
    /// Result handlers for open compose windows, keyed by correlation id.
    pub compose_replies: Rc<RefCell<HashMap<u64, Rc<dyn Fn(crate::compose::ComposeOutcome)>>>>,
    next_compose_id: Rc<Cell<u64>>,
    /// A draft whose body is still being fetched before compose can open.
    pub pending_draft: Rc<RefCell<Option<MessageId>>>,
}

impl Ui {
    pub fn new(
        app: &adw::Application,
        db: Db,
        engine: SyncEngine,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let status_label = gtk::Label::builder()
            .label("Ready")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        status_label.add_css_class("dim-label");
        status_label.add_css_class("caption");

        let remote_images = Rc::new(Cell::new(crate::prefs::get_bool(
            &db,
            crate::prefs::REMOTE_IMAGES,
            true,
        )));
        let readable_colors = Rc::new(Cell::new(crate::prefs::get_bool(
            &db,
            crate::prefs::READABLE_COLORS,
            true,
        )));

        let (header, search_toggle, window_title) = build_header();
        let folder_content = gtk::Box::new(Orientation::Vertical, 0);
        folder_content.set_margin_top(6);
        folder_content.set_margin_bottom(6);
        populate_folders(&folder_content, &db, None);
        let folder_pane = build_folder_pane(&folder_content);
        let reading = crate::actions::ReadingPane::new(status_label.clone());

        // Message list: store -> selection -> view.
        let store = gio::ListStore::new::<BoxedAnyObject>();
        let search_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let list_view = gtk::ListView::new(
            Some(selection.clone()),
            None::<gtk::SignalListItemFactory>,
        );
        list_view.add_css_class("navigation-sidebar");
        // NOTE: `single-click-activate` also selects rows on hover in GTK, so
        // it stays off; single click is handled explicitly below.
        list_view.set_single_click_activate(false);

        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search mail"));
        let search_bar = gtk::SearchBar::new();
        search_bar.set_child(Some(&search_entry));
        search_bar.connect_entry(&search_entry);
        search_bar.set_show_close_button(true);

        let list_scrolled = gtk::ScrolledWindow::builder()
            .child(&list_view)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let list_pane = gtk::Box::new(Orientation::Vertical, 0);
        list_pane.append(&search_bar);
        list_pane.append(&list_scrolled);

        // Middle + right: message list and the conversation reading pane.
        let list_split = adw::NavigationSplitView::new();
        list_split.set_sidebar(Some(&adw::NavigationPage::new(&list_pane, "Inbox")));
        list_split.set_content(Some(&adw::NavigationPage::new(&reading.root, "Conversation")));
        list_split.set_min_sidebar_width(320.0);
        list_split.set_max_sidebar_width(480.0);
        list_split.set_sidebar_width_fraction(0.38);

        // Left: the folder/account pane, always present.
        let folders_split = adw::OverlaySplitView::new();
        folders_split.set_sidebar(Some(&folder_pane));
        folders_split.set_content(Some(&list_split));
        folders_split.set_pin_sidebar(true);
        folders_split.set_show_sidebar(true);
        folders_split.set_min_sidebar_width(210.0);
        folders_split.set_max_sidebar_width(300.0);
        folders_split.set_sidebar_width_fraction(0.2);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&folders_split));

        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&toolbar));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Jmail")
            .default_width(1180)
            .default_height(760)
            .content(&toast_overlay)
            .build();

        // Squeeze the list/reading split into a single pane on narrow windows.
        if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 850px") {
            let breakpoint = adw::Breakpoint::new(condition);
            breakpoint.add_setter(&list_split, "collapsed", Some(&gtk::glib::Value::from(true)));
            window.add_breakpoint(breakpoint);
        }

        let ui = Self {
            window,
            toast_overlay,
            folders_split,
            list_split,
            window_title,
            status_label,
            db,
            engine,
            runtime,
            folder_content,
            store,
            selection,
            list_view,
            search_query,
            suppress_preview: Rc::new(Cell::new(false)),
            preview_reload_pending: Rc::new(Cell::new(false)),
            messages_reload_pending: Rc::new(Cell::new(false)),
            reading,
            detached_panes: Rc::new(RefCell::new(Vec::new())),
            search_bar,
            search_entry,
            search_toggle,
            active_view: Rc::new(Cell::new(ThreadFilter::Unified)),
            folder_rows: Rc::new(RefCell::new(Vec::new())),
            remote_images,
            readable_colors,
            compose_replies: Rc::new(RefCell::new(HashMap::new())),
            next_compose_id: Rc::new(Cell::new(0)),
            pending_draft: Rc::new(RefCell::new(None)),
        };

        // The factory needs a `Ui` handle for the row context menu, so it is
        // installed after the struct exists.
        ui.list_view.set_factory(Some(&build_factory(&ui)));

        // The main pane's Reply buttons route through the `win.*` actions (the
        // same path the keyboard uses), which already target `ui.reading`.
        {
            let window = ui.window.clone();
            ui.reading.set_on_reply(Rc::new(move |mode| {
                let action = match mode {
                    crate::compose::Mode::Reply => "win.reply",
                    crate::compose::Mode::ReplyAll => "win.reply-all",
                    crate::compose::Mode::Forward => "win.forward",
                    _ => return,
                };
                let _ = gtk::prelude::WidgetExt::activate_action(&window, action, None);
            }));
        }

        // Attachment chips inside a rendered body call back here; the fetch and
        // open/save routing lives in `actions`.
        {
            let reading = ui.reading.clone();
            let handle = ui.clone();
            reading.set_on_attachment(Rc::new(move |message, part, save| {
                crate::actions::handle_attachment(&handle, MessageId(message), part, save);
            }));
        }

        crate::context::set(AppContext {
            db: ui.db.clone(),
            engine: ui.engine.clone(),
            runtime: ui.runtime.clone(),
        });
        {
            let ui = ui.clone();
            crate::context::set_refresh_reading(Rc::new(move || ui.reload_reading_settings()));
        }

        ui.connect_widgets();
        ui.reload_folders();
        ui.reload_messages();
        crate::actions::install_window_actions(&ui);
        crate::keys::install(&ui);
        ui
    }

    /// Discover accounts from GNOME Online Accounts, then start a sync worker
    /// for every enabled account. Runs once per launch.
    pub fn bootstrap_accounts(&self) {
        self.refresh_accounts(true);
    }

    /// Reconcile GOA accounts with the cache, reload the folder pane, and start
    /// any newly enabled accounts. `show_welcome` presents first-run onboarding
    /// when reconciliation finishes.
    pub fn refresh_accounts(&self, show_welcome: bool) {
        let db = self.db.clone();
        let before = db.account_count().unwrap_or(0);
        let (tx, rx) = async_channel::bounded(1);
        self.runtime.spawn(async move {
            let result = jmail_core::goa::reconcile(&db).await;
            let _ = tx.send(result).await;
        });

        let ui = self.clone();
        gtk::glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(imported)) => tracing::debug!(imported, "account reconciliation complete"),
                Ok(Err(error)) => tracing::warn!(%error, "could not reconcile Online Accounts"),
                Err(_) => {}
            }
            // Only rebuild the folder pane when the set of accounts changed,
            // so regaining focus does not reset the sidebar selection.
            let after = ui.db.account_count().unwrap_or(before);
            if after != before {
                ui.reload_folders();
            }
            ui.start_enabled_accounts();
            if show_welcome {
                crate::welcome::maybe_show(&ui);
            }
        });
    }

    /// Start a sync worker for every enabled account.
    pub fn start_enabled_accounts(&self) {
        let accounts = match self.db.accounts() {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::error!(%error, "failed to list accounts");
                return;
            }
        };
        for account in accounts.into_iter().filter(|account| account.enabled) {
            self.start_account(account.id);
        }
    }

    /// Start (or restart) the sync worker for one account.
    pub fn start_account(&self, account: AccountId) {
        let engine = self.engine.clone();
        self.runtime.spawn(async move {
            if let Err(error) = engine.start_account(account).await {
                tracing::warn!(%error, account = account.0, "could not start account sync");
            }
        });
    }

    /// Apply a background sync event on the GTK main thread.
    pub fn handle_event(&self, event: Event) {
        match event {
            Event::FoldersChanged { .. } => self.reload_folders(),
            Event::MessagesChanged { .. } => crate::actions::schedule_messages_reload(self),
            Event::BodyLoaded { .. } => {
                // The list itself did not change; only refresh the reading pane.
                crate::actions::schedule_reload_preview(self);
                self.maybe_open_pending_draft();
            }
            Event::SyncStarted { .. } => {
                self.status_label.set_text("Syncing…");
            }
            Event::SyncFinished { .. } => {
                self.status_label.set_text("Up to date");
            }
            Event::SendResult { id, error, .. } => {
                self.deliver_compose(id, crate::compose::ComposeOutcome::Send { error });
            }
            Event::DraftSaved {
                id, draft, error, ..
            } => {
                self.deliver_compose(
                    id,
                    crate::compose::ComposeOutcome::Draft { draft, error },
                );
            }
            Event::Error { message, .. } => {
                tracing::warn!(%message, "sync error");
                self.status_label.set_text(&format!("Sync error: {message}"));
            }
            Event::AccountsChanged => {
                self.reset_view_if_disabled();
                self.reload_folders();
                self.reload_messages();
            }
            Event::NewInboxMessage { .. } => {}
            Event::AttachmentSaved {
                path, open, error, ..
            } => {
                // Opening hands straight off to the default viewer.
                if open && error.is_none() {
                    crate::actions::open_file(&path);
                    return;
                }
                let toast = if let Some(error) = error {
                    let verb = if open { "open" } else { "save" };
                    let toast = adw::Toast::new(&format!("Could not {verb} attachment: {error}"));
                    toast.set_timeout(5);
                    toast
                } else {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    let toast = adw::Toast::new(&format!("Saved {name}"));
                    toast.set_button_label(Some("Open"));
                    let path = path.clone();
                    toast.connect_button_clicked(move |_| {
                        crate::actions::open_file(&path);
                    });
                    toast
                };
                self.toast_overlay.add_toast(toast);
            }
            Event::OpFailed { error, .. } => {
                // The engine rolled back an optimistic change; refresh and say so.
                crate::actions::schedule_messages_reload(self);
                let toast = adw::Toast::new(&format!("Couldn't complete the change: {error}"));
                toast.set_timeout(5);
                self.toast_overlay.add_toast(toast);
            }
        }
    }

    /// If the active folder belongs to a now-disabled account, fall back to the
    /// Joined Inbox.
    fn reset_view_if_disabled(&self) {
        let ThreadFilter::Folder(id) = self.active_view.get() else {
            return;
        };
        let disabled = self
            .db
            .folder(id)
            .ok()
            .flatten()
            .and_then(|folder| self.db.account(folder.account_id).ok().flatten())
            .map(|account| !account.enabled)
            .unwrap_or(false);
        if disabled {
            self.active_view.set(ThreadFilter::Unified);
            self.window_title.set_subtitle("Joined Inbox");
        }
    }

    /// Allocate an id for a new compose window.
    pub fn register_compose(&self) -> u64 {
        let id = self.next_compose_id.get() + 1;
        self.next_compose_id.set(id);
        id
    }

    /// Attach the result handler for a compose window.
    pub fn register_compose_handler(
        &self,
        id: u64,
        handler: Rc<dyn Fn(crate::compose::ComposeOutcome)>,
    ) {
        self.compose_replies.borrow_mut().insert(id, handler);
    }

    /// Route a send/draft outcome to the matching compose window.
    pub fn deliver_compose(&self, id: u64, outcome: crate::compose::ComposeOutcome) {
        let handler = self.compose_replies.borrow().get(&id).cloned();
        if let Some(handler) = handler {
            handler(outcome);
        }
    }

    pub fn unregister_compose(&self, id: u64) {
        self.compose_replies.borrow_mut().remove(&id);
    }

    /// Remove the inline composer and forget its handler.
    pub fn clear_inline(&self) {
        self.reading.clear_inline(self);
    }

    /// Show a transient toast in the main window.
    pub fn toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(5);
        self.toast_overlay.add_toast(toast);
    }

    /// Scroll the reading pane to the bottom (to reveal an inline composer).
    pub fn scroll_reading_to_bottom(&self) {
        self.reading.scroll_to_bottom();
    }

    /// Open a draft once its body has been fetched.
    pub fn maybe_open_pending_draft(&self) {
        let Some(id) = self.pending_draft.borrow().clone() else {
            return;
        };
        let has_body = self
            .db
            .message_body(id)
            .ok()
            .flatten()
            .and_then(|body| body.text)
            .is_some();
        if has_body {
            *self.pending_draft.borrow_mut() = None;
            crate::compose::present(self, crate::compose::Mode::Draft, Some(id));
        }
    }

    /// Rebuild the account/folder pane from the cache.
    pub fn reload_folders(&self) {
        while let Some(child) = self.folder_content.first_child() {
            self.folder_content.remove(&child);
        }
        self.folder_rows.borrow_mut().clear();
        populate_folders(&self.folder_content, &self.db, Some(self));
        apply_folder_highlight(self);
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Re-read reading preferences (remote images) and re-render the open
    /// conversation, e.g. after the setting is toggled in the dialog.
    pub fn reload_reading_settings(&self) {
        self.remote_images.set(crate::prefs::get_bool(
            &self.db,
            crate::prefs::REMOTE_IMAGES,
            true,
        ));
        self.readable_colors.set(crate::prefs::get_bool(
            &self.db,
            crate::prefs::READABLE_COLORS,
            true,
        ));
        crate::actions::reload_preview(self);
    }

    /// Reload the message list from the cache (unified inbox, newest first),
    /// preserving the selection where possible.
    pub fn reload_messages(&self) {
        let previous_thread = self.selected_thread_id();
        let previous_position = self.selection.selected();

        let query = self.search_query.borrow().clone();
        let results = if query.is_empty() {
            self.db.threads(self.active_view.get(), 500)
        } else {
            // Search spans all cached mail (subject/sender/body), not just the
            // rows currently loaded in the list.
            self.db.search_threads(&query, 500)
        };
        let changed = match results {
            Ok(threads) => {
                if !query.is_empty() {
                    self.status_label.set_text(&format!(
                        "{} conversation(s) matching “{query}”",
                        threads.len()
                    ));
                }
                self.apply_threads(&threads)
            }
            Err(error) => {
                tracing::error!(%error, "failed to load messages");
                false
            }
        };
        if !changed {
            // Nothing to update; leave the selection and reading pane alone.
            return;
        }

        // Keep the same conversation selected if it is still present (its
        // representative message may have changed); otherwise fall back to the
        // same position so a removed conversation selects the next one.
        if let Some(thread) = previous_thread {
            if let Some(position) = self.position_of_thread(&thread) {
                self.select_quietly(position);
                return;
            }
        }
        if previous_position != u32::MAX {
            let count = self.selection.n_items();
            if count > 0 {
                let position = previous_position.min(count - 1);
                self.select_quietly(position);
                // The open conversation was removed (deleted/moved): load the
                // one that took its place, opening the pane in the narrow layout.
                crate::actions::preview_after_removal(
                    self,
                    position,
                    self.list_split.is_collapsed(),
                );
            } else {
                // Nothing left in this view.
                crate::actions::clear_preview(self, &self.reading);
            }
        }
    }

    /// Update the list in place: only rows that differ from what is shown are
    /// replaced, so unchanged rows are not rebound (which would drop their hover
    /// state and tear down an open context menu). Returns whether anything
    /// changed at all.
    fn apply_threads(&self, threads: &[ThreadSummary]) -> bool {
        let new_rows: Vec<MessageRow> = threads.iter().map(MessageRow::from_thread).collect();
        let old_rows: Vec<MessageRow> = (0..self.store.n_items())
            .filter_map(|index| self.store.item(index))
            .filter_map(|object| object.downcast::<BoxedAnyObject>().ok())
            .map(|boxed| boxed.borrow::<MessageRow>().clone())
            .collect();

        let (prefix, remove) = diff_indices(&old_rows, &new_rows);
        let suffix = old_rows.len() - prefix - remove;
        let additions: Vec<BoxedAnyObject> = new_rows[prefix..new_rows.len() - suffix]
            .iter()
            .cloned()
            .map(BoxedAnyObject::new)
            .collect();
        if remove == 0 && additions.is_empty() {
            return false;
        }
        self.store
            .splice(prefix as u32, remove as u32, &additions);
        true
    }

    /// Select a row without triggering the reading-pane preview. Used when a
    /// model rebuild or in-place row update would otherwise fire a redundant
    /// re-render (and drop the selection).
    pub(crate) fn select_quietly(&self, position: u32) {
        self.suppress_preview.set(true);
        self.selection.set_selected(position);
        self.suppress_preview.set(false);
    }

    fn selected_thread_id(&self) -> Option<String> {
        let item = self.selection.selected_item()?;
        let boxed = item.downcast::<BoxedAnyObject>().ok()?;
        let thread_id = boxed.borrow::<MessageRow>().thread_id.clone();
        Some(thread_id)
    }

    fn position_of_thread(&self, thread_id: &str) -> Option<u32> {
        for position in 0..self.selection.n_items() {
            let Some(item) = self.selection.item(position) else {
                continue;
            };
            let Ok(boxed) = item.downcast::<BoxedAnyObject>() else {
                continue;
            };
            if boxed.borrow::<MessageRow>().thread_id == thread_id {
                return Some(position);
            }
        }
        None
    }

    /// Wire the passive widgets (search bar, list selection) to the actions.
    fn connect_widgets(&self) {
        // Search toggle button <-> search bar visibility.
        {
            let bar = self.search_bar.clone();
            self.search_toggle
                .connect_toggled(move |button| bar.set_search_mode(button.is_active()));
        }
        {
            let toggle = self.search_toggle.clone();
            let ui = self.clone();
            let query = self.search_query.clone();
            self.search_bar
                .connect_search_mode_enabled_notify(move |bar| {
                    toggle.set_active(bar.is_search_mode());
                    if !bar.is_search_mode() {
                        // Closing search clears the query and restores the
                        // folder view that was active underneath it.
                        let had_query = !query.borrow().is_empty();
                        query.borrow_mut().clear();
                        if had_query {
                            ui.search_entry.set_text("");
                            ui.reload_messages();
                        }
                    }
                });
        }
        // Re-run the cache search after a short debounce so we query once per
        // pause instead of on every keystroke.
        {
            let ui = self.clone();
            let query = self.search_query.clone();
            let generation = Rc::new(Cell::new(0u64));
            self.search_entry.connect_search_changed(move |entry| {
                *query.borrow_mut() = entry.text().trim().to_string();
                let current = generation.get().wrapping_add(1);
                generation.set(current);
                let ui = ui.clone();
                let generation = generation.clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(150),
                    move || {
                        if generation.get() == current {
                            ui.reload_messages();
                        }
                    },
                );
            });
        }
        // Activation: double click opens a separate window in the wide layout;
        // in the collapsed layout a single click opens it in the reading pane.
        {
            let window = self.window.clone();
            let split = self.list_split.clone();
            self.list_view.connect_activate(move |_, _| {
                let action = if split.is_collapsed() {
                    "win.open-pane"
                } else {
                    "win.open-window"
                };
                let _ = gtk::prelude::WidgetExt::activate_action(&window, action, None);
            });
        }
        // GTK's `single-click-activate` also selects rows on hover, so only turn
        // it on when collapsed (hover-preview is suppressed there anyway).
        {
            let list_view = self.list_view.clone();
            self.list_split.connect_collapsed_notify(move |split| {
                list_view.set_single_click_activate(split.is_collapsed());
            });
        }
        // Selecting a message previews it in the reading pane. Note: the
        // `selection-changed` signal reports the start of the *changed range*,
        // not the newly selected row, so read the actual selection instead.
        {
            let ui = self.clone();
            self.selection
                .connect_selection_changed(move |selection, _, _| {
                    let position = selection.selected();
                    if position != u32::MAX {
                        crate::actions::request_preview(&ui, position);
                    }
                });
        }
        // Re-theme HTML bodies when the desktop switches light/dark.
        {
            let ui = self.clone();
            adw::StyleManager::default().connect_dark_notify(move |_| {
                crate::actions::reload_preview(&ui);
            });
        }
        // Warm up the reading pane's reusable WebView whenever it becomes
        // visible (at launch, or when the pane is first shown in the narrow
        // layout) so the first conversation avoids the cold-start black frame.
        {
            let ui = self.clone();
            self.reading.conversation.connect_map(move |_| {
                crate::actions::preload_reading_pane(&ui);
            });
        }
        // Pick up accounts added in GNOME Settings while the app was in the
        // background (for example after the first-run welcome dialog).
        {
            let ui = self.clone();
            self.window.connect_is_active_notify(move |window| {
                if window.is_active() {
                    ui.refresh_accounts(false);
                }
            });
        }
    }
}

fn build_header() -> (adw::HeaderBar, gtk::ToggleButton, adw::WindowTitle) {
    let header = adw::HeaderBar::new();

    let window_title = adw::WindowTitle::new("Jmail", "Joined Inbox");
    header.set_title_widget(Some(&window_title));

    let search_button = gtk::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text("Search (/)")
        .build();

    let compose_button = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Compose (c)")
        .build();
    compose_button.set_action_name(Some("win.compose"));

    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .build();
    menu_button.set_menu_model(Some(&build_menu()));

    header.pack_end(&menu_button);
    header.pack_end(&compose_button);
    header.pack_end(&status_dot());
    header.pack_end(&search_button);
    (header, search_button, window_title)
}

/// A small static dot that simply indicates Jmail is running.
fn status_dot() -> gtk::Box {
    let dot = gtk::Box::new(Orientation::Vertical, 0);
    dot.set_size_request(9, 9);
    dot.set_valign(Align::Center);
    dot.add_css_class("status-dot");
    dot.set_tooltip_text(Some("Jmail is running"));
    dot
}

fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("_Keyboard Shortcuts"), Some("win.help"));
    menu.append(Some("_Settings"), Some("app.settings"));
    menu.append(Some("_About Jmail"), Some("app.about"));
    menu.append(Some("_Quit"), Some("app.quit"));
    menu
}

fn build_folder_pane(content: &gtk::Box) -> gtk::Widget {
    let scrolled = gtk::ScrolledWindow::builder()
        .child(content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    scrolled.add_css_class("folder-pane");
    scrolled.upcast()
}

/// (Re)build the account and folder rows inside `content`, wiring clicks to
/// the folder view when a `Ui` handle is available.
fn populate_folders(content: &gtk::Box, db: &Db, ui: Option<&Ui>) {
    let joined = folder_list();
    let joined_row = folder_row("mail-unread-symbolic", "Joined Inbox", None);
    if let Some(ui) = ui {
        ui.folder_rows
            .borrow_mut()
            .push((ThreadFilter::Unified, joined_row.clone()));
        let ui = ui.clone();
        joined_row.connect_activated(move |_| set_view(&ui, "Joined Inbox", ThreadFilter::Unified));
    }
    joined.append(&joined_row);
    content.append(&joined);

    match db.accounts() {
        Ok(accounts) if accounts.is_empty() => {
            let empty = gtk::Label::new(Some("No accounts yet.\nAdd one in Settings."));
            empty.set_xalign(0.0);
            empty.set_margin_top(24);
            empty.set_margin_start(12);
            empty.set_margin_end(12);
            empty.set_wrap(true);
            empty.add_css_class("dim-label");
            content.append(&empty);
        }
        Ok(accounts) => {
            for account in accounts.iter().filter(|account| account.enabled) {
                let label = gtk::Label::builder()
                    .label(&account.email)
                    .xalign(0.0)
                    .margin_top(12)
                    .margin_bottom(4)
                    .margin_start(12)
                    .build();
                label.add_css_class("dim-label");
                label.add_css_class("caption");
                label.add_css_class("heading");
                content.append(&label);

                let list = folder_list();
                if let Ok(folders) = db.folders(account.id) {
                    for folder in &folders {
                        let row = folder_row(
                            folder_icon(folder.special_use),
                            &folder.name,
                            None,
                        );
                        if let Some(ui) = ui {
                            let id = folder.id;
                            ui.folder_rows
                                .borrow_mut()
                                .push((ThreadFilter::Folder(id), row.clone()));
                            let ui = ui.clone();
                            let name = folder.name.clone();
                            row.connect_activated(move |_| {
                                set_view(&ui, &name, ThreadFilter::Folder(id))
                            });
                        }
                        list.append(&row);
                    }
                }
                content.append(&list);
            }
        }
        Err(error) => tracing::error!(%error, "failed to load accounts"),
    }
}

/// Switch the list to a folder view and refresh.
pub fn set_view(ui: &Ui, label: &str, filter: ThreadFilter) {
    ui.active_view.set(filter);
    ui.window_title.set_subtitle(label);
    apply_folder_highlight(ui);
    ui.reload_messages();
    ui.status_label.set_text(&format!("Viewing {label}"));
}

/// Highlight the sidebar row matching the active view (and clear the rest).
fn apply_folder_highlight(ui: &Ui) {
    let active = ui.active_view.get();
    for (filter, row) in ui.folder_rows.borrow().iter() {
        if *filter == active {
            row.add_css_class("folder-active");
        } else {
            row.remove_css_class("folder-active");
        }
    }
}

fn folder_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    list
}

fn folder_row(icon: &str, title: &str, count: Option<&str>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    // AdwActionRow is unactivatable by default, so `activated` (and thus
    // clicking a folder) would never fire without this.
    row.set_activatable(true);
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    if let Some(count) = count {
        let label = gtk::Label::new(Some(count));
        label.add_css_class("dim-label");
        row.add_suffix(&label);
    }
    row
}

pub(crate) fn folder_icon(special_use: SpecialUse) -> &'static str {
    match special_use {
        SpecialUse::Inbox => "mail-unread-symbolic",
        SpecialUse::Sent => "mail-send-symbolic",
        SpecialUse::Drafts => "document-edit-symbolic",
        SpecialUse::Trash => "user-trash-symbolic",
        SpecialUse::Archive => "mail-archive-symbolic",
        SpecialUse::Spam => "dialog-warning-symbolic",
        SpecialUse::All => "mail-message-new-symbolic",
        SpecialUse::None => "folder-symbolic",
    }
}

/// The `(prefix, remove)` slice of `old` that must be replaced to turn it into
/// `new`: everything before `prefix` and after `prefix + remove` is identical.
fn diff_indices(old: &[MessageRow], new: &[MessageRow]) -> (usize, usize) {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (old.len() - prefix)
        && suffix < (new.len() - prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    (prefix, old.len() - prefix - suffix)
}

/// The entries shown in the message context menu, grouped into sections.
const MESSAGE_MENU: &[&[(&str, &str)]] = &[
    &[("Open in New Window", "win.open-window")],
    &[
        ("Reply", "win.reply"),
        ("Reply All", "win.reply-all"),
        ("Forward", "win.forward"),
    ],
    &[
        ("Move to Folder…", "win.move"),
        ("Archive", "win.archive"),
        ("Delete", "win.delete"),
    ],
    &[
        ("Mark as Read", "win.mark-read"),
        ("Mark as Unread", "win.mark-unread"),
        ("Star", "win.star"),
    ],
];

/// Select the row without opening it, then pop the message context menu anchored
/// to the (stable) list view at the click point.
fn show_message_context_menu(ui: &Ui, position: u32, row: &gtk::Widget, x: f64, y: f64) {
    if position == u32::MAX {
        return;
    }
    // Highlight the row the menu acts on, but do not preview/open it.
    ui.select_quietly(position);

    let point = row
        .compute_point(&ui.list_view, &gtk::graphene::Point::new(x as f32, y as f32))
        .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));

    let popover = gtk::Popover::new();
    popover.add_css_class("message-menu");
    popover.set_has_arrow(false);

    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(4);
    content.set_margin_end(4);
    for (section, entries) in MESSAGE_MENU.iter().enumerate() {
        if section > 0 {
            content.append(&gtk::Separator::new(Orientation::Horizontal));
        }
        for (label, action) in entries.iter() {
            content.append(&menu_button(ui, &popover, position, label, action));
        }
    }
    popover.set_child(Some(&content));
    popover.set_parent(&ui.list_view);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
        point.x() as i32,
        point.y() as i32,
        1,
        1,
    )));
    let popover_clone = popover.clone();
    popover.connect_closed(move |_| popover_clone.unparent());
    popover.popup();
}

/// One flat, full-width button in the message context menu. It re-asserts the
/// row's selection and activates the matching `win.*` action directly, the same
/// path the keyboard shortcuts use (so it can't fail through the popover's
/// action muxer).
fn menu_button(
    ui: &Ui,
    popover: &gtk::Popover,
    position: u32,
    label: &str,
    action: &str,
) -> gtk::Button {
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_halign(gtk::Align::Fill);
    let button = gtk::Button::builder().child(&text).build();
    button.add_css_class("flat");
    button.add_css_class("menu-item");
    button.set_halign(gtk::Align::Fill);

    let ui = ui.clone();
    let popover = popover.clone();
    let action = action.to_string();
    let reply_like = matches!(
        action.as_str(),
        "win.reply" | "win.reply-all" | "win.forward"
    );
    button.connect_clicked(move |_| {
        if reply_like {
            // Open the clicked chain so the reply/forward targets it.
            crate::actions::preview_position(&ui, position);
        } else {
            ui.select_quietly(position);
        }
        if let Err(error) = gtk::prelude::WidgetExt::activate_action(&ui.window, &action, None) {
            tracing::warn!(action = %action, %error, "context menu action failed");
        }
        popover.popdown();
    });
    button
}

fn build_factory(ui: &Ui) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    {
        let ui = ui.clone();
        factory.connect_setup(move |_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let child = build_message_row();
            item.set_child(Some(&child));

            // Right-click opens the row's context menu at the cursor.
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            {
                let ui = ui.clone();
                let item = item.clone();
                let child = child.clone();
                gesture.connect_pressed(move |_, _, x, y| {
                    show_message_context_menu(&ui, item.position(), &child, x, y);
                });
            }
            child.add_controller(gesture);
        });
    }
    factory.connect_bind(|_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(child) = item.child() else {
            return;
        };
        let Some(item_object) = item.item() else {
            return;
        };
        let Ok(boxed) = item_object.downcast::<BoxedAnyObject>() else {
            return;
        };
        let row = boxed.borrow::<MessageRow>();
        bind_message_row(&child, &row);
    });
    factory
}

fn build_message_row() -> gtk::Widget {
    let row = gtk::Box::new(Orientation::Horizontal, 10);
    row.add_css_class("message-row");
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(12);

    let dot = gtk::Box::new(Orientation::Vertical, 0);
    dot.add_css_class("msg-dot");
    dot.set_size_request(8, 8);
    dot.set_valign(Align::Center);
    row.append(&dot);

    let text = gtk::Box::new(Orientation::Vertical, 2);
    text.set_hexpand(true);
    let subject = gtk::Label::new(None);
    subject.add_css_class("msg-subject");
    subject.set_xalign(0.0);
    subject.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let meta = gtk::Label::new(None);
    meta.add_css_class("msg-meta");
    meta.add_css_class("dim-label");
    meta.set_xalign(0.0);
    meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.append(&subject);
    text.append(&meta);
    row.append(&text);

    let star = gtk::Label::new(Some("★"));
    star.add_css_class("msg-star");
    star.add_css_class("dim-label");
    star.set_valign(Align::Center);
    star.set_visible(false);
    row.append(&star);

    let count = gtk::Label::new(None);
    count.add_css_class("msg-count");
    count.set_valign(Align::Center);
    count.set_visible(false);
    row.append(&count);

    let date = gtk::Label::new(None);
    date.add_css_class("msg-date");
    date.add_css_class("dim-label");
    date.add_css_class("caption");
    date.set_valign(Align::Start);
    row.append(&date);

    row.upcast()
}

fn bind_message_row(widget: &gtk::Widget, row: &MessageRow) {
    set_label(widget, "msg-subject", &row.subject);
    let meta = if row.snippet.is_empty() {
        row.sender.clone()
    } else {
        format!("{} — {}", row.sender, row.snippet)
    };
    set_label(widget, "msg-meta", &meta);
    set_label(widget, "msg-date", &row.date_label);
    if let Some(dot) = crate::util::find_by_css(widget, "msg-dot") {
        dot.set_visible(row.unread);
    }
    if let Some(star) = crate::util::find_by_css(widget, "msg-star") {
        star.set_visible(row.starred);
    }
    if let Some(count) = crate::util::find_by_css(widget, "msg-count")
        .and_then(|widget| widget.downcast::<gtk::Label>().ok())
    {
        if row.message_count > 1 {
            count.set_text(&row.message_count.to_string());
            count.set_visible(true);
        } else {
            count.set_visible(false);
        }
    }
    if row.unread {
        widget.add_css_class("unread");
    } else {
        widget.remove_css_class("unread");
    }
}

fn set_label(widget: &gtk::Widget, css: &str, text: &str) {
    if let Some(label) = crate::util::find_by_css(widget, css)
        .and_then(|widget| widget.downcast::<gtk::Label>().ok())
    {
        label.set_text(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, unread: bool) -> MessageRow {
        MessageRow {
            id,
            account_id: 1,
            thread_id: format!("t{id}"),
            subject: format!("s{id}"),
            sender: "x".into(),
            snippet: String::new(),
            date_label: String::new(),
            unread,
            starred: false,
            message_count: 1,
            unread_count: u32::from(unread),
            starred_count: 0,
        }
    }

    #[test]
    fn diff_detects_no_change() {
        let old = vec![row(3, true), row(2, false)];
        assert_eq!(diff_indices(&old, &old), (2, 0));
    }

    #[test]
    fn diff_detects_prepend() {
        let old = vec![row(2, false), row(1, false)];
        let new = vec![row(3, true), row(2, false), row(1, false)];
        assert_eq!(diff_indices(&old, &new), (0, 0));
    }

    #[test]
    fn diff_detects_append() {
        let old = vec![row(2, false)];
        let new = vec![row(2, false), row(1, true)];
        assert_eq!(diff_indices(&old, &new), (1, 0));
    }

    #[test]
    fn diff_detects_removal() {
        let old = vec![row(3, true), row(2, false), row(1, false)];
        let new = vec![row(2, false), row(1, false)];
        assert_eq!(diff_indices(&old, &new), (0, 1));
    }

    #[test]
    fn diff_detects_middle_change() {
        let old = vec![row(3, true), row(2, false), row(1, false)];
        let new = vec![row(3, true), row(2, true), row(1, false)];
        assert_eq!(diff_indices(&old, &new), (1, 1));
    }

    #[test]
    fn diff_detects_clear() {
        let old = vec![row(1, false)];
        assert_eq!(diff_indices(&old, &[]), (0, 1));
    }
}
