use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::glib::BoxedAnyObject;
use gtk::subclass::prelude::*;

use jmail_core::{
    AccountId, Attachment, Db, Flags, FolderId, MessageId, MoveTarget, SpecialUse, SyncCommand,
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
#[derive(Clone)]
pub(crate) struct ReadingPane {
    pub root: gtk::Widget,
    pub empty: adw::StatusPage,
    pub subject: gtk::Label,
    pub conversation: gtk::Box,
    pub messages_box: gtk::Box,
    pub reply_bar: gtk::Box,
    pub compose_slot: gtk::Box,
    pub scroll: gtk::ScrolledWindow,
    pub status_label: gtk::Label,
    pub cells: Rc<RefCell<Vec<MessageCell>>>,
    /// The conversation currently shown in this pane, for re-rendering.
    pub row: Rc<RefCell<Option<MessageRow>>>,
    /// Bumped on every render so a stale mark-read timer can be ignored.
    pub generation: Rc<Cell<u64>>,
    /// The correlation id of the inline composer open in this pane.
    pub inline_composer: Rc<Cell<Option<u64>>>,
    /// Composes a reply/forward for this pane (installed once a `Ui` exists).
    on_reply: Rc<RefCell<Option<Rc<dyn Fn(crate::compose::Mode)>>>>,
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
        conversation.set_valign(gtk::Align::Start);

        // Message cells go in `messages_box`; an inline reply composer is
        // appended to `compose_slot` beneath them.
        let messages_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
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

        conversation.append(&messages_box);
        conversation.append(&reply_bar);
        conversation.append(&compose_slot);

        let scrolled = gtk::ScrolledWindow::builder()
            .child(&conversation)
            .vexpand(true)
            .build();

        let conversation_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        conversation_page.append(&subject);
        conversation_page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        conversation_page.append(&scrolled);

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
            messages_box,
            reply_bar,
            compose_slot,
            scroll: scrolled,
            status_label,
            cells: Rc::new(RefCell::new(Vec::new())),
            row: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            inline_composer: Rc::new(Cell::new(None)),
            on_reply,
        })
    }

    /// Install the callback used by this pane's Reply / Reply all / Forward
    /// buttons. Called once a `Ui` exists, since composing needs the engine.
    pub(crate) fn set_on_reply(&self, callback: Rc<dyn Fn(crate::compose::Mode)>) {
        *self.on_reply.borrow_mut() = Some(callback);
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

    /// Scroll to the bottom (to reveal an inline composer).
    pub(crate) fn scroll_to_bottom(&self) {
        let scroll = self.scroll.clone();
        gtk::glib::idle_add_local_once(move || {
            let adjustment = scroll.vadjustment();
            adjustment.set_value(adjustment.upper() - adjustment.page_size());
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
    pane.clear_inline(ui);
    pane.empty.set_visible(true);
    pane.reply_bar.set_visible(false);
    for cell in pane.cells.borrow().iter() {
        cell.root.set_visible(false);
    }
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
    let theme = html_theme(&pane.conversation);
    pane.subject.set_label(&row.subject);
    pane.empty.set_visible(false);
    // Keep the reply bar hidden while an inline composer is open; it comes back
    // when the composer is discarded or sent.
    pane.reply_bar.set_visible(pane.inline_composer.get().is_none());

    let data = thread_render_data(&ui.db, row);

    // Reuse the same long-lived cell (and WebView) for each message slot. New
    // cells are only created the first time a thread is deeper than any we have
    // rendered before; they are never destroyed, so WebKit's compositing
    // surface is never torn down and re-created.
    let mut cells = pane.cells.borrow_mut();
    while cells.len() < data.len() {
        let cell = MessageCell::new(&theme);
        pane.messages_box.append(&cell.root);
        cells.push(cell);
    }
    // Defensive: make sure every reused cell really lives in `messages_box`
    // (before the compose slot), so message order can't be scrambled.
    let target: gtk::Widget = pane.messages_box.clone().upcast();
    for cell in cells.iter() {
        if cell.root.parent().as_ref() != Some(&target) {
            pane.messages_box.append(&cell.root);
        }
    }
    for (cell, message) in cells.iter().zip(data.iter()) {
        cell.root.set_visible(true);
        cell.load(ui, message, &theme);
    }
    for cell in cells.iter().skip(data.len()) {
        cell.root.set_visible(false);
    }
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

mod auto_height {
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct AutoHeight {
        pub child: RefCell<Option<gtk::Widget>>,
        pub height: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AutoHeight {
        const NAME: &'static str = "JmailAutoHeight";
        type Type = super::AutoHeight;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for AutoHeight {
        fn dispose(&self) {
            if let Some(child) = self.child.borrow_mut().take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for AutoHeight {
        fn measure(
            &self,
            orientation: gtk::Orientation,
            for_size: i32,
        ) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Vertical {
                let height = self.height.get();
                (height, height, -1, -1)
            } else if let Some(child) = self.child.borrow().as_ref() {
                child.measure(orientation, for_size)
            } else {
                (0, 0, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.child.borrow().as_ref() {
                child.allocate(width, height, baseline, None);
            }
        }
    }
}

glib::wrapper! {
    /// A container that gives its single child an exact height.
    ///
    /// `gtk_widget_set_size_request` only sets a *minimum*, and a reused
    /// `WebKitWebView`'s natural height sticks at the largest value it has seen
    /// (floored at the viewport), so it cannot shrink. This ignores the child's
    /// natural height and reports the height set via [`AutoHeight::set_height`].
    pub struct AutoHeight(ObjectSubclass<auto_height::AutoHeight>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl AutoHeight {
    pub(crate) fn new(view: &webkit6::WebView) -> Self {
        let holder: Self = glib::Object::new();
        view.set_parent(&holder);
        holder.imp().child.replace(Some(view.clone().upcast()));
        holder
    }

    /// Set the exact height this container (and its child) will take.
    pub(crate) fn set_height(&self, height: i32) {
        if self.imp().height.get() != height {
            self.imp().height.set(height);
            self.queue_resize();
        }
    }
}

/// A message cell whose body WebView is created once and then reused.
pub(crate) struct MessageCell {
    root: gtk::Box,
    who: gtk::Label,
    when: gtk::Label,
    /// "in reply to …" attribution line, hidden for thread starters.
    reply_label: gtk::Label,
    view: webkit6::WebView,
    /// Rebuilt for each message; holds the attachment chips.
    attachments_box: gtk::Box,
    /// `(message id, body available, dark theme, remote images)` of the content
    /// currently loaded, so a redundant `reload_preview` (e.g. after a flag
    /// change) does not repaint the body.
    last: std::cell::RefCell<Option<(i64, bool, bool, bool)>>,
}

impl MessageCell {
    fn new(theme: &HtmlTheme) -> Self {
        use webkit6::prelude::*;

        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_top(10);
        root.set_margin_bottom(10);
        root.set_margin_start(14);
        root.set_margin_end(14);
        root.add_css_class("conversation-message");

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let who = gtk::Label::new(None);
        who.set_xalign(0.0);
        who.add_css_class("heading");
        let when = gtk::Label::new(None);
        when.add_css_class("dim-label");
        when.add_css_class("caption");
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        head.append(&who);
        head.append(&spacer);
        head.append(&when);
        root.append(&head);

        let reply_label = gtk::Label::new(None);
        reply_label.set_xalign(0.0);
        reply_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        reply_label.add_css_class("dim-label");
        reply_label.add_css_class("caption");
        reply_label.set_visible(false);
        root.append(&reply_label);

        let view = webkit6::WebView::new();
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_enable_javascript(true);
            settings.set_enable_javascript_markup(false);
            settings.set_enable_developer_extras(false);
            settings.set_auto_load_images(false);
        }
        install_webview_policy(&view);
        view.set_background_color(&theme.bg);
        view.set_hexpand(true);
        view.set_vexpand(false);
        // Keep the (reused) surface hidden until it has painted its content.
        view.set_opacity(0.0);

        // Force the cell to the measured body height: a reused WebView's
        // natural height otherwise sticks at the largest value it has seen.
        let holder = AutoHeight::new(&view);
        holder.set_hexpand(true);
        holder.set_height(140);
        install_height_messaging(&view, &holder);

        {
            let view = view.clone();
            let holder = holder.clone();
            view.connect_load_changed(move |view, event| {
                if event == webkit6::LoadEvent::Finished {
                    // Reveal only once the content height has been measured.
                    measure_view_height(view, &holder, true);
                }
            });
        }
        {
            let view = view.clone();
            view.connect_load_failed(move |view, _, _, _| {
                view.set_opacity(1.0);
                false
            });
        }
        // Re-measure whenever the allocated width changes (the pane was resized).
        let last_width = Rc::new(std::cell::Cell::new(0));
        {
            let holder = holder.clone();
            view.add_tick_callback(move |view, _clock| {
                let width = view.width();
                if width != last_width.get() {
                    last_width.set(width);
                    measure_view_height(view, &holder, false);
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        root.append(&holder);

        let attachments_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        attachments_box.set_visible(false);
        root.append(&attachments_box);

        Self {
            root,
            who,
            when,
            reply_label,
            view,
            attachments_box,
            last: std::cell::RefCell::new(None),
        }
    }

    /// Point this cell at a different message; the WebView is reused as-is.
    fn load(&self, ui: &Ui, message: &RenderedMessage, theme: &HtmlTheme) {
        let remote = ui.remote_images.get();
        let key = (message.id, message.has_body, theme.dark, remote);
        if self.last.borrow().as_ref() == Some(&key) {
            return;
        }
        *self.last.borrow_mut() = Some(key);
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&self.view) {
            settings.set_auto_load_images(remote);
        }

        self.who.set_text(&message.sender);
        self.when.set_text(&message.date);
        // Reply attribution: who this message answers.
        match &message.reply_to {
            Some(name) => {
                self.reply_label.set_text(&format!("↳ in reply to {name}"));
                self.reply_label.set_visible(true);
            }
            None => self.reply_label.set_visible(false),
        }
        self.set_attachments(ui, message.id, &message.attachments);

        let html = match &message.html {
            Some(html) => themed_html(&collapse_quotes(html), theme),
            None => themed_html(&text_body_html(&message.text), theme),
        };
        self.load_html(&html, theme);
    }

    /// Replace the attachment chips shown under the body.
    fn set_attachments(&self, ui: &Ui, message_id: i64, attachments: &[Attachment]) {
        while let Some(child) = self.attachments_box.first_child() {
            self.attachments_box.remove(&child);
        }
        self.attachments_box.set_visible(!attachments.is_empty());
        for attachment in attachments {
            self.attachments_box
                .append(&attachment_row(ui, MessageId(message_id), attachment));
        }
    }

    /// Load a blank themed page so WebKit creates and paints its compositing
    /// surface before the first real message arrives. A real message is never
    /// clobbered; a warm-up page is marked by `id = -1` in the `last` guard.
    fn warm(&self, theme: &HtmlTheme) {
        let showing_message = self
            .last
            .borrow()
            .as_ref()
            .map(|(id, _, _, _)| *id >= 0)
            .unwrap_or(false);
        if showing_message {
            return;
        }
        // The empty state above is opaque, so this render is never visible, but
        // it still primes WebKit's compositing surface for the first message.
        *self.last.borrow_mut() = Some((-1, false, theme.dark, false));
        self.load_html(&themed_html("", theme), theme);
    }

    /// Render new HTML while hidden, then reveal it once it has painted.
    fn load_html(&self, html: &str, theme: &HtmlTheme) {
        use webkit6::prelude::*;

        self.view.set_background_color(&theme.bg);
        self.view.set_opacity(0.0);
        self.view.load_html(html, None);
        // Backstop so a load that never reports finished still appears.
        let view = self.view.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(1), move || view.set_opacity(1.0));
    }
}

/// Ensure the reading pane's reusable WebView exists and has painted once, so
/// the first conversation opened does not cold-start WebKit on screen.
pub(crate) fn preload_reading_pane(ui: &Ui) {
    let theme = html_theme(&ui.reading.conversation);
    let mut cells = ui.reading.cells.borrow_mut();
    if cells.is_empty() {
        let cell = MessageCell::new(&theme);
        ui.reading.messages_box.append(&cell.root);
        cells.push(cell);
    }
    cells[0].warm(&theme);
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Intercept link clicks and hand the URI to the desktop instead of navigating
/// the message view. Page-initiated navigation and `target="_blank"` popups are
/// blocked as well.
fn install_webview_policy(view: &webkit6::WebView) {
    use webkit6::prelude::*;

    view.connect_decide_policy(|_, decision, decision_type| {
        // Only user link clicks are intercepted. Everything else (in particular
        // the programmatic `load_html` that renders a body) is left to WebKit,
        // otherwise the message itself would be blocked.
        if decision_type != webkit6::PolicyDecisionType::NavigationAction {
            return false;
        }
        let is_link = decision
            .downcast_ref::<webkit6::NavigationPolicyDecision>()
            .and_then(|nav| nav.navigation_action())
            .is_some_and(|action| action.navigation_type() == webkit6::NavigationType::LinkClicked);
        if is_link {
            if let Some(uri) = decision
                .downcast_ref::<webkit6::NavigationPolicyDecision>()
                .and_then(|nav| nav.navigation_action())
                .and_then(|action| action.request())
                .and_then(|request| request.uri())
            {
                open_external(&uri);
            }
            decision.ignore();
            return true;
        }
        false
    });

    view.connect_create(|_, action| {
        if let Some(uri) = action.request().and_then(|request| request.uri()) {
            open_external(&uri);
        }
        None
    });
}

/// Keep the `AutoHeight` container in sync when the page re-lays-out (for
/// example when a "Show quoted text" `<details>` is expanded): a user script
/// posts the document height whenever a `details` element toggles.
fn install_height_messaging(view: &webkit6::WebView, holder: &AutoHeight) {
    use webkit6::prelude::*;

    const NAME: &str = "jmailHeight";
    const SCRIPT: &str = r#"
document.addEventListener('toggle', function (event) {
    var el = event.target;
    if (el && el.tagName === 'DETAILS') {
        var h = Math.ceil(Math.max(document.body.scrollHeight,
                                   document.body.getBoundingClientRect().height));
        window.webkit.messageHandlers.jmailHeight.postMessage(h);
    }
}, true);
"#;

    let Some(manager) = view.user_content_manager() else {
        return;
    };
    manager.register_script_message_handler(NAME, None);
    {
        let holder = holder.clone();
        manager.connect_script_message_received(Some(NAME), move |_, value| {
            let height = value.to_double();
            if height > 0.0 {
                holder.set_height(height.ceil() as i32 + 2);
            }
        });
    }
    let script = webkit6::UserScript::new(
        SCRIPT,
        webkit6::UserContentInjectedFrames::TopFrame,
        webkit6::UserScriptInjectionTime::End,
        &[],
        &[],
    );
    manager.add_script(&script);
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

/// A single attachment chip: icon, name and human-readable size. Clicking it
/// opens a save dialog.
fn attachment_row(ui: &Ui, message: MessageId, attachment: &Attachment) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("attachment");
    row.set_margin_top(2);
    row.set_margin_bottom(2);

    let icon = gtk::Image::from_icon_name("mail-attachment-symbolic");
    icon.set_valign(gtk::Align::Center);
    row.append(&icon);

    let name = if attachment.filename.is_empty() {
        if attachment.mime.is_empty() {
            "(unnamed attachment)".to_string()
        } else {
            attachment.mime.clone()
        }
    } else {
        attachment.filename.clone()
    };
    let label = gtk::Label::new(Some(&name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    row.append(&label);

    let size = gtk::Label::new(Some(&format_size(attachment.size)));
    size.add_css_class("dim-label");
    size.add_css_class("caption");
    size.set_valign(gtk::Align::Center);
    row.append(&size);

    row.set_cursor_from_name(Some("pointer"));
    let gesture = gtk::GestureClick::new();
    let ui = ui.clone();
    let attachment = attachment.clone();
    gesture.connect_released(move |_, _, _, _| {
        save_attachment(&ui, message, &attachment);
    });
    row.add_controller(gesture);

    row.upcast()
}

/// Ask where to save an attachment, then have the account's worker fetch and
/// write it. Result arrives as `Event::AttachmentSaved`.
fn save_attachment(ui: &Ui, message: MessageId, attachment: &Attachment) {
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
    let part: u32 = attachment.part.parse().unwrap_or(0);
    let suggested = if attachment.filename.is_empty() {
        format!("attachment-{part}")
    } else {
        attachment.filename.clone()
    };

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
                },
            );
            ui.status_label.set_text("Saving attachment…");
        },
    );
}

/// Open a saved file with the desktop's default application.
pub(crate) fn open_file(path: &std::path::Path) {
    let uri = gtk::gio::File::for_path(path).uri();
    if let Err(error) =
        gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>)
    {
        tracing::warn!(%error, path = %path.display(), "could not open attachment");
    }
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
    let account = AccountId(row.account_id);
    match ui.db.set_thread_seen(account, &row.thread_id, true) {
        Ok(ids) if !ids.is_empty() => {
            if ui.engine.is_running(account) {
                ui.engine.send(
                    account,
                    SyncCommand::SetThreadSeen {
                        message: MessageId(row.id),
                        seen: true,
                    },
                );
            }
        }
        Ok(_) => return,
        Err(error) => {
            tracing::error!(%error, "failed to mark conversation read");
            return;
        }
    }
    ui.reload_messages();
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
    if let Err(error) = ui.db.set_thread_flagged(account, &row.thread_id, star) {
        tracing::error!(%error, "failed to update flags");
    }
    if ui.engine.is_running(account) {
        ui.engine.send(
            account,
            SyncCommand::SetThreadFlagged {
                message: MessageId(row.id),
                flagged: star,
            },
        );
    }
    ui.reload_messages();
    ui.status_label
        .set_text(if star { "Starred" } else { "Unstarred" });
}

fn set_unread(ui: &Ui, unread: bool) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    let account = AccountId(row.account_id);
    if let Err(error) = ui.db.set_thread_seen(account, &row.thread_id, !unread) {
        tracing::error!(%error, "failed to update flags");
    }
    if ui.engine.is_running(account) {
        ui.engine.send(
            account,
            SyncCommand::SetThreadSeen {
                message: MessageId(row.id),
                seen: !unread,
            },
        );
    }
    ui.reload_messages();
    ui.status_label
        .set_text(if unread { "Marked unread" } else { "Marked read" });
}

/// Move the selected conversation to a target folder. When the account is
/// syncing, the engine applies it on the server and the list refreshes; when
/// offline, the cache is updated directly. Only messages in the folder the row
/// represents move, so moving a thread does not drag along its Sent copies.
fn move_selected(ui: &Ui, target: MoveTarget, verb: &'static str) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    let account = AccountId(row.account_id);
    let message = MessageId(row.id);

    if ui.engine.is_running(account) {
        ui.engine.send(account, SyncCommand::MoveThread { message, to: target });
        ui.status_label.set_text(&format!("{verb} conversation"));
        return;
    }

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
    let locations = ui.db.thread_locations(account, &row.thread_id).unwrap_or_default();
    let mut result = Ok(());
    for (id, folder, _path, _uid, flags) in locations {
        if folder != folder_id {
            continue;
        }
        result = match target_id {
            Some(to) => ui.db.move_message(id, to),
            // No special-use folder (e.g. archiving without an Archive
            // mailbox): mark the conversation seen instead.
            None => ui.db.set_flags(id, Flags { seen: true, ..flags }),
        };
        if result.is_err() {
            break;
        }
    }
    if let Err(error) = result {
        tracing::error!(%error, "failed to move conversation");
    }
    ui.reload_messages();
    ui.status_label.set_text(&format!("{verb} conversation"));
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
    let label = gtk::Label::builder().label("Ready").xalign(0.0).build();
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

/// Measure the body content height and apply it to the `AutoHeight` container.
///
/// The measurement deliberately ignores the document root: `documentElement`
/// scroll height is floored at the view's current viewport, which would stop the
/// view from ever shrinking. `themed_html` supplies a doctype so the body is in
/// standards mode, where `body` reports the real content height (in quirks mode
/// the body stretches to the viewport, which is what made this look stuck). When
/// `reveal` is set, the view is made visible only after the height is applied.
fn measure_view_height(view: &webkit6::WebView, holder: &AutoHeight, reveal: bool) {
    use webkit6::prelude::*;

    let holder = holder.clone();
    let view_for_reveal = view.clone();
    view.evaluate_javascript(
        "Math.ceil(Math.max(document.body.scrollHeight, document.body.getBoundingClientRect().height))",
        None,
        None,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            match result {
                Ok(value) => {
                    let height = value.to_double();
                    if height > 0.0 {
                        holder.set_height(height.ceil() as i32 + 2);
                    }
                }
                Err(error) => tracing::debug!(%error, "could not measure HTML body height"),
            }
            if reveal {
                view_for_reveal.set_opacity(1.0);
            }
        },
    );
}

/// Wrap sanitized HTML with a stylesheet that matches the active theme.
fn themed_html(html: &str, theme: &HtmlTheme) -> String {
    const STYLE: &str = r#"<style>
:root { color-scheme: __SCHEME__; }
html, body { background-color: __BG__; color: __FG__; margin: 0; padding: 8px 10px; overflow: hidden; font-family: system-ui, "Adwaita Sans", sans-serif; }
a { color: __ACCENT__; }
img { max-width: 100%; height: auto; }
pre, code { font-family: monospace; }
pre { background: rgba(127,127,127,0.12); padding: 8px; border-radius: 4px; overflow-x: auto; }
blockquote { border-left: 3px solid __ACCENT__; margin: 0; padding-left: 10px; opacity: 0.85; }
details.quote { margin: 6px 0; }
details.quote > summary { cursor: pointer; color: __ACCENT__; }
details.quote blockquote { margin: 6px 0 0 0; }
table { border-collapse: collapse; max-width: 100%; }
th, td { border: 1px solid rgba(127,127,127,0.4); padding: 4px 8px; }
</style>"#;
    let scheme = if theme.dark { "dark" } else { "light" };
    let style = STYLE
        .replace("__SCHEME__", scheme)
        .replace("__BG__", &rgba_hex(&theme.bg))
        .replace("__FG__", &theme.fg)
        .replace("__ACCENT__", &theme.accent);
    format!("<!DOCTYPE html>{style}{}", sanitize_html(html))
}

/// Remove active/remote content (`script`, `iframe`, `object`, `embed`) from an
/// HTML body before handing it to WebKit. JavaScript is also disabled.
fn sanitize_html(input: &str) -> String {
    const BLOCKED: [&str; 4] = ["script", "iframe", "object", "embed"];
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
        if let Some(name) = name.filter(|name| BLOCKED.contains(&name.as_str())) {
            let close = format!("</{name}");
            let end = lower[start..]
                .find(&close)
                .and_then(|rel| input[start + rel..].find('>').map(|gt| start + rel + gt + 1))
                .or_else(|| input[start..].find('>').map(|gt| start + gt + 1))
                .unwrap_or(input.len());
            cursor = end;
            continue;
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
}
