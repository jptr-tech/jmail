use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::BoxedAnyObject;

use jmail_core::{AccountId, Db, MessageId, SpecialUse, SyncCommand};

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

/// Replace the selected store item in place so the row widget rebinds.
fn replace_row(ui: &Ui, row: &MessageRow) {
    let Some(item) = ui.selection.selected_item() else {
        return;
    };
    let Some(position) = ui.store.find(&item) else {
        return;
    };
    let filter_position = ui.selection.selected();
    ui.store
        .splice(position, 1, &[BoxedAnyObject::new(row.clone())]);
    if filter_position != u32::MAX {
        ui.select_quietly(filter_position);
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
    ensure_body(ui, row);
    if show_content {
        ui.list_split.set_show_content(true);
    }
    ui.status_label.set_text(&format!("Reading: {}", row.subject));

    if row.unread {
        let handle = ui.clone();
        let id = row.id;
        gtk::glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if handle.preview_generation.get() == generation {
                mark_read_by_id(&handle, id);
            }
        });
    }
}

fn render_preview(ui: &Ui, row: &MessageRow) {
    let theme = html_theme(&ui.conversation);
    ui.reading_subject.set_label(&row.subject);
    ui.reading_stack.set_visible_child_name("conversation");

    let generation = ui.preview_generation.get();

    // Drop any conversation still preloading from a previous selection.
    if let Some(pending) = ui.pending_preview.borrow_mut().take() {
        ui.preview_overlay.remove_overlay(&pending);
    }

    // If something is already on screen, build the next conversation off to the
    // side and only swap it in once it has rendered.
    let keep_previous = ui.conversation.first_child().is_some();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_valign(gtk::Align::Start);

    if !keep_previous {
        // No previous view: render straight into the visible pane.
        while let Some(child) = ui.conversation.first_child() {
            ui.conversation.remove(&child);
        }
        for widget in conversation_message_widgets(&ui.db, &theme, None, row) {
            ui.conversation.append(&widget);
        }
        return;
    }

    content.set_opacity(0.0);
    ui.preview_overlay.add_overlay(&content);
    *ui.pending_preview.borrow_mut() = Some(content.clone().upcast());

    let swapped = Rc::new(std::cell::Cell::new(false));
    let swap: Rc<dyn Fn()> = {
        let ui = ui.clone();
        let content = content.clone();
        let swapped = swapped.clone();
        Rc::new(move || {
            if swapped.replace(true) {
                return;
            }
            // A newer preview superseded this one; it will clean us up.
            if ui.preview_generation.get() != generation {
                return;
            }
            while let Some(child) = ui.conversation.first_child() {
                ui.conversation.remove(&child);
            }
            while let Some(child) = content.first_child() {
                content.remove(&child);
                ui.conversation.append(&child);
            }
            ui.preview_overlay.remove_overlay(&content);
            *ui.pending_preview.borrow_mut() = None;
        })
    };

    let ready = PreviewReady {
        remaining: Rc::new(std::cell::Cell::new(0)),
        notify: swap.clone(),
    };
    for widget in conversation_message_widgets(&ui.db, &theme, Some(&ready), row) {
        content.append(&widget);
    }
    // Nothing to wait for (no HTML bodies): swap right away.
    if ready.remaining.get() == 0 {
        (ready.notify)();
    }

    // Backstop: swap even if some view never reports ready.
    {
        let swap = swap.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(1), move || swap());
    }
}

/// Re-render the reading pane (e.g. after a body finished fetching).
pub(crate) fn reload_preview(ui: &Ui) {
    if let Some(row) = ui.preview_row.borrow().clone() {
        render_preview(ui, &row);
    }
}

/// Ask the sync engine for a message body if it is not cached yet.
fn ensure_body(ui: &Ui, row: &MessageRow) {
    let cached = ui
        .db
        .message_body(MessageId(row.id))
        .ok()
        .flatten()
        .map(|body| body.text.is_some() || body.html.is_some())
        .unwrap_or(false);
    if cached {
        return;
    }
    let account = AccountId(row.account_id);
    if ui.engine.is_running(account) {
        ui.engine.send(
            account,
            SyncCommand::FetchBody {
                message: MessageId(row.id),
            },
        );
    }
}

/// Mark a message read, updating the cache, the list row and the server.
fn mark_read_by_id(ui: &Ui, id: i64) {
    let mut update = None;
    update_row_by_id(ui, id, |row| {
        if row.unread {
            row.unread = false;
            row.flags.seen = true;
            update = Some((row.account_id, row.flags));
        }
    });
    if let Some((account, flags)) = update {
        if let Err(error) = ui.db.set_flags(MessageId(id), flags) {
            tracing::error!(%error, "failed to mark read");
        }
        ui.engine.send(
            AccountId(account),
            SyncCommand::SetFlags {
                message: MessageId(id),
                flags,
            },
        );
    }
}

/// Apply `update` to the store row with the given message id.
fn update_row_by_id(ui: &Ui, id: i64, update: impl FnOnce(&mut MessageRow)) {
    // Replacing a store item drops the selection (object identity changes), so
    // remember the selected row and restore it once the splice is done.
    let selected = ui.selection.selected();
    for position in 0..ui.store.n_items() {
        let Some(item) = ui.store.item(position) else {
            continue;
        };
        let Ok(boxed) = item.downcast::<BoxedAnyObject>() else {
            continue;
        };
        if boxed.borrow::<MessageRow>().id != id {
            continue;
        }
        let mut row = boxed.borrow::<MessageRow>().clone();
        update(&mut row);
        ui.store.splice(position, 1, &[BoxedAnyObject::new(row)]);
        if selected != u32::MAX {
            ui.select_quietly(selected);
        }
        break;
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
    let Some(mut row) = selected_row(ui) else {
        return;
    };
    row.starred = !row.starred;
    row.flags.flagged = row.starred;
    if let Err(error) = ui.db.set_flags(MessageId(row.id), row.flags) {
        tracing::error!(%error, "failed to update flags");
    }
    ui.engine.send(
        AccountId(row.account_id),
        SyncCommand::SetFlags {
            message: MessageId(row.id),
            flags: row.flags,
        },
    );
    replace_row(ui, &row);
    ui.status_label
        .set_text(if row.starred { "Starred" } else { "Unstarred" });
}

fn set_unread(ui: &Ui, unread: bool) {
    let Some(mut row) = selected_row(ui) else {
        return;
    };
    row.unread = unread;
    row.flags.seen = !unread;
    if let Err(error) = ui.db.set_flags(MessageId(row.id), row.flags) {
        tracing::error!(%error, "failed to update flags");
    }
    ui.engine.send(
        AccountId(row.account_id),
        SyncCommand::SetFlags {
            message: MessageId(row.id),
            flags: row.flags,
        },
    );
    replace_row(ui, &row);
    ui.status_label
        .set_text(if unread { "Marked unread" } else { "Marked read" });
}

/// Move the selected message to a special-use folder. When the account is
/// syncing, the engine applies it on the server and the list refreshes; when
/// offline, the cache is updated directly.
fn move_selected(ui: &Ui, special_use: SpecialUse, verb: &'static str) {
    let Some(row) = selected_row(ui) else {
        return;
    };
    let account = AccountId(row.account_id);
    let message = MessageId(row.id);

    if ui.engine.is_running(account) {
        ui.engine.send(account, SyncCommand::Move { message, to: special_use });
        ui.status_label.set_text(&format!("{verb} message"));
        return;
    }

    let filter_position = ui.selection.selected();
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
    let result = match target {
        Some(folder) => ui.db.move_message(message, folder),
        None => ui.db.delete_message(message),
    };
    if let Err(error) = result {
        tracing::error!(%error, "failed to move message");
    }
    if let Some(item) = ui.selection.selected_item() {
        if let Some(position) = ui.store.find(&item) {
            ui.store.remove(position);
        }
    }
    let count = ui.selection.n_items();
    if count > 0 && filter_position != u32::MAX {
        ui.selection.set_selected(filter_position.min(count - 1));
    }
    ui.status_label.set_text(&format!("{verb} message"));
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

    mark_read_by_id(ui, row.id);
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
            theme,
            ready,
        )];
    }
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
            conversation_message(
                message.from.display(),
                &format_timestamp(message.date.unix_timestamp()),
                &text,
                html.as_deref(),
                theme,
                ready,
            )
        })
        .collect()
}

fn conversation_message(
    sender: &str,
    date: &str,
    body: &str,
    html: Option<&str>,
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
    view.load_html(&themed_html(html, theme), None);

    {
        let finish = finish.clone();
        view.connect_load_changed(move |view, event| {
            if event == webkit6::LoadEvent::Finished {
                measure_view_height(view);
                // `Finished` only means the load completed; wait until the page
                // can be rasterized (snapshot succeeds) before revealing.
                let finish = finish.clone();
                view.snapshot(
                    webkit6::SnapshotRegion::Visible,
                    webkit6::SnapshotOptions::NONE,
                    None::<&gtk::gio::Cancellable>,
                    move |_result| finish(),
                );
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
