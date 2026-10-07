use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::BoxedAnyObject;

use jmail_core::{AccountId, Attachment, Db, MessageId, SpecialUse, SyncCommand};

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
        move || crate::compose::present(Some(&ui.window), crate::compose::Mode::New, None)
    });
    for (name, mode) in [
        ("reply", crate::compose::Mode::Reply),
        ("reply-all", crate::compose::Mode::ReplyAll),
        ("forward", crate::compose::Mode::Forward),
    ] {
        let handle = ui.clone();
        on(ui, name, move || {
            let subject = selected_subject(&handle);
            crate::compose::present(Some(&handle.window), mode, subject);
        });
    }

    // Message actions.
    on(ui, "archive", {
        let ui = ui.clone();
        move || move_selected(&ui, SpecialUse::Archive, "Archived")
    });
    on(ui, "delete", {
        let ui = ui.clone();
        move || move_selected(&ui, SpecialUse::Trash, "Deleted")
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
        move || ui.status_label.set_text("Move to folder (not implemented yet)")
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
    for (name, folder) in [
        ("go-inbox", "Joined Inbox"),
        ("go-sent", "Sent"),
        ("go-drafts", "Drafts"),
        ("go-all", "All Mail"),
    ] {
        let handle = ui.clone();
        on(ui, name, move || {
            handle.window_title.set_subtitle(folder);
            handle.status_label.set_text(&format!("Go to {folder}"));
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

fn selected_subject(ui: &Ui) -> Option<String> {
    selected_row(ui).map(|row| row.subject)
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

/// Preview the message at `position` in the reading pane (hover / j / k).
pub(crate) fn request_preview(ui: &Ui, position: u32) {
    if ui.suppress_preview.get() || ui.list_split.is_collapsed() {
        return;
    }
    let Some(row) = row_at(ui, position) else {
        return;
    };
    begin_preview(ui, &row, false);
}

/// Single click: preview and make sure the reading pane is shown.
fn open_pane(ui: &Ui) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    begin_preview(ui, &row, true);
}

/// `o` / Enter: open the conversation in its own window.
fn open_window(ui: &Ui) {
    let Some(row) = selected_row(ui) else {
        ui.status_label.set_text("No message selected");
        return;
    };
    open_message_window(ui, &row);
}

/// Render a preview and schedule marking it read after a short delay.
fn begin_preview(ui: &Ui, row: &MessageRow, show_content: bool) {
    let generation = ui.preview_generation.get().wrapping_add(1);
    ui.preview_generation.set(generation);
    *ui.preview_row.borrow_mut() = Some(row.clone());

    render_preview(ui, row);
    ensure_thread_bodies(ui, row);
    if show_content {
        ui.list_split.set_show_content(true);
    }
    ui.status_label.set_text(&format!("Reading: {}", row.subject));

    if row.unread_count > 0 {
        let handle = ui.clone();
        let row = row.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if handle.preview_generation.get() == generation {
                mark_thread_read(&handle, &row);
            }
        });
    }
}

fn render_preview(ui: &Ui, row: &MessageRow) {
    let theme = html_theme(&ui.conversation);
    ui.reading_subject.set_label(&row.subject);
    ui.reading_empty.set_visible(false);

    let data = thread_render_data(&ui.db, row);

    // Reuse the same long-lived cell (and WebView) for each message slot. New
    // cells are only created the first time a thread is deeper than any we have
    // rendered before; they are never destroyed, so WebKit's compositing
    // surface is never torn down and re-created.
    let mut cells = ui.cells.borrow_mut();
    while cells.len() < data.len() {
        let cell = MessageCell::new(&theme);
        ui.conversation.append(&cell.root);
        cells.push(cell);
    }
    for (cell, message) in cells.iter().zip(data.iter()) {
        cell.root.set_visible(true);
        cell.load(message, &theme);
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

    let senders = sender_by_message_id(&messages);

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
/// who it answers.
fn sender_by_message_id(messages: &[jmail_core::Message]) -> HashMap<String, String> {
    let mut senders = HashMap::new();
    for message in messages {
        let id = jmail_core::threading::normalize_message_id(&message.message_id);
        if !id.is_empty() {
            senders
                .entry(id)
                .or_insert_with(|| message.from.display().to_string());
        }
    }
    senders
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
    /// `(message id, body available, dark theme)` of the content currently
    /// loaded, so a redundant `reload_preview` (e.g. after a flag change) does
    /// not repaint the body.
    last: std::cell::RefCell<Option<(i64, bool, bool)>>,
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
        view.set_size_request(-1, 140);
        // Keep the (reused) surface hidden until it has painted its content.
        view.set_opacity(0.0);

        {
            let view = view.clone();
            view.connect_load_changed(move |view, event| {
                if event == webkit6::LoadEvent::Finished {
                    measure_view_height(view);
                    view.set_opacity(1.0);
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
        view.add_tick_callback(move |view, _clock| {
            let width = view.width();
            if width != last_width.get() {
                last_width.set(width);
                measure_view_height(view);
            }
            gtk::glib::ControlFlow::Continue
        });

        root.append(&view);

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
    fn load(&self, message: &RenderedMessage, theme: &HtmlTheme) {
        let key = (message.id, message.has_body, theme.dark);
        if self.last.borrow().as_ref() == Some(&key) {
            return;
        }
        *self.last.borrow_mut() = Some(key);

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
        self.set_attachments(&message.attachments);

        let html = match &message.html {
            Some(html) => themed_html(&collapse_quotes(html), theme),
            None => themed_html(&text_body_html(&message.text), theme),
        };
        self.load_html(&html, theme);
    }

    /// Replace the attachment chips shown under the body.
    fn set_attachments(&self, attachments: &[Attachment]) {
        while let Some(child) = self.attachments_box.first_child() {
            self.attachments_box.remove(&child);
        }
        self.attachments_box.set_visible(!attachments.is_empty());
        for attachment in attachments {
            self.attachments_box.append(&attachment_row(attachment));
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
            .map(|(id, _, _)| *id >= 0)
            .unwrap_or(false);
        if showing_message {
            return;
        }
        // The empty state above is opaque, so this render is never visible, but
        // it still primes WebKit's compositing surface for the first message.
        *self.last.borrow_mut() = Some((-1, false, theme.dark));
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
    let theme = html_theme(&ui.conversation);
    let mut cells = ui.cells.borrow_mut();
    if cells.is_empty() {
        let cell = MessageCell::new(&theme);
        ui.conversation.append(&cell.root);
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

/// A single attachment chip: icon, name and human-readable size.
fn attachment_row(attachment: &Attachment) -> gtk::Widget {
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

    row.upcast()
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
    if let Some(row) = ui.preview_row.borrow().clone() {
        render_preview(ui, &row);
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
    for message in messages {
        let cached = ui
            .db
            .message_body(message.id)
            .ok()
            .flatten()
            .map(|body| body.text.is_some() || body.html.is_some())
            .unwrap_or(false);
        if !cached {
            ui.engine
                .send(account, SyncCommand::FetchBody { message: message.id });
        }
    }
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

/// Move the selected conversation to a special-use folder. When the account is
/// syncing, the engine applies it on the server and the list refreshes; when
/// offline, the cache is updated directly. Only messages in the folder the row
/// represents move, so archiving a thread does not drag along its Sent copies.
fn move_selected(ui: &Ui, special_use: SpecialUse, verb: &'static str) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    let account = AccountId(row.account_id);
    let message = MessageId(row.id);

    if ui.engine.is_running(account) {
        ui.engine.send(
            account,
            SyncCommand::MoveThread {
                message,
                to: special_use,
            },
        );
        ui.status_label.set_text(&format!("{verb} conversation"));
        return;
    }

    let Some((_account, _path, folder_id, _uid, _flags)) =
        ui.db.message_location(message).ok().flatten()
    else {
        return;
    };
    let target = ui
        .db
        .folders(account)
        .ok()
        .and_then(|folders| {
            folders
                .into_iter()
                .find(|folder| folder.special_use == special_use)
                .map(|folder| folder.id)
        });
    let locations = ui.db.thread_locations(account, &row.thread_id).unwrap_or_default();
    let mut result = Ok(());
    for (id, folder, _path, _uid, _flags) in locations {
        if folder != folder_id {
            continue;
        }
        result = match target {
            Some(to) => ui.db.move_message(id, to),
            None => ui.db.delete_message(id),
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

/// Open a conversation in a standalone window.
fn open_message_window(ui: &Ui, row: &MessageRow) {
    let Some(app) = ui.window.application() else {
        return;
    };

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&row.subject, &row.sender);
    header.set_title_widget(Some(&title));

    let conversation = gtk::Box::new(gtk::Orientation::Vertical, 0);
    conversation.set_valign(gtk::Align::Start);
    let theme = html_theme(&ui.window);
    for widget in conversation_message_widgets(&ui.db, &theme, None, row) {
        conversation.append(&widget);
    }
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&conversation)
        .vexpand(true)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));

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
    window.present();

    mark_thread_read(ui, row);
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

/// Tracks how many WebViews in a conversation still need to render before the
/// preloaded conversation can be swapped in.
#[derive(Clone)]
struct PreviewReady {
    remaining: Rc<std::cell::Cell<usize>>,
    notify: Rc<dyn Fn()>,
}

impl PreviewReady {
    fn start(&self) {
        self.remaining.set(self.remaining.get() + 1);
    }

    fn finish(&self) {
        let remaining = self.remaining.get().saturating_sub(1);
        self.remaining.set(remaining);
        if remaining == 0 {
            (self.notify)();
        }
    }
}

/// Build the message widgets for a conversation, oldest first.
fn conversation_message_widgets(
    db: &Db,
    theme: &HtmlTheme,
    ready: Option<&PreviewReady>,
    row: &MessageRow,
) -> Vec<gtk::Widget> {
    let messages = db
        .thread_messages(AccountId(row.account_id), &row.thread_id, 50)
        .unwrap_or_default();
    if messages.is_empty() {
        return vec![conversation_message(
            &row.sender,
            &row.date_label,
            &row.snippet,
            None,
            None,
            &[],
            theme,
            ready,
        )];
    }
    let senders = sender_by_message_id(&messages);
    messages
        .iter()
        .map(|message| {
            let body = db.message_body(message.id).ok().flatten();
            let (text, html) = match body {
                Some(body) => (
                    body.text.unwrap_or_else(|| message.snippet.clone()),
                    body.html,
                ),
                None => (message.snippet.clone(), None),
            };
            let reply_to = reply_to_name(&senders, &message.in_reply_to);
            let attachments = db.attachments(message.id).unwrap_or_default();
            conversation_message(
                message.from.display(),
                &format_timestamp(message.date.unix_timestamp()),
                &text,
                html.as_deref(),
                reply_to.as_deref(),
                &attachments,
                theme,
                ready,
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn conversation_message(
    sender: &str,
    date: &str,
    body: &str,
    html: Option<&str>,
    reply_to: Option<&str>,
    attachments: &[Attachment],
    theme: &HtmlTheme,
    ready: Option<&PreviewReady>,
) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(14);
    root.set_margin_end(14);
    root.add_css_class("conversation-message");

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let who = gtk::Label::new(Some(sender));
    who.set_xalign(0.0);
    who.add_css_class("heading");
    let when = gtk::Label::new(Some(date));
    when.add_css_class("dim-label");
    when.add_css_class("caption");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    head.append(&who);
    head.append(&spacer);
    head.append(&when);

    root.append(&head);
    if let Some(name) = reply_to {
        let reply = gtk::Label::new(Some(&format!("↳ in reply to {name}")));
        reply.set_xalign(0.0);
        reply.set_ellipsize(gtk::pango::EllipsizeMode::End);
        reply.add_css_class("dim-label");
        reply.add_css_class("caption");
        root.append(&reply);
    }
    match html {
        Some(html) => {
            if let Some(ready) = ready {
                ready.start();
            }
            root.append(&html_body(html, theme, ready.cloned()));
        }
        None => {
            let text = gtk::Label::new(Some(body));
            text.set_xalign(0.0);
            text.set_wrap(true);
            text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            root.append(&text);
        }
    }
    for attachment in attachments {
        root.append(&attachment_row(attachment));
    }
    root.upcast()
}

/// Render an HTML body in a locked-down, theme-matched `WebKitWebView` that
/// sizes itself to its content so the reading pane scrolls as a whole.
fn html_body(html: &str, theme: &HtmlTheme, ready: Option<PreviewReady>) -> gtk::Widget {
    use webkit6::prelude::*;

    let view = webkit6::WebView::new();
    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
        // JS must be enabled for `evaluate_javascript` (used to auto-size), but
        // disabling "markup" stops the page's own scripts from running.
        settings.set_enable_javascript(true);
        settings.set_enable_javascript_markup(false);
        settings.set_enable_developer_extras(false);
        settings.set_auto_load_images(false);
    }
    install_webview_policy(&view);
    view.set_background_color(&theme.bg);
    view.set_hexpand(true);
    view.set_vexpand(false);
    view.set_size_request(-1, 140);
    // Keep the view invisible until it has actually rendered, so the pane never
    // shows WebKit's uninitialised (black) compositing surface.
    view.set_opacity(0.0);

    let finish: Rc<dyn Fn()> = {
        let view = view.clone();
        let ready = ready.clone();
        let done = Rc::new(std::cell::Cell::new(false));
        Rc::new(move || {
            if done.replace(true) {
                return;
            }
            view.set_opacity(1.0);
            if let Some(ready) = &ready {
                ready.finish();
            }
        })
    };

    {
        let finish = finish.clone();
        view.connect_load_failed(move |_, _, _, _| {
            finish();
            false
        });
    }
    view.load_html(&themed_html(&collapse_quotes(html), theme), None);

    {
        let finish = finish.clone();
        view.connect_load_changed(move |view, event| {
            if event == webkit6::LoadEvent::Finished {
                measure_view_height(view);
                // `Finished` only means the load completed; wait for the next
                // frame tick so WebKit has actually painted before revealing.
                let finish = finish.clone();
                view.add_tick_callback(move |_, _| {
                    finish();
                    gtk::glib::ControlFlow::Break
                });
            }
        });
    }
    // Backstop so a view that never reports ready still appears.
    {
        let finish = finish.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(1), move || finish());
    }
    // Re-measure whenever the allocated width changes (the pane was resized).
    let last_width = std::rc::Rc::new(std::cell::Cell::new(0));
    view.add_tick_callback(move |view, _clock| {
        let width = view.width();
        if width != last_width.get() {
            last_width.set(width);
            measure_view_height(view);
        }
        gtk::glib::ControlFlow::Continue
    });
    view.upcast()
}

fn measure_view_height(view: &webkit6::WebView) {
    use webkit6::prelude::*;

    let target = view.clone();
    view.evaluate_javascript(
        "Math.ceil(Math.max(document.body.scrollHeight, document.documentElement.scrollHeight))",
        None,
        None,
        None::<&gtk::gio::Cancellable>,
        move |result| match result {
            Ok(value) => {
                let height = value.to_double();
                if height > 0.0 {
                    target.set_size_request(-1, height.ceil() as i32 + 2);
                }
            }
            Err(error) => tracing::debug!(%error, "could not measure HTML body height"),
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
    format!("{style}{}", sanitize_html(html))
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
