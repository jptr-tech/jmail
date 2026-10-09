//! The composer: a markdown editor with live styling plus reply/reply-all/
//! forward/draft setup, Send and Save draft. It is embedded either in a dialog
//! (new message / draft editing) or inline at the bottom of the reading pane.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{Align, Orientation};

use jmail_core::outgoing::{display_address, reply_address, reply_recipients};
use jmail_core::{
    parse_address_list, prefixed_subject, Account, Address, DraftRef, Message, MessageId,
    OutgoingMessage, SyncCommand,
};

use crate::markdown_editor::MarkdownEditor;
use crate::message_row::format_timestamp;
use crate::window::Ui;

/// Which kind of message the composer was opened for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    New,
    Reply,
    ReplyAll,
    Forward,
    Draft,
}

impl Mode {
    fn title(self) -> &'static str {
        match self {
            Self::New => "New Message",
            Self::Reply => "Reply",
            Self::ReplyAll => "Reply All",
            Self::Forward => "Forward",
            Self::Draft => "Draft",
        }
    }
}

/// Result of a send/draft attempt, routed back to the originating composer.
pub enum ComposeOutcome {
    Send {
        error: Option<String>,
    },
    Draft {
        draft: Option<DraftRef>,
        error: Option<String>,
    },
}

/// A reusable composer widget.
struct Composer {
    root: gtk::Box,
    actions: gtk::Box,
    id: u64,
    mode: Mode,
    accounts: Vec<Account>,
    from: gtk::DropDown,
    to: gtk::Entry,
    cc: gtk::Entry,
    bcc: gtk::Entry,
    subject: gtk::Entry,
    editor: MarkdownEditor,
    banner: gtk::Label,
    send: gtk::Button,
    save: gtk::Button,
    preview: gtk::ToggleButton,
    message_id: Rc<RefCell<String>>,
    draft: Rc<RefCell<Option<DraftRef>>>,
    target: Option<Message>,
    /// Dismisses the composer's container (dialog / inline slot) on submit so
    /// sending never blocks the UI; failures re-present it.
    on_submit: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
}

impl Composer {
    fn new(ui: &Ui, mode: Mode, target: Option<Message>, compact: bool) -> Rc<Self> {
        let all_accounts = ui.db.accounts().unwrap_or_default();
        let accounts: Vec<Account> = all_accounts
            .iter()
            .filter(|account| account.enabled)
            .cloned()
            .collect();
        let my_addresses: Vec<String> = all_accounts
            .iter()
            .map(|account| account.email.clone())
            .collect();
        let default_index = target
            .as_ref()
            .and_then(|target| accounts.iter().position(|account| account.id == target.account_id))
            .unwrap_or(0);

        let from = gtk::DropDown::from_strings(
            &accounts
                .iter()
                .map(|account| account.email.as_str())
                .collect::<Vec<_>>(),
        );
        from.set_selected(default_index as u32);

        let to = gtk::Entry::builder().placeholder_text("To").build();
        let cc = gtk::Entry::builder().placeholder_text("Cc").build();
        let bcc = gtk::Entry::builder().placeholder_text("Bcc").build();
        let subject = gtk::Entry::builder().placeholder_text("Subject").build();

        let mut draft_ref: Option<DraftRef> = None;
        let mut initial_markdown = String::new();

        if let Some(target) = &target {
            let body = ui
                .db
                .message_body(target.id)
                .ok()
                .flatten()
                .and_then(|body| body.text);
            match mode {
                Mode::Reply | Mode::ReplyAll => {
                    subject.set_text(&prefixed_subject(&target.subject, "Re: "));
                    let (to_list, cc_list) =
                        reply_recipients(target, &my_addresses, mode == Mode::ReplyAll);
                    to.set_text(&display_address_list(&to_list));
                    cc.set_text(&display_address_list(&cc_list));
                    initial_markdown = quote_reply(target, body.as_deref(), &my_addresses);
                }
                Mode::Forward => {
                    subject.set_text(&prefixed_subject(&target.subject, "Fwd: "));
                    initial_markdown = forward_body(target, body.as_deref());
                }
                Mode::Draft => {
                    subject.set_text(&target.subject);
                    to.set_text(&display_address_list(&target.to));
                    cc.set_text(&display_address_list(&target.cc));
                    initial_markdown = body.unwrap_or_default();
                    if let Ok(Some((_, path, _, uid, _))) = ui.db.message_location(target.id) {
                        draft_ref = Some(DraftRef {
                            folder: path,
                            uid,
                            message_id: target.message_id.clone(),
                        });
                    }
                }
                Mode::New => {}
            }
        }

        let editor = MarkdownEditor::new();
        editor.set_text(&initial_markdown);
        if mode == Mode::Draft {
            // Continue where the draft left off.
            editor.place_cursor_at_end();
        }

        // Show the field set the mode needs; compact inline replies hide the
        // fields Gmail hides.
        let show_cc = !compact || mode == Mode::ReplyAll;
        let show_bcc = !compact;
        let show_subject = !compact;
        let show_from = !compact || accounts.len() > 1;
        cc.set_visible(show_cc);
        bcc.set_visible(show_bcc);
        subject.set_visible(show_subject);

        let from_label = gtk::Label::builder().label("From").xalign(0.0).build();
        from_label.add_css_class("dim-label");
        let from_row = gtk::Box::new(Orientation::Horizontal, 8);
        from_row.append(&from_label);
        from_row.append(&from);
        from_row.set_visible(show_from);

        let banner = gtk::Label::builder().xalign(0.0).wrap(true).build();
        banner.add_css_class("error");
        banner.set_visible(false);

        let margin = if compact { 8 } else { 12 };
        let header = gtk::Box::new(Orientation::Vertical, 6);
        header.set_margin_top(margin);
        header.set_margin_bottom(6);
        header.set_margin_start(margin);
        header.set_margin_end(margin);
        header.append(&from_row);
        header.append(&to);
        header.append(&cc);
        header.append(&bcc);
        header.append(&subject);

        // Attachment chips for forwarded mail.
        if let (Mode::Forward, Some(target)) = (mode, &target) {
            if let Ok(attachments) = ui.db.attachments(target.id) {
                if !attachments.is_empty() {
                    let chips = gtk::Box::new(Orientation::Horizontal, 6);
                    for attachment in &attachments {
                        let chip = gtk::Label::new(Some(&format!(
                            "📎 {}",
                            if attachment.filename.is_empty() {
                                &attachment.mime
                            } else {
                                &attachment.filename
                            }
                        )));
                        chip.add_css_class("attachment");
                        chips.append(&chip);
                    }
                    header.append(&chips);
                }
            }
        }

        let preview = gtk::ToggleButton::with_label("Preview");
        let send = gtk::Button::with_label("Send");
        send.add_css_class("suggested-action");
        let save = gtk::Button::with_label("Save draft");

        let actions = gtk::Box::new(Orientation::Horizontal, 6);
        actions.set_halign(Align::End);
        actions.set_margin_top(6);
        actions.set_margin_bottom(margin);
        actions.set_margin_start(margin);
        actions.set_margin_end(margin);
        actions.append(&banner);
        banner.set_hexpand(true);
        actions.append(&preview);
        actions.append(&save);
        actions.append(&send);

        let root = gtk::Box::new(Orientation::Vertical, 0);
        root.add_css_class("composer");
        root.append(&header);
        root.append(&gtk::Separator::new(Orientation::Horizontal));
        root.append(&editor.container);
        root.append(&gtk::Separator::new(Orientation::Horizontal));
        root.append(&actions);

        Rc::new(Self {
            root,
            actions,
            id: ui.register_compose(),
            mode,
            accounts,
            from,
            to,
            cc,
            bcc,
            subject,
            editor,
            banner,
            send,
            save,
            preview,
            message_id: Rc::new(RefCell::new(OutgoingMessage::generate_message_id(
                all_accounts
                    .first()
                    .map(|account| account.email.clone())
                    .as_deref()
                    .unwrap_or("jmail.local"),
            ))),
            draft: Rc::new(RefCell::new(draft_ref)),
            target,
            on_submit: Rc::new(RefCell::new(None)),
        })
    }

    /// Wire the buttons once the `Rc<Composer>` exists.
    fn wire(self: &Rc<Self>, ui: &Ui) {
        {
            let ui = ui.clone();
            let composer = self.clone();
            self.send.connect_clicked(move |_| composer.submit(&ui));
        }
        {
            let ui = ui.clone();
            let composer = self.clone();
            self.save.connect_clicked(move |_| composer.save_draft(&ui));
        }
        {
            let composer = self.clone();
            self.preview
                .connect_toggled(move |button| composer.editor.set_preview(button.is_active()));
        }
        {
            // Shift/Ctrl+Enter sends. Use a capture-phase controller so it works
            // while the editor has focus (Return would otherwise insert a
            // newline). Ctrl+C etc. are left alone for the editor to handle.
            let ui = ui.clone();
            let composer = self.clone();
            let controller = gtk::EventControllerKey::new();
            controller.set_propagation_phase(gtk::PropagationPhase::Capture);
            controller.connect_key_pressed(move |_, key, _, state| {
                let send_mod = state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                    || state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                if send_mod && matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
                    composer.submit(&ui);
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            });
            self.root.add_controller(controller);
        }
    }

    fn account(&self) -> Option<Account> {
        self.accounts.get(self.from.selected() as usize).cloned()
    }

    fn focus_editor(&self) {
        self.editor.focus();
    }

    /// Install the callback that dismisses this composer's container on submit
    /// (closing the dialog / removing the inline slot) so sending is
    /// non-blocking. A send failure re-presents the composer.
    fn set_on_submit(&self, callback: Rc<dyn Fn()>) {
        *self.on_submit.borrow_mut() = Some(callback);
    }

    fn build_outgoing(&self, account: &Account) -> OutgoingMessage {
        let (in_reply_to, references, forward_of) = match (&self.target, self.mode) {
            (Some(target), Mode::Reply | Mode::ReplyAll) => {
                let mut references = target.references.clone();
                if !target.message_id.is_empty() {
                    references.push(target.message_id.clone());
                }
                (target.message_id.clone(), references, None)
            }
            (Some(target), Mode::Forward) => (String::new(), Vec::new(), Some(target.id)),
            _ => (String::new(), Vec::new(), None),
        };
        OutgoingMessage {
            message_id: self.message_id.borrow().clone(),
            from: Address::new(account.display_name.clone(), account.email.clone()),
            to: parse_address_list(&self.to.text()),
            cc: parse_address_list(&self.cc.text()),
            bcc: parse_address_list(&self.bcc.text()),
            subject: self.subject.text().to_string(),
            markdown: self.editor.text(),
            in_reply_to,
            references,
            attachments: Vec::new(),
            forward_of,
        }
    }

    fn set_error(&self, message: &str) {
        self.banner.set_text(message);
        self.banner.set_visible(true);
    }

    fn clear_error(&self) {
        self.banner.set_visible(false);
    }

    fn submit(&self, ui: &Ui) {
        if !self.send.is_sensitive() {
            return;
        }
        let Some(account) = self.account() else {
            self.set_error("Pick a sending account");
            return;
        };
        if !ui.engine.is_running(account.id) {
            self.set_error(&format!("{} is not connected", account.email));
            return;
        }
        self.clear_error();
        self.send.set_sensitive(false);
        let outgoing = self.build_outgoing(&account);
        let delete_draft = self.draft.borrow().clone();
        ui.engine.send(
            account.id,
            SyncCommand::Send {
                id: self.id,
                message: Box::new(outgoing),
                delete_draft,
            },
        );
        // Dismiss immediately: the Sent copy and any error arrive via events.
        if let Some(callback) = self.on_submit.borrow().clone() {
            callback();
        }
    }

    fn save_draft(&self, ui: &Ui) {
        let Some(account) = self.account() else {
            self.set_error("Pick a sending account");
            return;
        };
        if !ui.engine.is_running(account.id) {
            self.set_error(&format!("{} is not connected", account.email));
            return;
        }
        self.clear_error();
        let outgoing = self.build_outgoing(&account);
        let replace = self.draft.borrow().clone();
        ui.engine.send(
            account.id,
            SyncCommand::SaveDraft {
                id: self.id,
                message: Box::new(outgoing),
                replace,
            },
        );
    }

    fn handle_outcome(&self, ui: &Ui, outcome: ComposeOutcome) {
        match outcome {
            ComposeOutcome::Send { error } => match error {
                None => {
                    ui.clear_inline();
                    ui.reload_messages();
                    // Re-render the current thread so the Sent copy shows up,
                    // and fetch its body (reload_preview also requests bodies).
                    crate::actions::reload_preview(ui);
                    ui.scroll_reading_to_bottom();
                    ui.status_label.set_text("Message sent");
                }
                Some(error) => {
                    self.send.set_sensitive(true);
                    self.set_error(&format!("Send failed: {error}"));
                }
            },
            ComposeOutcome::Draft { draft, error } => match error {
                None => {
                    *self.draft.borrow_mut() = draft.clone();
                    if let Some(draft) = draft {
                        self.message_id.replace(draft.message_id);
                    }
                    self.send.set_sensitive(true);
                    self.set_error("Draft saved");
                    ui.status_label.set_text("Draft saved");
                }
                Some(error) => {
                    self.send.set_sensitive(true);
                    self.set_error(&format!("Draft failed: {error}"));
                }
            },
        }
    }
}

/// Open the composer in a modal dialog (new message / draft editing).
pub fn present(ui: &Ui, mode: Mode, target: Option<MessageId>) {
    present_in(&ui.window, ui, mode, target);
}

/// Open the composer in a dialog parented to `parent` — the main window, or a
/// detached message window.
pub fn present_in(
    parent: &impl IsA<gtk::Widget>,
    ui: &Ui,
    mode: Mode,
    target: Option<MessageId>,
) {
    let accounts = ui.db.accounts().unwrap_or_default();
    if !accounts.iter().any(|account| account.enabled) {
        ui.status_label.set_text("Add an account before composing");
        return;
    }
    let target = target.and_then(|id| ui.db.get_message(id).ok().flatten());
    let composer = Composer::new(ui, mode, target, false);
    composer.wire(ui);

    let dialog = adw::Dialog::builder()
        .title(mode.title())
        .content_width(720)
        .content_height(600)
        .child(&composer.root)
        .build();

    // A dialog needs its own Cancel; inline gets a Discard instead.
    let cancel = gtk::Button::with_label("Cancel");
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }
    composer.actions.insert_child_after(&cancel, Some(&composer.preview));

    // Submitting dismisses the dialog right away (sending is non-blocking).
    {
        let dialog = dialog.clone();
        composer.set_on_submit(Rc::new(move || {
            dialog.close();
        }));
    }

    {
        let ui = ui.clone();
        let composer = composer.clone();
        let dialog = dialog.clone();
        let parent: gtk::Widget = parent.clone().upcast();
        ui.clone().register_compose_handler(
            composer.id,
            Rc::new(move |outcome| match outcome {
                ComposeOutcome::Send { error: None } => {
                    ui.unregister_compose(composer.id);
                    dialog.close();
                    ui.status_label.set_text("Message sent");
                    ui.reload_messages();
                }
                ComposeOutcome::Send { error: Some(error) } => {
                    // Re-present so the content is not lost and can be retried.
                    composer.send.set_sensitive(true);
                    composer.set_error(&format!("Send failed: {error}"));
                    ui.status_label.set_text(&format!("Send failed: {error}"));
                    ui.toast(&format!("Send failed: {error}"));
                    dialog.present(Some(&parent));
                }
                other => composer.handle_outcome(&ui, other),
            }),
        );
    }

    // Save unsent content when the dialog is dismissed (but not while sending).
    {
        let ui = ui.clone();
        let composer = composer.clone();
        dialog.connect_close_attempt(move |_| {
            if !composer.editor.is_empty() && composer.send.is_sensitive() {
                composer.save_draft(&ui);
            }
        });
    }
    {
        let ui = ui.clone();
        let id = composer.id;
        let composer = composer.clone();
        dialog.connect_closed(move |_| {
            // Keep the result handler alive while a send is in flight.
            if composer.send.is_sensitive() {
                ui.unregister_compose(id);
            }
        });
    }

    dialog.present(Some(parent));
}

/// Open the composer inline at the bottom of a reading pane.
pub fn present_inline(
    ui: &Ui,
    pane: &Rc<crate::actions::ReadingPane>,
    mode: Mode,
    target: Option<MessageId>,
) {
    let accounts = ui.db.accounts().unwrap_or_default();
    if !accounts.iter().any(|account| account.enabled) {
        ui.status_label.set_text("Add an account before composing");
        return;
    }
    pane.clear_inline(ui);
    let target = target.and_then(|id| ui.db.get_message(id).ok().flatten());
    let composer = Composer::new(ui, mode, target, true);
    composer.wire(ui);
    pane.set_inline_composer(composer.id);

    // Submitting detaches the composer right away (sending is non-blocking).
    {
        let pane = pane.clone();
        composer.set_on_submit(Rc::new(move || pane.detach_inline()));
    }

    {
        let ui = ui.clone();
        let composer = composer.clone();
        let pane = pane.clone();
        ui.clone().register_compose_handler(
            composer.id,
            Rc::new(move |outcome| match outcome {
                ComposeOutcome::Send { error: None } => {
                    ui.unregister_compose(composer.id);
                    ui.status_label.set_text("Message sent");
                    ui.reload_messages();
                    crate::actions::reload_preview(&ui);
                }
                ComposeOutcome::Send { error: Some(error) } => {
                    composer.send.set_sensitive(true);
                    composer.set_error(&format!("Send failed: {error}"));
                    ui.status_label.set_text(&format!("Send failed: {error}"));
                    ui.toast(&format!("Send failed: {error}"));
                    // Put the composer back where it was so it can be retried.
                    pane.compose_slot.append(&composer.root);
                    pane.reply_bar.set_visible(false);
                    pane.set_inline_composer(composer.id);
                    pane.scroll_to_bottom();
                }
                other => composer.handle_outcome(&ui, other),
            }),
        );
    }

    let discard = gtk::Button::with_label("Discard");
    {
        let ui = ui.clone();
        let pane = pane.clone();
        discard.connect_clicked(move |_| pane.clear_inline(&ui));
    }
    composer
        .actions
        .insert_child_after(&discard, Some(&composer.preview));

    pane.compose_slot.append(&composer.root);
    pane.reply_bar.set_visible(false);
    composer.focus_editor();
    pane.scroll_to_bottom();
}

fn display_address_list(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(display_address)
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_reply(target: &Message, body: Option<&str>, my_addresses: &[String]) -> String {
    let from = reply_address(target);
    let author = if my_addresses
        .iter()
        .any(|mine| mine.eq_ignore_ascii_case(&from.email))
    {
        "You"
    } else {
        from.display()
    };
    let attribution = format!(
        "On {}, {} wrote:",
        format_timestamp(target.date.unix_timestamp()),
        author
    );
    let mut markdown = String::from("\n\n");
    markdown.push_str(&attribution);
    markdown.push('\n');
    if let Some(body) = body {
        for line in body.lines() {
            markdown.push_str("> ");
            markdown.push_str(line);
            markdown.push('\n');
        }
    }
    markdown
}

fn forward_body(target: &Message, body: Option<&str>) -> String {
    let mut markdown = String::from("\n\n---------- Forwarded message ----------\n");
    markdown.push_str(&format!("From: {}\n", display_address(&target.from)));
    markdown.push_str(&format!(
        "Date: {}\n",
        format_timestamp(target.date.unix_timestamp())
    ));
    markdown.push_str(&format!("Subject: {}\n", target.subject));
    if !target.to.is_empty() {
        markdown.push_str(&format!("To: {}\n", display_address_list(&target.to)));
    }
    markdown.push('\n');
    if let Some(body) = body {
        markdown.push_str(body);
        markdown.push('\n');
    }
    markdown
}
