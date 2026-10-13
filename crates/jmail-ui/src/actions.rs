use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::glib::BoxedAnyObject;

use jmail_core::{
    AccountId, Attachment, Db, Flags, FolderId, MessageId, MoveTarget, OpPayload, QueuedMessage,
    SpecialUse, SyncCommand,
    ThreadFilter,
};

use crate::message_row::{format_timestamp, MessageRow};
use crate::window::Ui;

/// Application-level actions (`app.settings`, `app.about`).
pub fn install_app_actions(app: &adw::Application) {
    let settings = gio::SimpleAction::new("settings", None);
    {
        let app = app.clone();
        settings.connect_activate(move |_, _| {
            crate::settings::present(app.active_window().as_ref());
        });
    }
    app.add_action(&settings);

    let about = gio::SimpleAction::new("about", None);
    {
        let app = app.clone();
        about.connect_activate(move |_, _| {
            crate::settings::about(app.active_window().as_ref());
        });
    }
    app.add_action(&about);
}

/// Window-level actions (`win.*`), most of which are bound to keyboard
/// shortcuts in [`crate::keys`].
pub fn install_window_actions(ui: &Ui) {
    // Navigation.
    on(ui, "next-message", {
        let ui = ui.clone();
        move || move_selection(&ui, 1)
    });
    on(ui, "prev-message", {
        let ui = ui.clone();
        move || move_selection(&ui, -1)
    });
    on(ui, "open-pane", {
        let ui = ui.clone();
        move || open_pane(&ui)
    });
    on(ui, "open-window", {
        let ui = ui.clone();
        move || open_window(&ui)
    });
    on(ui, "back", {
        let ui = ui.clone();
        move || close_or_back(&ui)
    });

    // Compose.
    on(ui, "compose", {
        let ui = ui.clone();
        move || crate::compose::present(&ui, crate::compose::Mode::New, None)
    });
    on(ui, "discard-reply", {
        let ui = ui.clone();
        move || ui.clear_inline()
    });
    for (name, mode) in [
        ("reply", crate::compose::Mode::Reply),
        ("reply-all", crate::compose::Mode::ReplyAll),
        ("forward", crate::compose::Mode::Forward),
    ] {
        let handle = ui.clone();
        on(ui, name, move || match reply_row(&handle) {
            Some(row) => {
                let target = reply_target_id(&handle, &row);
                crate::compose::present_inline(&handle, &handle.reading, mode, target);
            }
            None => handle.status_label.set_text("Select a message first"),
        });
    }

    // Message actions.
    on(ui, "archive", {
        let ui = ui.clone();
        move || move_selected(&ui, MoveTarget::Special(SpecialUse::Archive), "Archived")
    });
    on(ui, "delete", {
        let ui = ui.clone();
        move || move_selected(&ui, MoveTarget::Special(SpecialUse::Trash), "Deleted")
    });
    on(ui, "star", {
        let ui = ui.clone();
        move || toggle_star(&ui)
    });
    on(ui, "mark-read", {
        let ui = ui.clone();
        move || set_unread(&ui, false)
    });
    on(ui, "mark-unread", {
        let ui = ui.clone();
        move || set_unread(&ui, true)
    });
    on(ui, "select", {
        let ui = ui.clone();
        move || ui.status_label.set_text("Selected")
    });
    on(ui, "move", {
        let ui = ui.clone();
        move || open_move_picker(&ui)
    });
    on(ui, "label", {
        let ui = ui.clone();
        move || ui.status_label.set_text("Label as (not implemented yet)")
    });
    on(ui, "send", {
        let ui = ui.clone();
        move || ui.status_label.set_text("Send (only available while composing)")
    });

    // Search.
    on(ui, "search", {
        let ui = ui.clone();
        move || open_search(&ui)
    });

    // Folder jumps.
    let views: [(&str, &str, Option<SpecialUse>, bool); 4] = [
        ("go-inbox", "Joined Inbox", None, false),
        ("go-sent", "Sent", Some(SpecialUse::Sent), false),
        ("go-drafts", "Drafts", Some(SpecialUse::Drafts), false),
        ("go-all", "All Mail", None, true),
    ];
    for (name, label, special, all) in views {
        let handle = ui.clone();
        on(ui, name, move || {
            let filter = if all {
                ThreadFilter::All
            } else if let Some(special) = special {
                first_folder(&handle.db, special)
                    .map(ThreadFilter::Folder)
                    .unwrap_or(ThreadFilter::Unified)
            } else {
                ThreadFilter::Unified
            };
            crate::window::set_view(&handle, label, filter);
        });
    }

    // Help dialog.
    on(ui, "help", {
        let ui = ui.clone();
        move || crate::settings::shortcuts(Some(&ui.window))
    });
}

/// Register an action on the window.
fn on<F: Fn() + 'static>(ui: &Ui, name: &'static str, f: F) {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| f());
    ui.window.add_action(&action);
}

/// The currently selected message, if any.
fn selected_row(ui: &Ui) -> Option<MessageRow> {
    let item = ui.selection.selected_item()?;
    let boxed = item.downcast::<BoxedAnyObject>().ok()?;
    let row = boxed.borrow::<MessageRow>().clone();
    Some(row)
}

/// The message at a position in the (filtered) list model.
fn row_at(ui: &Ui, position: u32) -> Option<MessageRow> {
    let item = ui.selection.item(position)?;
    let boxed = item.downcast::<BoxedAnyObject>().ok()?;
    let row = boxed.borrow::<MessageRow>().clone();
    Some(row)
}

/// The conversation a reply/reply-all/forward should target: the one shown in
/// the reading pane when it is visible, otherwise the list selection. Replying
/// to what you are reading (rather than a possibly-stale selection) is what the
/// reader expects, and it is also what the reply bar belongs to.
fn reply_row(ui: &Ui) -> Option<MessageRow> {
    if ui.list_split.shows_content() {
        if let Some(row) = ui.reading.previewed() {
            return Some(row);
        }
    }
    selected_row(ui)
}

/// Select and preview the conversation at `position`. The context menu uses this
/// for Reply/Forward so the composer targets the clicked chain (and the reading
/// pane follows it).
pub(crate) fn preview_position(ui: &Ui, position: u32) {
    if let Some(row) = row_at(ui, position) {
        ui.select_quietly(position);
        begin_preview(ui, &ui.reading, &row, true);
    }
}

/// The first folder with the given special use across all accounts.
fn first_folder(db: &Db, special: SpecialUse) -> Option<jmail_core::FolderId> {
    for account in db.accounts().ok()? {
        if let Some(folder) = db
            .folders(account.id)
            .ok()?
            .into_iter()
            .find(|folder| folder.special_use == special)
        {
            return Some(folder.id);
        }
    }
    None
}

/// Whether the row represents a draft.
fn is_draft(ui: &Ui, row: &MessageRow) -> bool {
    ui.db.get_message(MessageId(row.id))
        .ok()
        .flatten()
        .map(|message| message.flags.draft)
        .unwrap_or(false)
}

/// Which message in the conversation a reply/forward should target: the latest
/// message in the thread.
fn reply_target_id(ui: &Ui, row: &MessageRow) -> Option<MessageId> {
    let messages = ui
        .db
        .thread_messages(AccountId(row.account_id), &row.thread_id, 50)
        .unwrap_or_default();
    jmail_core::select_reply_target(&messages)
        .map(|message| message.id)
        .or(Some(MessageId(row.id)))
}

/// Open a draft in the composer, fetching its body first if needed.
fn open_draft(ui: &Ui, row: &MessageRow) {
    let message = MessageId(row.id);
    let has_body = ui
        .db
        .message_body(message)
        .ok()
        .flatten()
        .and_then(|body| body.text)
        .is_some();
    if has_body {
        crate::compose::present(ui, crate::compose::Mode::Draft, Some(message));
        return;
    }
    *ui.pending_draft.borrow_mut() = Some(message);
    let account = AccountId(row.account_id);
    if ui.engine.is_running(account) {
        ui.engine
            .send(account, SyncCommand::FetchBodies { messages: vec![message] });
    }
    ui.status_label.set_text("Loading draft…");
}

/// Move the selection by `delta` through the filtered list.
fn move_selection(ui: &Ui, delta: i32) {
    let count = ui.selection.n_items();
    if count == 0 {
        return;
    }
    let current = ui.selection.selected();
    let next = if current == u32::MAX {
        if delta > 0 {
            0
        } else {
            count - 1
        }
    } else {
        (current as i64 + delta as i64).clamp(0, count as i64 - 1) as u32
    };
    ui.selection.set_selected(next);
    ui.list_view.scroll_to(
        next,
        gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
        None,
    );
}

/// A reusable conversation view: the widgets and per-pane state used by both the
/// main reading pane and detached message windows. All rendering and inline
/// composing goes through this, so the two stay identical.
/// What is currently loaded in a pane's WebView: the per-message body keys, the
/// theme, and the reading settings. A redundant re-render (e.g. after a flag
/// change) is skipped so the scroll position is not reset.
type RenderKey = (Vec<(i64, bool)>, bool, bool, bool);

#[derive(Clone)]
pub(crate) struct ReadingPane {
    pub root: gtk::Widget,
    pub empty: adw::StatusPage,
    pub subject: gtk::Label,
    /// The vertical box holding the body WebView, the reply bar and the
    /// composer. Also used to read the active theme colours.
    pub conversation: gtk::Box,
    /// The single WebView that renders the whole conversation and scrolls its
    /// own content. Sizing it to the pane (rather than to the content) keeps
    /// every compositing surface within GPU limits, however long the mail.
    pub webview: webkit6::WebView,
    pub reply_bar: gtk::Box,
    pub compose_slot: gtk::Box,
    pub status_label: gtk::Label,
    /// The conversation currently shown in this pane, for re-rendering.
    pub row: Rc<RefCell<Option<MessageRow>>>,
    /// Bumped on every render so a stale mark-read timer can be ignored.
    pub generation: Rc<Cell<u64>>,
    /// The correlation id of the inline composer open in this pane.
    pub inline_composer: Rc<Cell<Option<u64>>>,
    /// Key of the conversation currently loaded into `webview`.
    last: Rc<RefCell<Option<RenderKey>>>,
    /// Composes a reply/forward for this pane (installed once a `Ui` exists).
    on_reply: Rc<RefCell<Option<Rc<dyn Fn(crate::compose::Mode)>>>>,
    /// Opens or saves an attachment, by `(message, part, save)` (installed once
    /// a `Ui` exists, since it needs the database and engine).
    on_attachment: Rc<RefCell<Option<Rc<dyn Fn(i64, u32, bool)>>>>,
}

impl ReadingPane {
    pub(crate) fn new(status_label: gtk::Label) -> Rc<Self> {
        let empty = adw::StatusPage::builder()
            .icon_name("mail-read-symbolic")
            .title("No conversation selected")
            .description("Choose a message to read it here. Conversations are shown oldest first, newest at the bottom.")
            .vexpand(true)
            .build();
        // Opaque so the pre-warmed (and initially black) body WebView mounted
        // behind it is never visible while it paints for the first time.
        empty.add_css_class("reading-empty");
        empty.set_hexpand(true);
        empty.set_vexpand(true);

        let subject = gtk::Label::new(None);
        subject.set_xalign(0.0);
        subject.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subject.add_css_class("title-3");
        subject.set_margin_top(12);
        subject.set_margin_bottom(6);
        subject.set_margin_start(14);
        subject.set_margin_end(14);

        let conversation = gtk::Box::new(gtk::Orientation::Vertical, 0);
        conversation.set_vexpand(true);

        // The whole conversation renders into one WebView that scrolls its own
        // content. Because the widget is only ever the size of the pane, a very
        // long message can never exceed the GPU texture/GBM ceiling (which is
        // what used to leave a white body when each message was sized to its
        // full content height).
        let webview = webkit6::WebView::new();
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&webview) {
            settings.set_enable_javascript(true);
            settings.set_enable_javascript_markup(false);
            settings.set_enable_developer_extras(false);
            settings.set_auto_load_images(true);
        }
        let on_attachment: Rc<RefCell<Option<Rc<dyn Fn(i64, u32, bool)>>>> =
            Rc::new(RefCell::new(None));
        install_webview_policy(&webview, on_attachment.clone());
        // Paint the active theme background from the start so revealing the view
        // (even before its first body paints) never flashes white or black.
        webkit6::prelude::WebViewExt::set_background_color(&webview, &html_theme(&conversation).bg);
        webview.set_hexpand(true);
        webview.set_vexpand(true);

        // An inline reply composer is appended to `compose_slot`, beneath the
        // pinned reply bar.
        let compose_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let reply_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        reply_bar.set_halign(gtk::Align::Center);
        reply_bar.set_margin_top(12);
        reply_bar.set_margin_bottom(12);
        reply_bar.set_margin_start(14);
        reply_bar.set_margin_end(14);
        reply_bar.add_css_class("reply-bar");
        reply_bar.set_visible(false);

        let on_reply: Rc<RefCell<Option<Rc<dyn Fn(crate::compose::Mode)>>>> =
            Rc::new(RefCell::new(None));
        for (icon, label, mode) in [
            (
                "mail-reply-sender-symbolic",
                "Reply",
                crate::compose::Mode::Reply,
            ),
            (
                "mail-reply-all-symbolic",
                "Reply all",
                crate::compose::Mode::ReplyAll,
            ),
            (
                "mail-forward-symbolic",
                "Forward",
                crate::compose::Mode::Forward,
            ),
        ] {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&gtk::Image::from_icon_name(icon));
            content.append(&gtk::Label::new(Some(label)));
            let button = gtk::Button::builder().child(&content).build();
            button.add_css_class("pill");
            let on_reply = on_reply.clone();
            button.connect_clicked(move |_| {
                let callback = on_reply.borrow().clone();
                if let Some(callback) = callback {
                    callback(mode);
                }
            });
            reply_bar.append(&button);
        }

        conversation.append(&webview);
        conversation.append(&reply_bar);
        conversation.append(&compose_slot);

        let conversation_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        conversation_page.append(&subject);
        conversation_page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        conversation_page.append(&conversation);

        // The conversation page stays mounted (and mapped) behind the empty
        // state, so a preloaded body WebView can warm up its compositing surface
        // before the first conversation is opened.
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&conversation_page));
        overlay.add_overlay(&empty);
        overlay.set_vexpand(true);

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        footer.set_margin_top(6);
        footer.set_margin_bottom(6);
        footer.set_margin_start(12);
        footer.set_margin_end(12);
        footer.append(&status_label);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&overlay);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&footer);

        Rc::new(Self {
            root: root.upcast(),
            empty,
            subject,
            conversation,
            webview,
            reply_bar,
            compose_slot,
            status_label,
            row: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            inline_composer: Rc::new(Cell::new(None)),
            last: Rc::new(RefCell::new(None)),
            on_reply,
            on_attachment,
        })
    }

    /// Install the callback used by this pane's Reply / Reply all / Forward
    /// buttons. Called once a `Ui` exists, since composing needs the engine.
    pub(crate) fn set_on_reply(&self, callback: Rc<dyn Fn(crate::compose::Mode)>) {
        *self.on_reply.borrow_mut() = Some(callback);
    }

    /// Install the callback used when an attachment chip in the body is
    /// clicked: `(message id, part, save)`. Called once a `Ui` exists.
    pub(crate) fn set_on_attachment(&self, callback: Rc<dyn Fn(i64, u32, bool)>) {
        *self.on_attachment.borrow_mut() = Some(callback);
    }

    pub(crate) fn previewed(&self) -> Option<MessageRow> {
        self.row.borrow().clone()
    }

    pub(crate) fn set_inline_composer(&self, id: u64) {
        self.inline_composer.set(Some(id));
    }

    /// Remove this pane's inline composer and restore its reply bar.
    pub(crate) fn clear_inline(&self, ui: &Ui) {
        if let Some(id) = self.inline_composer.take() {
            ui.unregister_compose(id);
        }
        while let Some(child) = self.compose_slot.first_child() {
            self.compose_slot.remove(&child);
        }
        self.reply_bar.set_visible(self.row.borrow().is_some());
    }

    /// Detach an inline composer that is mid-send without unregistering its
    /// result handler (so a failure can re-present it), restoring the reply bar.
    pub(crate) fn detach_inline(&self) {
        while let Some(child) = self.compose_slot.first_child() {
            self.compose_slot.remove(&child);
        }
        self.inline_composer.set(None);
        self.reply_bar.set_visible(self.row.borrow().is_some());
    }

    /// Scroll the body WebView to the bottom (to show the latest message while
    /// an inline composer is open).
    pub(crate) fn scroll_to_bottom(&self) {
        use webkit6::prelude::*;

        let view = self.webview.clone();
        gtk::glib::idle_add_local_once(move || {
            view.evaluate_javascript(
                "window.scrollTo(0, document.body ? document.body.scrollHeight : 0)",
                None,
                None,
                None::<&gtk::gio::Cancellable>,
                |_| {},
            );
        });
    }
}

/// Preview the message at `position` in the reading pane (hover / j / k).
pub(crate) fn request_preview(ui: &Ui, position: u32) {
    if ui.suppress_preview.get() || ui.list_split.is_collapsed() {
        return;
    }
    let Some(row) = row_at(ui, position) else {
        return;
    };
    begin_preview(ui, &ui.reading, &row, false);
}

/// Show whatever now occupies `position` after the selected conversation was
/// removed (deleted/moved). In the narrow layout `open` brings up the reading
/// pane so it is not left showing the message that was just deleted.
pub(crate) fn preview_after_removal(ui: &Ui, position: u32, open: bool) {
    let Some(row) = row_at(ui, position) else {
        clear_preview(ui, &ui.reading);
        return;
    };
    begin_preview(ui, &ui.reading, &row, open);
}

/// Reset a reading pane to its empty state, e.g. when the last conversation in a
/// folder is deleted.
pub(crate) fn clear_preview(ui: &Ui, pane: &ReadingPane) {
    *pane.row.borrow_mut() = None;
    *pane.last.borrow_mut() = None;
    pane.clear_inline(ui);
    pane.empty.set_visible(true);
    pane.reply_bar.set_visible(false);
    // In the narrow layout, fall back to the list; in the wide layout the empty
    // state fills the already-visible reading pane.
    if ui.list_split.is_collapsed() {
        ui.list_split.set_show_content(false);
    }
}

/// Single click: preview and make sure the reading pane is shown.
fn open_pane(ui: &Ui) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    if is_draft(ui, &row) {
        open_draft(ui, &row);
        return;
    }
    begin_preview(ui, &ui.reading, &row, true);
}

/// `o` / Enter: open the conversation in its own window.
fn open_window(ui: &Ui) {
    let Some(row) = selected_row(ui) else {
        ui.status_label.set_text("No message selected");
        return;
    };
    if is_draft(ui, &row) {
        open_draft(ui, &row);
        return;
    }
    open_message_window(ui, &row);
}

/// Render a conversation in `pane` and schedule marking it read after a short
/// delay.
fn begin_preview(ui: &Ui, pane: &ReadingPane, row: &MessageRow, show_content: bool) {
    // Switching to a different conversation discards any inline reply.
    let switched = pane
        .row
        .borrow()
        .as_ref()
        .is_some_and(|previous| previous.thread_id != row.thread_id);
    if switched {
        pane.clear_inline(ui);
    }

    let generation = pane.generation.get().wrapping_add(1);
    pane.generation.set(generation);
    *pane.row.borrow_mut() = Some(row.clone());

    render_preview(ui, pane, row);
    ensure_thread_bodies(ui, row);
    if show_content {
        ui.list_split.set_show_content(true);
    }
    pane.status_label
        .set_text(&format!("Reading: {}", row.subject));

    if row.unread_count > 0 {
        let handle = ui.clone();
        let row = row.clone();
        let pane = pane.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if pane.generation.get() == generation {
                mark_thread_read(&handle, &row);
            }
        });
    }
}

fn render_preview(ui: &Ui, pane: &ReadingPane, row: &MessageRow) {
    use webkit6::prelude::*;

    let theme = html_theme(&pane.conversation);
    pane.subject.set_label(&row.subject);
    pane.empty.set_visible(false);
    // Keep the reply bar hidden while an inline composer is open; it comes back
    // when the composer is discarded or sent.
    pane.reply_bar.set_visible(pane.inline_composer.get().is_none());

    let data = thread_render_data(&ui.db, row);
    let remote = ui.remote_images.get();
    let readable = ui.readable_colors.get();
    let key: RenderKey = (
        data.iter().map(|message| (message.id, message.has_body)).collect(),
        theme.dark,
        remote,
        readable,
    );
    // Bodies arrive one at a time, and the setting/theme can change; only
    // rebuild the document when it actually differs, so scrolling is not reset
    // by an unrelated refresh.
    if pane.last.borrow().as_ref() == Some(&key) {
        return;
    }
    *pane.last.borrow_mut() = Some(key);

    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&pane.webview) {
        settings.set_auto_load_images(remote);
    }
    pane.webview.set_background_color(&theme.bg);
    pane.webview
        .load_html(&conversation_html(&theme, &data, readable), None);
}

/// The bits of one message the reading pane needs to render.
struct RenderedMessage {
    id: i64,
    has_body: bool,
    sender: String,
    date: String,
    text: String,
    html: Option<String>,
    /// Display name of the message this one replies to, when known.
    reply_to: Option<String>,
    attachments: Vec<Attachment>,
}

/// Build render data for every message in `row`'s conversation, oldest first.
fn thread_render_data(db: &Db, row: &MessageRow) -> Vec<RenderedMessage> {
    let messages = db
        .thread_messages(AccountId(row.account_id), &row.thread_id, 50)
        .unwrap_or_default();
    if messages.is_empty() {
        return vec![RenderedMessage {
            id: row.id,
            has_body: false,
            sender: row.sender.clone(),
            date: row.date_label.clone(),
            text: row.snippet.clone(),
            html: None,
            reply_to: None,
            attachments: Vec::new(),
        }];
    }

    let senders = sender_by_message_id(&messages, &my_addresses(db));

    messages
        .iter()
        .map(|message| {
            let body = db.message_body(message.id).ok().flatten();
            let has_body = body
                .as_ref()
                .map(|body| body.text.is_some() || body.html.is_some())
                .unwrap_or(false);
            let (text, html) = match body {
                Some(body) => (
                    body.text.unwrap_or_else(|| message.snippet.clone()),
                    body.html,
                ),
                None => (message.snippet.clone(), None),
            };
            let reply_to = reply_to_name(&senders, &message.in_reply_to);
            RenderedMessage {
                id: message.id.0,
                has_body,
                sender: message.from.display().to_string(),
                date: format_timestamp(message.date.unix_timestamp()),
                text,
                html,
                reply_to,
                attachments: db.attachments(message.id).unwrap_or_default(),
            }
        })
        .collect()
}

/// Map every `Message-ID` in a conversation to its sender, so a reply can name
/// who it answers. Messages sent by one of `my_addresses` display as "You".
fn sender_by_message_id(
    messages: &[jmail_core::Message],
    my_addresses: &[String],
) -> HashMap<String, String> {
    let mut senders = HashMap::new();
    for message in messages {
        let id = jmail_core::threading::normalize_message_id(&message.message_id);
        if !id.is_empty() {
            let is_mine = my_addresses
                .iter()
                .any(|mine| mine.eq_ignore_ascii_case(&message.from.email));
            let display = if is_mine {
                "You".to_string()
            } else {
                message.from.display().to_string()
            };
            senders.entry(id).or_insert(display);
        }
    }
    senders
}

/// The account addresses belonging to the user.
fn my_addresses(db: &Db) -> Vec<String> {
    db.accounts()
        .unwrap_or_default()
        .iter()
        .map(|account| account.email.clone())
        .collect()
}

fn reply_to_name(senders: &HashMap<String, String>, in_reply_to: &str) -> Option<String> {
    let parent = jmail_core::threading::normalize_message_id(in_reply_to);
    if parent.is_empty() {
        None
    } else {
        Some(
            senders
                .get(&parent)
                .cloned()
                .unwrap_or_else(|| "an earlier message".to_string()),
        )
    }
}

/// Load a blank themed page into the reading pane's WebView so WebKit primes
/// its compositing surface before the first conversation is opened. The empty
/// state sits opaquely on top, so the warm-up page is never visible.
pub(crate) fn preload_reading_pane(ui: &Ui) {
    use webkit6::prelude::*;

    let pane = &ui.reading;
    // Never clobber a conversation that is already on screen.
    if pane.last.borrow().is_some() {
        return;
    }
    let theme = html_theme(&pane.conversation);
    pane.webview.set_background_color(&theme.bg);
    pane.webview.load_html(&themed_html("", &theme), None);
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Intercept link clicks in the message body. External links are handed to the
/// desktop instead of navigating the view; our own `jmail-attachment:` links open
/// or save an attachment. Page-initiated navigation and popups are blocked too.
fn install_webview_policy(
    view: &webkit6::WebView,
    on_attachment: Rc<RefCell<Option<Rc<dyn Fn(i64, u32, bool)>>>>,
) {
    use webkit6::prelude::*;

    view.connect_decide_policy(move |_, decision, decision_type| {
        // Only user link clicks are intercepted. Everything else (in particular
        // the programmatic `load_html` that renders a body) is left to WebKit,
        // otherwise the message itself would be blocked.
        if decision_type != webkit6::PolicyDecisionType::NavigationAction {
            return false;
        }
        let Some(action) = decision
            .downcast_ref::<webkit6::NavigationPolicyDecision>()
            .and_then(|nav| nav.navigation_action())
        else {
            return false;
        };
        if action.navigation_type() != webkit6::NavigationType::LinkClicked {
            return false;
        }
        let Some(uri) = action.request().and_then(|request| request.uri()) else {
            decision.ignore();
            return true;
        };
        if let Some((message, part, save)) = parse_attachment_uri(&uri) {
            let callback = on_attachment.borrow().clone();
            if let Some(callback) = callback {
                callback(message, part, save);
            }
            decision.ignore();
            return true;
        }
        open_external(&uri);
        decision.ignore();
        true
    });

    view.connect_create(|_, action| {
        if let Some(uri) = action.request().and_then(|request| request.uri()) {
            open_external(&uri);
        }
        None
    });
}

/// Parse a body-attachment link, e.g. `jmail-attachment:open/42/2`, into
/// `(message id, part, save)`.
fn parse_attachment_uri(uri: &str) -> Option<(i64, u32, bool)> {
    let rest = uri.strip_prefix("jmail-attachment:")?;
    let rest = rest.trim_start_matches("//");
    let mut parts = rest.splitn(3, '/');
    let save = match parts.next()? {
        "open" => false,
        "save" => true,
        _ => return None,
    };
    let message = parts.next()?.parse().ok()?;
    let part = parts.next()?.parse().ok()?;
    Some((message, part, save))
}

/// Open a link with the desktop's URI handler (Android/iOS-style handoff via
/// `GtkUriLauncher`, which uses the OpenURI portal inside Flatpak).
fn open_external(uri: &str) {
    if !(uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("mailto:")) {
        tracing::debug!(%uri, "ignoring unsupported link scheme");
        return;
    }
    let uri = uri.to_string();
    gtk::UriLauncher::new(&uri).launch(
        None::<&gtk::Window>,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Err(error) = result {
                tracing::warn!(%error, %uri, "could not open link");
            }
        },
    );
}

/// Entry point for an attachment link inside a rendered conversation body.
pub(crate) fn handle_attachment(ui: &Ui, message: MessageId, part: u32, save: bool) {
    if save {
        save_attachment(ui, message, part);
    } else {
        open_attachment(ui, message, part);
    }
}

/// Resolve an attachment's filename from the cache, falling back to the part
/// number if it is unknown.
fn attachment_filename(ui: &Ui, message: MessageId, part: u32) -> String {
    ui.db
        .attachments(message)
        .ok()
        .and_then(|attachments| {
            attachments
                .into_iter()
                .find(|attachment| attachment.part.parse::<u32>().ok() == Some(part))
        })
        .map(|attachment| attachment.filename)
        .filter(|filename| !filename.is_empty())
        .unwrap_or_else(|| format!("attachment-{part}"))
}

/// Open an attachment with the desktop's default application. The bytes are
/// fetched into a per-message cache file (reused if it is already there) and
/// handed off via `open_file`, so PDFs land in the document viewer instead of a
/// save dialog.
fn open_attachment(ui: &Ui, message: MessageId, part: u32) {
    let Some(account) = ui
        .db
        .get_message(message)
        .ok()
        .flatten()
        .map(|message| message.account_id)
    else {
        ui.status_label.set_text("Message is not available");
        return;
    };
    let filename = attachment_filename(ui, message, part);
    let Some(path) = attachment_cache_path(message, part, &filename) else {
        ui.status_label.set_text("Could not prepare attachment");
        return;
    };
    if path.exists() {
        open_file(&path);
        return;
    }
    if !ui.engine.is_running(account) {
        ui.status_label
            .set_text("Account is offline; reconnect to open attachments");
        return;
    }
    ui.engine.send(
        account,
        SyncCommand::FetchAttachment {
            message,
            part,
            destination: path,
            open: true,
        },
    );
    ui.status_label.set_text("Opening attachment…");
}

/// Where an opened attachment is cached: a stable per-message path under the
/// user cache directory, so reopening is instant and needs no re-download.
fn attachment_cache_path(
    message: MessageId,
    part: u32,
    filename: &str,
) -> Option<std::path::PathBuf> {
    let name = if filename.is_empty() {
        format!("attachment-{part}")
    } else {
        filename.to_string()
    };
    let dir = gtk::glib::user_cache_dir()
        .join("jmail")
        .join("attachments");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%error, dir = %dir.display(), "could not create attachment cache directory");
        return None;
    }
    Some(dir.join(format!("{}-{part}-{}", message.0, safe_filename(&name))))
}

/// Keep a user-supplied filename safe to join onto our cache directory.
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Ask where to save an attachment, then have the account's worker fetch and
/// write it. Result arrives as `Event::AttachmentSaved`.
fn save_attachment(ui: &Ui, message: MessageId, part: u32) {
    let Some(account) = ui
        .db
        .get_message(message)
        .ok()
        .flatten()
        .map(|message| message.account_id)
    else {
        ui.status_label.set_text("Message is not available");
        return;
    };
    if !ui.engine.is_running(account) {
        ui.status_label
            .set_text("Account is offline; reconnect to save attachments");
        return;
    }
    let suggested = attachment_filename(ui, message, part);

    let dialog = gtk::FileDialog::builder()
        .title("Save attachment")
        .initial_name(&suggested)
        .build();
    let ui = ui.clone();
    dialog.save(
        Some(&ui.window),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let Ok(file) = result else {
                return;
            };
            let Some(path) = file.path() else {
                ui.status_label.set_text("Could not save attachment");
                return;
            };
            ui.engine.send(
                account,
                SyncCommand::FetchAttachment {
                    message,
                    part,
                    destination: path,
                    open: false,
                },
            );
            ui.status_label.set_text("Saving attachment…");
        },
    );
}

/// Open a saved file with the desktop's default application. Uses
/// `GtkFileLauncher`, which routes through the OpenURI portal (and the document
/// portal for the file) inside a Flatpak sandbox — unlike a plain
/// `g_app_info_launch_default_for_uri`, which can hand the host app a path it
/// cannot read.
pub(crate) fn open_file(path: &std::path::Path) {
    let file = gtk::gio::File::for_path(path);
    let launcher = gtk::FileLauncher::new(Some(&file));
    let path = path.to_path_buf();
    launcher.launch(
        None::<&gtk::Window>,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Err(error) = result {
                tracing::warn!(%error, path = %path.display(), "could not open attachment");
            }
        },
    );
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Wrap each top-level `<blockquote>` in a collapsible `<details>`.
fn collapse_quotes(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() + 64);
    let mut cursor = 0;
    let mut depth: i32 = 0;
    while cursor < html.len() {
        let Some(rel) = lower[cursor..].find('<') else {
            out.push_str(&html[cursor..]);
            break;
        };
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        let Some(end) = lower[start..].find('>').map(|gt| start + gt + 1) else {
            out.push_str(&html[start..]);
            break;
        };
        let tag = &html[start..end];
        let lower_tag = &lower[start..end];
        if tag_name(lower_tag).as_deref() == Some("blockquote") {
            if lower_tag.starts_with("</") {
                out.push_str(tag);
                depth = (depth - 1).max(0);
                if depth == 0 {
                    out.push_str("</details>");
                }
            } else {
                if depth == 0 {
                    out.push_str(
                        "<details class=\"quote\"><summary>Show quoted text</summary>",
                    );
                }
                depth += 1;
                out.push_str(tag);
            }
        } else {
            out.push_str(tag);
        }
        cursor = end;
    }
    out
}

/// Render a plain-text body, folding runs of `>`-quoted lines into a
/// collapsible `<details>` block.
fn text_body_html(text: &str) -> String {
    const PRE: &str =
        "<pre style=\"white-space: pre-wrap; overflow-wrap: anywhere; margin: 0;\">";
    let mut out = String::new();
    let mut normal = String::new();
    let mut quoted = String::new();

    fn flush_normal(normal: &mut String, out: &mut String) {
        if !normal.is_empty() {
            out.push_str(
                "<pre style=\"white-space: pre-wrap; overflow-wrap: anywhere; margin: 0;\">",
            );
            out.push_str(&escape_html(normal));
            out.push_str("</pre>");
            normal.clear();
        }
    }
    fn flush_quoted(quoted: &mut String, out: &mut String) {
        if !quoted.is_empty() {
            out.push_str("<details class=\"quote\"><summary>Show quoted text</summary>");
            out.push_str(
                "<pre style=\"white-space: pre-wrap; overflow-wrap: anywhere; margin: 0;\">",
            );
            out.push_str(&escape_html(quoted));
            out.push_str("</pre></details>");
            quoted.clear();
        }
    }

    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('>') {
            flush_normal(&mut normal, &mut out);
            quoted.push_str(line);
        } else {
            flush_quoted(&mut quoted, &mut out);
            normal.push_str(line);
        }
    }
    flush_quoted(&mut quoted, &mut out);
    flush_normal(&mut normal, &mut out);
    if out.is_empty() {
        out.push_str(PRE);
        out.push_str("</pre>");
    }
    out
}

/// Re-render the reading pane (e.g. after a body finished fetching).
pub(crate) fn reload_preview(ui: &Ui) {
    reload_pane(ui, &ui.reading);
    for pane in ui.detached_panes.borrow().iter() {
        reload_pane(ui, pane);
    }
}

/// Re-render one pane's current conversation and request any bodies that
/// arrived since it was opened (e.g. a just-sent Sent copy).
fn reload_pane(ui: &Ui, pane: &ReadingPane) {
    if let Some(row) = pane.previewed() {
        render_preview(ui, pane, &row);
        ensure_thread_bodies(ui, &row);
    }
}

/// Ask the sync engine for the body of every message in the conversation that
/// is not cached yet.
fn ensure_thread_bodies(ui: &Ui, row: &MessageRow) {
    let account = AccountId(row.account_id);
    if !ui.engine.is_running(account) {
        return;
    }
    let Ok(messages) = ui.db.thread_messages(account, &row.thread_id, 50) else {
        return;
    };
    // One batched command for the whole conversation, so the worker fetches all
    // bodies in as few round trips as possible instead of one per message.
    let pending: Vec<MessageId> = messages
        .into_iter()
        .filter(|message| !ui.db.body_loaded(message.id).unwrap_or(false))
        .map(|message| message.id)
        .collect();
    if !pending.is_empty() {
        ui.engine.send(account, SyncCommand::FetchBodies { messages: pending });
    }
}

/// Coalesce a burst of body arrivals into a single reading-pane refresh. Body
/// fetches finish one at a time; without this the whole conversation (and its
/// WebViews) would be rebuilt once per message.
pub(crate) fn schedule_reload_preview(ui: &Ui) {
    if ui.preview_reload_pending.get() {
        return;
    }
    ui.preview_reload_pending.set(true);
    let ui = ui.clone();
    gtk::glib::timeout_add_local_once(Duration::from_millis(40), move || {
        ui.preview_reload_pending.set(false);
        reload_preview(&ui);
    });
}

/// Coalesce a burst of `MessagesChanged` events (the initial sync emits many)
/// into a single list refresh so the message rows are not rebuilt repeatedly.
pub(crate) fn schedule_messages_reload(ui: &Ui) {
    if ui.messages_reload_pending.get() {
        return;
    }
    ui.messages_reload_pending.set(true);
    let ui = ui.clone();
    gtk::glib::timeout_add_local_once(Duration::from_millis(80), move || {
        ui.messages_reload_pending.set(false);
        ui.reload_messages();
        reload_preview(&ui);
        ui.maybe_open_pending_draft();
    });
}

/// Mark every message in a conversation read, updating the cache, the list and
/// the server.
fn mark_thread_read(ui: &Ui, row: &MessageRow) {
    if row.unread_count == 0 {
        return;
    }
    let account = AccountId(row.account_id);
    if enqueue_thread_flag(ui, account, &row.thread_id, Flags::SEEN, true) {
        ui.reload_messages();
    }
}

fn close_or_back(ui: &Ui) {
    if ui.search_bar.is_search_mode() {
        ui.search_bar.set_search_mode(false);
        ui.search_toggle.set_active(false);
    } else {
        ui.list_split.set_show_content(false);
        ui.status_label.set_text("Back to list");
    }
}

fn open_search(ui: &Ui) {
    ui.search_bar.set_search_mode(true);
    ui.search_toggle.set_active(true);
    ui.search_entry.grab_focus();
}

fn toggle_star(ui: &Ui) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    // Star the whole conversation unless every message is already starred.
    let star = !(row.message_count > 0 && row.starred_count == row.message_count);
    let account = AccountId(row.account_id);
    enqueue_thread_flag(ui, account, &row.thread_id, Flags::FLAGGED, star);
    ui.reload_messages();
    ui.status_label
        .set_text(if star { "Starred" } else { "Unstarred" });
}

fn set_unread(ui: &Ui, unread: bool) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    let account = AccountId(row.account_id);
    enqueue_thread_flag(ui, account, &row.thread_id, Flags::SEEN, !unread);
    ui.reload_messages();
    ui.status_label
        .set_text(if unread { "Marked unread" } else { "Marked read" });
}

/// Move the selected conversation to a target folder. The cache is updated
/// immediately (optimistically) and the move is queued for the server, so the
/// list never waits on the network; the engine rolls it back if it ultimately
/// fails. Only messages in the folder the row represents move, so moving a
/// thread does not drag along its Sent copies.
fn move_selected(ui: &Ui, target: MoveTarget, verb: &'static str) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    let account = AccountId(row.account_id);
    let message = MessageId(row.id);
    let Some((_account, _path, folder_id, _uid, _flags)) =
        ui.db.message_location(message).ok().flatten()
    else {
        return;
    };
    let folders = ui.db.folders(account).unwrap_or_default();
    let target_id = match target {
        MoveTarget::Folder(id) => Some(id),
        MoveTarget::Special(special) => folders
            .iter()
            .find(|folder| folder.special_use == special)
            .map(|folder| folder.id),
    };
    let messages: Vec<QueuedMessage> = ui
        .db
        .thread_locations(account, &row.thread_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_id, folder, _path, _uid, _flags)| *folder == folder_id)
        .map(|(id, folder, path, uid, flags)| QueuedMessage {
            message_id: id.0,
            folder_id: folder.0,
            path,
            uid,
            flags: flags.to_i64(),
        })
        .collect();
    if messages.is_empty() {
        return;
    }
    let payload = OpPayload::Move {
        target: target_id.map(|id| id.0),
        messages,
    };
    if let Err(error) = ui.db.enqueue_op(account, &payload) {
        tracing::error!(%error, "failed to queue move");
        ui.status_label.set_text("Couldn't move conversation");
        return;
    }
    flush_outbox_if_running(ui, account);
    ui.reload_messages();
    ui.status_label.set_text(&format!("{verb} conversation"));
}

/// Wake the account's worker so it applies queued (optimistic) operations now.
fn flush_outbox_if_running(ui: &Ui, account: AccountId) {
    if ui.engine.is_running(account) {
        ui.engine.send(account, SyncCommand::FlushOutbox);
    }
}

/// Apply a flag bit (`Flags::SEEN` / `Flags::FLAGGED`) to every message in a
/// conversation: optimistically in the cache, and queued for the server.
fn enqueue_thread_flag(
    ui: &Ui,
    account: AccountId,
    thread_id: &str,
    bit: i64,
    value: bool,
) -> bool {
    let messages: Vec<QueuedMessage> = ui
        .db
        .thread_locations(account, thread_id)
        .unwrap_or_default()
        .into_iter()
        .map(|(id, folder, path, uid, flags)| QueuedMessage {
            message_id: id.0,
            folder_id: folder.0,
            path,
            uid,
            flags: flags.to_i64(),
        })
        .collect();
    if messages.is_empty() {
        return false;
    }
    let payload = OpPayload::Flags { bit, value, messages };
    match ui.db.enqueue_op(account, &payload) {
        Ok(_) => {
            flush_outbox_if_running(ui, account);
            true
        }
        Err(error) => {
            tracing::error!(%error, "failed to queue flag change");
            false
        }
    }
}

/// Present a folder picker for the selected conversation (the `v` key).
fn open_move_picker(ui: &Ui) {
    let Some(row) = selected_row(ui) else {
        ui.status_label.set_text("Select a message first");
        return;
    };
    let account = AccountId(row.account_id);
    let Ok(folders) = ui.db.folders(account) else {
        return;
    };
    let current = ui
        .db
        .message_location(MessageId(row.id))
        .ok()
        .flatten()
        .map(|(_, _, folder, _, _)| folder);
    let entries: Vec<(FolderId, String, SpecialUse)> = folders
        .into_iter()
        .filter(|folder| !folder.path.is_empty() && Some(folder.id) != current)
        .map(|folder| (folder.id, folder.name, folder.special_use))
        .collect();

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_focusable(true);
    list.add_css_class("boxed-list");
    list.set_margin_top(12);
    list.set_margin_bottom(12);
    list.set_margin_start(12);
    list.set_margin_end(12);
    let mut wired: Vec<(adw::ActionRow, FolderId)> = Vec::new();
    for (id, name, special) in entries {
        let item = adw::ActionRow::builder().title(&name).build();
        item.set_activatable(true);
        item.add_prefix(&gtk::Image::from_icon_name(crate::window::folder_icon(special)));
        list.append(&item);
        wired.push((item, id));
    }
    if wired.is_empty() {
        ui.status_label.set_text("No other folders to move to");
        return;
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let dialog = adw::Dialog::builder()
        .title("Move to folder")
        .content_width(400)
        .content_height(460)
        .child(&scrolled)
        .build();

    let chosen = Rc::new(std::cell::Cell::new(false));
    for (item, id) in wired {
        let ui = ui.clone();
        let dialog = dialog.clone();
        let chosen = chosen.clone();
        item.connect_activated(move |_| {
            // A single click can activate the row through both the row's own
            // gesture and the list; only move once.
            if chosen.replace(true) {
                return;
            }
            move_selected(&ui, MoveTarget::Folder(id), "Moved");
            dialog.close();
        });
    }

    // j/k (and arrows) move the highlight, Enter chooses, Esc cancels. The list
    // holds ActionRows, so activating the selected row runs the click handler.
    {
        let dialog = dialog.clone();
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let list = list.clone();
            controller.connect_key_pressed(move |_, key, _, _| {
                let step: i32 = match key {
                    gdk::Key::j | gdk::Key::Down => 1,
                    gdk::Key::k | gdk::Key::Up => -1,
                    gdk::Key::Return | gdk::Key::KP_Enter => {
                        if let Some(row) = list.selected_row() {
                            if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
                                adw::prelude::ActionRowExt::activate(row);
                            }
                        }
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::Escape => {
                        dialog.close();
                        return glib::Propagation::Stop;
                    }
                    _ => return glib::Propagation::Proceed,
                };
                let current = list.selected_row().map(|row| row.index()).unwrap_or(-1);
                let target = (current + step).max(0);
                if let Some(row) = list.row_at_index(target) {
                    list.select_row(Some(&row));
                    if !row.grab_focus() {
                        list.grab_focus();
                    }
                }
                glib::Propagation::Stop
            });
        }
        list.add_controller(controller);
    }
    // Focus the list and preselect the first folder so j/k work immediately.
    {
        let list = list.clone();
        dialog.connect_map(move |_| {
            if let Some(row) = list.row_at_index(0) {
                list.select_row(Some(&row));
                if !row.grab_focus() {
                    list.grab_focus();
                }
            } else {
                list.grab_focus();
            }
        });
    }
    dialog.present(Some(&ui.window));
}

/// Open a conversation in a standalone window, using a second reading pane so
/// the view (including the inline composer) is identical to the main one.
fn open_message_window(ui: &Ui, row: &MessageRow) {
    let Some(app) = ui.window.application() else {
        return;
    };

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&row.subject, &row.sender);
    header.set_title_widget(Some(&title));

    let pane = ReadingPane::new(detached_status_label());
    {
        let ui = ui.clone();
        let weak = Rc::downgrade(&pane);
        pane.set_on_reply(Rc::new(move |mode| {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            let row = pane.previewed().or_else(|| selected_row(&ui));
            if let Some(row) = row {
                let target = reply_target_id(&ui, &row);
                crate::compose::present_inline(&ui, &pane, mode, target);
            }
        }));
    }
    {
        let ui = ui.clone();
        pane.set_on_attachment(Rc::new(move |message, part, save| {
            handle_attachment(&ui, MessageId(message), part, save);
        }));
    }
    // Render into this pane without touching the main window's split layout.
    begin_preview(ui, &pane, row, false);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&pane.root));

    let window = adw::ApplicationWindow::builder()
        .application(&app)
        .title(&row.subject)
        .default_width(760)
        .default_height(640)
        .content(&toolbar)
        .build();

    let controller = gtk::EventControllerKey::new();
    {
        let window = window.clone();
        controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(controller);

    // Keep the pane (and its reply callback) alive for the window's lifetime.
    ui.detached_panes.borrow_mut().push(pane.clone());
    {
        let panes = ui.detached_panes.clone();
        let pane = pane.clone();
        window.connect_destroy(move |_| {
            panes.borrow_mut().retain(|p| !Rc::ptr_eq(p, &pane));
        });
    }

    window.present();
    mark_thread_read(ui, row);
}

/// A dim status line for a detached reading pane.
fn detached_status_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .label("Ready")
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label
}

/// Palette injected into HTML bodies so they match the active GTK theme.
struct HtmlTheme {
    bg: gtk::gdk::RGBA,
    fg: String,
    accent: String,
    dark: bool,
}

/// Read the current theme colours from a themed widget in the reading pane.
#[allow(deprecated)]
fn html_theme(widget: &impl IsA<gtk::Widget>) -> HtmlTheme {
    let style = widget.style_context();
    let fg_rgba = style
        .lookup_color("window_fg_color")
        .unwrap_or_else(|| widget.color());
    let bg_rgba = style
        .lookup_color("window_bg_color")
        .unwrap_or(gtk::gdk::RGBA::WHITE);
    let accent_rgba = style.lookup_color("accent_color").unwrap_or(fg_rgba);
    HtmlTheme {
        bg: bg_rgba,
        fg: rgba_hex(&fg_rgba),
        accent: rgba_hex(&accent_rgba),
        dark: adw::StyleManager::default().is_dark(),
    }
}

fn rgba_hex(color: &gtk::gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (color.red() * 255.0).round() as u8,
        (color.green() * 255.0).round() as u8,
        (color.blue() * 255.0).round() as u8
    )
}

/// The stylesheet injected into every body: theme colours plus the conversation
/// layout. Shared by the warm-up page and the rendered conversation.
const STYLE: &str = r#"<style>
:root { color-scheme: __SCHEME__; }
html, body { background-color: __BG__; color: __FG__; margin: 0; padding: 0; font-family: system-ui, "Adwaita Sans", sans-serif; overflow-wrap: anywhere; word-break: break-word; }
/* Neutralise viewport-bound CSS in message bodies so the view keeps scrolling,
   even when the mail sets `body { height: 100% }`. */
html body { height: auto !important; min-height: 0 !important; overflow: visible !important; }
a { color: __ACCENT__; }
img { max-width: 100%; height: auto; }
pre, code { font-family: monospace; }
pre { background: rgba(127,127,127,0.12); padding: 8px; border-radius: 4px; overflow-x: auto; }
blockquote { border-left: 3px solid __ACCENT__; margin: 0; padding-left: 10px; opacity: 0.85; }
details.quote { margin: 6px 0; }
details.quote > summary { cursor: pointer; color: __ACCENT__; }
details.quote blockquote { margin: 6px 0 0 0; }
table { border-collapse: collapse; max-width: 100%; }
/* Only outline cells in tables that declare a border: most HTML mail (Outlook,
   newsletters) uses nested layout tables, and an unconditional rule would box
   up every element of the message. */
table[border]:not([border="0"]) th,
table[border]:not([border="0"]) td { border: 1px solid rgba(127,127,127,0.4); padding: 4px 8px; }
.thread { padding: 8px 12px 12px; }
.msg { padding: 12px 0; border-bottom: 1px solid rgba(127,127,127,0.25); }
.msg:last-child { border-bottom: 0; padding-bottom: 4px; }
.head { display: flex; align-items: baseline; gap: 8px; }
.who { font-weight: 700; }
.when { margin-left: auto; font-size: 0.85em; opacity: 0.7; white-space: nowrap; }
.reply { font-size: 0.85em; opacity: 0.7; margin-top: 2px; }
.body { margin-top: 6px; }
.atts { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 10px; }
.att, .att-save { display: inline-flex; align-items: center; gap: 6px; padding: 4px 8px; border-radius: 6px; background: rgba(127,127,127,0.12); color: inherit; text-decoration: none; }
.att-save { padding: 4px; }
.att:hover, .att-save:hover { background: rgba(127,127,127,0.22); }
.att svg, .att-save svg { width: 16px; height: 16px; flex: none; }
.att-name { font-size: 0.95em; }
.att-size { font-size: 0.8em; opacity: 0.7; }
__READABLE__</style>"#;

/// Overrides that make message bodies follow the app theme instead of the
/// sender's own colours, so text is always legible. Outlook/Word mail (and much
/// marketing mail) hardcodes colours like `color: rgb(0,0,0)` inline, which
/// disappears against a dark background and would otherwise never change with
/// the system theme. An author `!important` beats the sender's non-important
/// inline `style`, and the `#jmail-thread` id keeps it above any `<style>` block
/// a message ships. Toggled by the "Force readable message colors" preference.
const READABLE_OVERRIDES: &str = r#"
html, body {
  background-color: __BG__ !important;
  background-image: none !important;
  color: __FG__ !important;
}
#jmail-thread .body,
#jmail-thread .body * {
  background-color: transparent !important;
  background-image: none !important;
  color: inherit !important;
}
#jmail-thread .body a { color: __ACCENT__ !important; }
#jmail-thread .body pre,
#jmail-thread .body code { background-color: rgba(127,127,127,0.14) !important; }
#jmail-thread .body details.quote > summary { color: __ACCENT__ !important; }
"#;

/// Fill the theme placeholders in [`STYLE`], optionally appending the
/// sender-colour overrides.
fn theme_style(theme: &HtmlTheme, readable: bool) -> String {
    let scheme = if theme.dark { "dark" } else { "light" };
    let overrides = if readable { READABLE_OVERRIDES } else { "" };
    // Replace `__READABLE__` first: the override block contains its own
    // `__BG__`/`__FG__`/`__ACCENT__` placeholders that must be filled too.
    STYLE
        .replace("__READABLE__", overrides)
        .replace("__SCHEME__", scheme)
        .replace("__BG__", &rgba_hex(&theme.bg))
        .replace("__FG__", &theme.fg)
        .replace("__ACCENT__", &theme.accent)
}

/// A blank themed document, used to warm up the WebView.
fn themed_html(html: &str, theme: &HtmlTheme) -> String {
    format!(
        "<!DOCTYPE html>{}{}",
        theme_style(theme, false),
        sanitize_html(html, false)
    )
}

/// Build the whole conversation as a single document: every message with its
/// header, body and attachments, oldest first. Only the per-message body is
/// sanitised; the surrounding conversation markup is ours.
fn conversation_html(theme: &HtmlTheme, messages: &[RenderedMessage], readable: bool) -> String {
    let mut body = String::from("<div id=\"jmail-thread\" class=\"thread\">");
    for message in messages {
        body.push_str("<section class=\"msg\">");
        body.push_str("<div class=\"head\"><span class=\"who\">");
        body.push_str(&escape_html(&message.sender));
        body.push_str("</span><span class=\"when\">");
        body.push_str(&escape_html(&message.date));
        body.push_str("</span></div>");
        if let Some(reply_to) = &message.reply_to {
            body.push_str("<div class=\"reply\">↳ in reply to ");
            body.push_str(&escape_html(reply_to));
            body.push_str("</div>");
        }
        body.push_str("<div class=\"body\">");
        let rendered = match &message.html {
            Some(html) => collapse_quotes(html),
            None => text_body_html(&message.text),
        };
        body.push_str(&sanitize_html(&rendered, readable));
        body.push_str("</div>");
        body.push_str(&attachments_html(message.id, &message.attachments));
        body.push_str("</section>");
    }
    body.push_str("</div>");
    format!("<!DOCTYPE html>{}{body}", theme_style(theme, readable))
}

/// The attachment chips under one message, as links back into the app. Clicking
/// the chip opens the file; the trailing button opens a save dialog.
fn attachments_html(message_id: i64, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    // Feather-style paperclip and download glyphs, drawn with `currentColor`.
    let clip = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg>"#;
    let save = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M7 10l5 5 5-5"/><path d="M12 15V3"/></svg>"#;

    let mut out = String::from("<div class=\"atts\">");
    for attachment in attachments {
        let part = attachment.part.parse::<u32>().unwrap_or(0);
        let raw = if attachment.filename.is_empty() {
            if attachment.mime.is_empty() {
                "(unnamed attachment)".to_string()
            } else {
                attachment.mime.clone()
            }
        } else {
            attachment.filename.clone()
        };
        let name = escape_html(&raw);
        let name_attr = escape_attr(&raw);
        let size = escape_html(&format_size(attachment.size));
        out.push_str(&format!(
            "<a class=\"att\" href=\"jmail-attachment:open/{message_id}/{part}\" title=\"Open {name_attr}\">{clip}<span class=\"att-name\">{name}</span><span class=\"att-size\">{size}</span></a>"
        ));
        out.push_str(&format!(
            "<a class=\"att-save\" href=\"jmail-attachment:save/{message_id}/{part}\" title=\"Save {name_attr}\">{save}</a>"
        ));
    }
    out.push_str("</div>");
    out
}

/// Escape text for use inside a quoted HTML attribute.
fn escape_attr(input: &str) -> String {
    escape_html(input)
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Remove active/remote content (`script`, `iframe`, `object`, `embed`) from an
/// HTML body before handing it to WebKit. JavaScript is also disabled.
///
/// When `strip_document_tags` is set, the sender's `<html>/<head>/<body>` tags
/// are dropped (content kept): the HTML parser would otherwise merge their
/// attributes — e.g. `bgcolor`/`background-color`/`height:100%` on `<body>` —
/// onto our own document, overriding the reading theme. This is only done while
/// the "Force readable message colors" preference is on; with it off the sender
/// keeps control of those colours.
fn sanitize_html(input: &str, strip_document_tags: bool) -> String {
    const BLOCKED: [&str; 4] = ["script", "iframe", "object", "embed"];
    const DOCUMENT: [&str; 3] = ["html", "head", "body"];
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let Some(rel) = lower[cursor..].find('<') else {
            out.push_str(&input[cursor..]);
            break;
        };
        let start = cursor + rel;
        out.push_str(&input[cursor..start]);
        let name = tag_name(&lower[start..]);
        match name.as_deref() {
            Some(blocked) if BLOCKED.contains(&blocked) => {
                let close = format!("</{blocked}");
                let end = lower[start..]
                    .find(&close)
                    .and_then(|rel| input[start + rel..].find('>').map(|gt| start + rel + gt + 1))
                    .or_else(|| input[start..].find('>').map(|gt| start + gt + 1))
                    .unwrap_or(input.len());
                cursor = end;
                continue;
            }
            Some(tag) if strip_document_tags && DOCUMENT.contains(&tag) => {
                let end = input[start..]
                    .find('>')
                    .map(|gt| start + gt + 1)
                    .unwrap_or(input.len());
                cursor = end;
                continue;
            }
            _ => {}
        }
        let end = input[start..]
            .find('>')
            .map(|gt| start + gt + 1)
            .unwrap_or(input.len());
        out.push_str(&input[start..end]);
        cursor = end;
    }
    out
}

fn tag_name(from_lt: &str) -> Option<String> {
    let rest = from_lt.strip_prefix('<')?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_attachment_cache_filenames() {
        assert_eq!(safe_filename("report.pdf"), "report.pdf");
        assert_eq!(safe_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_filename("  "), "attachment");
    }

    #[test]
    fn collapses_a_blockquote_behind_details() {
        let html = "<p>hi</p><blockquote type=\"cite\">old text</blockquote><p>bye</p>";
        let out = collapse_quotes(html);
        assert!(out.contains("<details class=\"quote\"><summary>Show quoted text</summary>"));
        assert!(out.contains("<blockquote type=\"cite\">old text</blockquote></details>"));
        assert!(out.contains("<p>hi</p>"));
        assert!(out.contains("<p>bye</p>"));
    }

    #[test]
    fn only_the_outer_blockquote_is_collapsed() {
        let html = "<blockquote>outer<blockquote>inner</blockquote></blockquote>";
        let out = collapse_quotes(html);
        assert_eq!(out.matches("<details").count(), 1);
        assert_eq!(out.matches("</details>").count(), 1);
    }

    #[test]
    fn leaves_html_without_quotes_alone() {
        let html = "<p>just text</p>";
        assert_eq!(collapse_quotes(html), html);
    }

    #[test]
    fn folds_quoted_text_lines() {
        let text = "Hello\n> previous message\n> second line\nRegards\n";
        let out = text_body_html(text);
        assert!(out.contains("Hello"));
        assert!(out.contains("Regards"));
        assert_eq!(out.matches("<details").count(), 1);
        assert!(out.contains("previous message"));
    }

    #[test]
    fn plain_text_without_quotes_is_a_single_pre() {
        let out = text_body_html("just a line\n");
        assert_eq!(out.matches("<pre").count(), 1);
        assert!(!out.contains("<details"));
    }

    #[test]
    fn formats_attachment_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    fn test_theme() -> HtmlTheme {
        HtmlTheme {
            bg: gtk::gdk::RGBA::WHITE,
            fg: "#000000".to_string(),
            accent: "#3584e4".to_string(),
            dark: false,
        }
    }

    fn rendered(id: i64, sender: &str, text: &str) -> RenderedMessage {
        RenderedMessage {
            id,
            has_body: true,
            sender: sender.to_string(),
            date: "Mon, 1 Jan".to_string(),
            text: text.to_string(),
            html: None,
            reply_to: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn conversation_lists_messages_oldest_first() {
        let first = rendered(1, "Alice", "first body");
        let second = rendered(2, "Bob", "second body");
        let out = conversation_html(&test_theme(), &[first, second], true);
        let alice = out.find("Alice").expect("alice present");
        let bob = out.find("Bob").expect("bob present");
        assert!(alice < bob, "Alice should precede Bob");
        assert!(out.contains("first body"));
        assert!(out.contains("second body"));
        assert!(out.starts_with("<!DOCTYPE html>"));
    }

    #[test]
    fn conversation_escapes_sender_names() {
        let out = conversation_html(&test_theme(), &[rendered(1, "<b>x</b>", "hi")], true);
        assert!(out.contains("&lt;b&gt;x&lt;/b&gt;"));
        assert!(!out.contains("<b>x</b>"));
    }

    #[test]
    fn conversation_strips_scripts_from_bodies() {
        let mut message = rendered(1, "Alice", "");
        message.html = Some("<p>hi</p><script>evil()</script>".to_string());
        let out = conversation_html(&test_theme(), &[message], true);
        assert!(out.contains("<p>hi</p>"));
        assert!(!out.contains("<script"));
        assert!(!out.contains("evil()"));
    }

    #[test]
    fn conversation_collapses_quoted_bodies() {
        let mut message = rendered(1, "Alice", "");
        message.html = Some("<p>new</p><blockquote>old</blockquote>".to_string());
        let out = conversation_html(&test_theme(), &[message], true);
        assert!(out.contains("<details class=\"quote\">"));
    }

    #[test]
    fn conversation_shows_snippet_when_body_is_missing() {
        let mut message = rendered(1, "Alice", "still fetching");
        message.has_body = false;
        message.html = None;
        let out = conversation_html(&test_theme(), &[message], true);
        assert!(out.contains("still fetching"));
    }

    #[test]
    fn conversation_renders_attachment_links() {
        let mut message = rendered(42, "Alice", "see attached");
        message.attachments = vec![Attachment {
            filename: "report.pdf".to_string(),
            part: "2".to_string(),
            size: 2048,
            ..Attachment::default()
        }];
        let out = conversation_html(&test_theme(), &[message], true);
        assert!(out.contains("jmail-attachment:open/42/2"));
        assert!(out.contains("jmail-attachment:save/42/2"));
        assert!(out.contains("report.pdf"));
        assert!(out.contains("2.0 KB"));
    }

    #[test]
    fn attachment_links_parse_back() {
        assert_eq!(
            parse_attachment_uri("jmail-attachment:open/42/2"),
            Some((42, 2, false))
        );
        assert_eq!(
            parse_attachment_uri("jmail-attachment:save/42/7"),
            Some((42, 7, true))
        );
        assert_eq!(parse_attachment_uri("jmail-attachment:foo/42/7"), None);
        assert_eq!(parse_attachment_uri("https://example.com"), None);
    }

    #[test]
    fn escapes_attribute_quotes() {
        assert_eq!(escape_attr("a\"b'c&<>"), "a&quot;b&#39;c&amp;&lt;&gt;");
    }

    #[test]
    fn readable_overrides_follow_the_preference() {
        let theme = test_theme();
        let with = theme_style(&theme, true);
        let without = theme_style(&theme, false);
        // The page itself is pinned, so a sender's `<body>` colours cannot win.
        assert!(with.contains("html, body"));
        assert!(with.contains(&format!("{} !important", rgba_hex(&theme.bg))));
        assert!(with.contains(&format!("{} !important", theme.fg)));
        assert!(with.contains("#jmail-thread .body"));
        assert!(with.contains("color: inherit !important"));
        // The accent placeholder inside the overrides is filled in too.
        assert!(with.contains(&format!("{} !important", theme.accent)));
        assert!(!without.contains("#jmail-thread .body"));
    }

    #[test]
    fn drops_document_level_tags_from_bodies() {
        let mut message = rendered(1, "Alice", "");
        message.html = Some(
            "<html><head><style>body{background:#fff}</style></head>\
             <body bgcolor=\"#ffffff\" style=\"height:100% !important\">hello</body></html>"
                .to_string(),
        );
        let out = conversation_html(&test_theme(), &[message], true);
        assert!(!out.contains("bgcolor"));
        assert!(!out.contains("height:100%"));
        assert!(!out.contains("<body"));
        assert!(out.contains("hello"));
        // The sender's stylesheet is kept (but outranked by our override).
        assert!(out.contains("body{background:#fff}"));
    }

    #[test]
    fn keeps_document_tags_when_not_forcing_readable() {
        let mut message = rendered(1, "Alice", "");
        message.html = Some("<body bgcolor=\"#ffffff\">hello</body>".to_string());
        let out = conversation_html(&test_theme(), &[message], false);
        assert!(out.contains("bgcolor=\"#ffffff\""));
        assert!(out.contains("hello"));
    }

    #[test]
    fn conversation_root_is_identified_for_overrides() {
        let out = conversation_html(&test_theme(), &[rendered(1, "Alice", "hi")], true);
        assert!(out.contains("<div id=\"jmail-thread\" class=\"thread\">"));
    }
}
