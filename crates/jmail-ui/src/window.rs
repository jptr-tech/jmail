use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::BoxedAnyObject;
use gtk::{Align, Orientation};

use jmail_core::{AccountId, Db, Event, SpecialUse, SyncEngine};

use crate::context::AppContext;
use crate::message_row::MessageRow;

/// The main application window and the widgets the actions operate on.
///
/// All fields are cheaply cloneable (GTK reference-counted handles), so the
/// action and key layers can capture a clone of the whole struct.
#[derive(Clone)]
pub struct Ui {
    pub window: adw::ApplicationWindow,
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
    pub search_filter: gtk::CustomFilter,
    pub search_query: Rc<RefCell<String>>,
    /// Bumped on every preview so a stale mark-read timer can be ignored.
    pub preview_generation: Rc<Cell<u64>>,
    /// The message currently shown in the reading pane, for re-rendering.
    pub preview_row: Rc<RefCell<Option<MessageRow>>>,
    /// While set, selection changes do not trigger a reading-pane preview.
    pub suppress_preview: Rc<Cell<bool>>,
    pub reading_stack: gtk::Stack,
    pub reading_subject: gtk::Label,
    pub conversation: gtk::Box,
    /// Overlay used to preload the next conversation without disturbing the
    /// currently displayed one.
    pub preview_overlay: gtk::Overlay,
    /// The in-flight conversation container mounted on `preview_overlay`.
    pub pending_preview: Rc<RefCell<Option<gtk::Widget>>>,
    pub search_bar: gtk::SearchBar,
    pub search_entry: gtk::SearchEntry,
    pub search_toggle: gtk::ToggleButton,
}

impl Ui {
    pub fn new(
        app: &adw::Application,
        db: Db,
        engine: SyncEngine,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let status_label = gtk::Label::builder().label("Ready").xalign(0.0).build();
        status_label.add_css_class("dim-label");
        status_label.add_css_class("caption");

        let (header, search_toggle, window_title) = build_header();
        let folder_content = gtk::Box::new(Orientation::Vertical, 0);
        folder_content.set_margin_top(6);
        folder_content.set_margin_bottom(6);
        populate_folders(&folder_content, &db);
        let folder_pane = build_folder_pane(&folder_content);
        let reading = build_reading_pane(&status_label);

        // Message list: store -> filter -> selection -> view.
        let store = gio::ListStore::new::<BoxedAnyObject>();
        let search_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let preview_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let search_filter = gtk::CustomFilter::new({
            let query = search_query.clone();
            move |object| {
                let query = query.borrow();
                if query.is_empty() {
                    return true;
                }
                object
                    .downcast_ref::<BoxedAnyObject>()
                    .map(|boxed| boxed.borrow::<MessageRow>().matches(query.as_str()))
                    .unwrap_or(true)
            }
        });
        let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(search_filter.clone()));
        let selection = gtk::SingleSelection::new(Some(filter_model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let list_view = gtk::ListView::new(Some(selection.clone()), Some(build_factory()));
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

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Jmail")
            .default_width(1180)
            .default_height(760)
            .content(&toolbar)
            .build();

        // Squeeze the list/reading split into a single pane on narrow windows.
        if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 850px") {
            let breakpoint = adw::Breakpoint::new(condition);
            breakpoint.add_setter(&list_split, "collapsed", Some(&gtk::glib::Value::from(true)));
            window.add_breakpoint(breakpoint);
        }

        let ui = Self {
            window,
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
            search_filter,
            search_query,
            preview_generation,
            preview_row: Rc::new(RefCell::new(None)),
            suppress_preview: Rc::new(Cell::new(false)),
            reading_stack: reading.stack,
            reading_subject: reading.subject,
            conversation: reading.conversation,
            preview_overlay: reading.overlay,
            pending_preview: Rc::new(RefCell::new(None)),
            search_bar,
            search_entry,
            search_toggle,
        };

        crate::context::set(AppContext {
            db: ui.db.clone(),
            engine: ui.engine.clone(),
            runtime: ui.runtime.clone(),
        });

        ui.connect_widgets();
        ui.reload_messages();
        crate::actions::install_window_actions(&ui);
        crate::keys::install(&ui);
        ui
    }

    /// Start a sync worker for every enabled account.
    pub fn start_all_accounts(&self) {
        let accounts = match self.db.accounts() {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::error!(%error, "failed to list accounts");
                return;
            }
        };
        for account in accounts {
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
            Event::MessagesChanged { .. } => {
                self.reload_messages();
                crate::actions::reload_preview(self);
            }
            Event::SyncStarted { .. } => {
                self.status_label.set_text("Syncing…");
            }
            Event::SyncFinished { .. } => {
                self.status_label.set_text("Up to date");
            }
            Event::Error { message, .. } => {
                tracing::warn!(%message, "sync error");
                self.status_label.set_text(&format!("Sync error: {message}"));
            }
            Event::NewInboxMessage { .. } => {}
        }
    }

    /// Rebuild the account/folder pane from the cache.
    pub fn reload_folders(&self) {
        while let Some(child) = self.folder_content.first_child() {
            self.folder_content.remove(&child);
        }
        populate_folders(&self.folder_content, &self.db);
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Reload the message list from the cache (unified inbox, newest first),
    /// preserving the selection where possible.
    pub fn reload_messages(&self) {
        let previous_id = self.selected_message_id();
        let previous_position = self.selection.selected();

        self.store.remove_all();
        match self.db.unified_inbox(500) {
            Ok(messages) => {
                for message in &messages {
                    self.store
                        .append(&BoxedAnyObject::new(MessageRow::from_message(message)));
                }
            }
            Err(error) => tracing::error!(%error, "failed to load messages"),
        }

        // Keep the same message selected if it is still present; otherwise fall
        // back to the same position so a deleted row selects the next one.
        if let Some(id) = previous_id {
            if let Some(position) = self.position_of_message(id) {
                self.select_quietly(position);
                return;
            }
        }
        if previous_position != u32::MAX {
            let count = self.selection.n_items();
            if count > 0 {
                self.select_quietly(previous_position.min(count - 1));
            }
        }
    }

    /// Select a row without triggering the reading-pane preview. Used when a
    /// model rebuild or in-place row update would otherwise fire a redundant
    /// re-render (and drop the selection).
    pub(crate) fn select_quietly(&self, position: u32) {
        self.suppress_preview.set(true);
        self.selection.set_selected(position);
        self.suppress_preview.set(false);
    }

    fn selected_message_id(&self) -> Option<i64> {
        let item = self.selection.selected_item()?;
        let boxed = item.downcast::<BoxedAnyObject>().ok()?;
        let id = boxed.borrow::<MessageRow>().id;
        Some(id)
    }

    fn position_of_message(&self, id: i64) -> Option<u32> {
        for position in 0..self.selection.n_items() {
            let Some(item) = self.selection.item(position) else {
                continue;
            };
            let Ok(boxed) = item.downcast::<BoxedAnyObject>() else {
                continue;
            };
            if boxed.borrow::<MessageRow>().id == id {
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
            self.search_bar
                .connect_search_mode_enabled_notify(move |bar| {
                    toggle.set_active(bar.is_search_mode());
                });
        }
        // Live filtering.
        {
            let query = self.search_query.clone();
            let filter = self.search_filter.clone();
            self.search_entry.connect_search_changed(move |entry| {
                *query.borrow_mut() = entry.text().to_lowercase();
                filter.changed(gtk::FilterChange::Different);
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
    }
}

struct ReadingPane {
    root: gtk::Widget,
    stack: gtk::Stack,
    subject: gtk::Label,
    conversation: gtk::Box,
    overlay: gtk::Overlay,
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

/// (Re)build the account and folder rows inside `content`.
fn populate_folders(content: &gtk::Box, db: &Db) {
    let joined = folder_list();
    joined.append(&folder_row("mail-unread-symbolic", "Joined Inbox", None));
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
            for account in &accounts {
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
                        list.append(&folder_row(
                            folder_icon(folder.special_use),
                            &folder.name,
                            None,
                        ));
                    }
                }
                content.append(&list);
            }
        }
        Err(error) => tracing::error!(%error, "failed to load accounts"),
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
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    if let Some(count) = count {
        let label = gtk::Label::new(Some(count));
        label.add_css_class("dim-label");
        row.add_suffix(&label);
    }
    row
}

fn folder_icon(special_use: SpecialUse) -> &'static str {
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

fn build_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        item.set_child(Some(&build_message_row()));
    });
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

fn build_reading_pane(status_label: &gtk::Label) -> ReadingPane {
    let empty = adw::StatusPage::builder()
        .icon_name("mail-read-symbolic")
        .title("No conversation selected")
        .description("Choose a message to read it here. Conversations are shown oldest first, newest at the bottom.")
        .vexpand(true)
        .build();

    let subject = gtk::Label::new(None);
    subject.set_xalign(0.0);
    subject.set_ellipsize(gtk::pango::EllipsizeMode::End);
    subject.add_css_class("title-3");
    subject.set_margin_top(12);
    subject.set_margin_bottom(6);
    subject.set_margin_start(14);
    subject.set_margin_end(14);

    let conversation = gtk::Box::new(Orientation::Vertical, 0);
    conversation.set_valign(Align::Start);

    // While a new conversation loads, it is mounted as an overlay child at zero
    // opacity so it renders off-screen; once ready it is swapped into
    // `conversation`, keeping the previous message visible until then.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&conversation));

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&overlay)
        .vexpand(true)
        .build();

    let conversation_page = gtk::Box::new(Orientation::Vertical, 0);
    conversation_page.append(&subject);
    conversation_page.append(&gtk::Separator::new(Orientation::Horizontal));
    conversation_page.append(&scrolled);

    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&conversation_page, Some("conversation"));
    stack.set_visible_child_name("empty");
    stack.set_vexpand(true);

    let footer = gtk::Box::new(Orientation::Horizontal, 0);
    footer.set_margin_top(6);
    footer.set_margin_bottom(6);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.append(status_label);

    let root = gtk::Box::new(Orientation::Vertical, 0);
    root.append(&stack);
    root.append(&gtk::Separator::new(Orientation::Horizontal));
    root.append(&footer);

    ReadingPane {
        root: root.upcast(),
        stack,
        subject,
        conversation,
        overlay,
    }
}
