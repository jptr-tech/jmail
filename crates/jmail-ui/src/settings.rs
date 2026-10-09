use std::rc::Rc;

use adw::prelude::*;
use gtk::Orientation;

use jmail_core::{Account, Credential, Security};

/// Open the preferences dialog.
pub fn present(parent: Option<&impl IsA<gtk::Widget>>) {
    let dialog = adw::PreferencesDialog::builder()
        .title("Settings")
        .content_height(800)
        .build();
    dialog.add(&accounts_page());
    dialog.add(&notifications_page());
    dialog.add(&reading_page());
    dialog.add(&sync_page());
    dialog.add(&advanced_page());
    dialog.present(parent);
}

fn advanced_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Advanced")
        .icon_name("preferences-other-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Cache")
        .description("Rebuild the local mail cache from the server.")
        .build();

    let resync = adw::ActionRow::builder()
        .title("Force resync")
        .subtitle("Delete cached mail and re-fetch every folder for all accounts")
        .build();
    let button = gtk::Button::with_label("Resync");
    button.set_valign(gtk::Align::Center);
    button.add_css_class("destructive-action");
    button.connect_clicked(|_| force_resync());
    resync.add_suffix(&button);
    group.add(&resync);

    page.add(&group);
    page
}

/// Drop the cached mail for every account and rebuild from the server.
fn force_resync() {
    let Some(context) = crate::context::get() else {
        return;
    };
    let accounts = context.db.accounts().unwrap_or_default();
    for account in accounts {
        let id = account.id;
        if context.engine.is_running(id) {
            context.engine.send(id, jmail_core::SyncCommand::Rebuild);
        } else {
            if let Err(error) = context.db.clear_account_messages(id) {
                tracing::error!(%error, "could not clear cached mail");
            }
            if let Err(error) = context.db.reset_folder_sync_state(id) {
                tracing::error!(%error, "could not reset folder sync state");
            }
            let engine = context.engine.clone();
            context.runtime.spawn(async move {
                let _ = engine.start_account(id).await;
            });
        }
    }
}

/// Show the About dialog.
pub fn about(parent: Option<&impl IsA<gtk::Widget>>) {
    let dialog = adw::AboutDialog::builder()
        .application_name("Jmail")
        .application_icon(crate::APP_ID)
        .developer_name("jptr")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A fast, keyboard-driven GTK4 email client.")
        .build();
    dialog.present(parent);
}

/// Show the keyboard shortcuts reference.
pub fn shortcuts(parent: Option<&impl IsA<gtk::Widget>>) {
    let dialog = adw::ShortcutsDialog::new();

    dialog.add(section(
        "Navigation",
        &[
            ("Next (older) message", "j"),
            ("Previous (newer) message", "k"),
            ("Open in new window", "o"),
            ("Back to list", "u"),
        ],
    ));
    dialog.add(section(
        "Compose & reply",
        &[
            ("Compose", "c"),
            ("Reply", "r"),
            ("Reply all", "a"),
            ("Forward", "f"),
            ("Send", "<Shift>Return"),
        ],
    ));
    dialog.add(section(
        "Message actions",
        &[
            ("Archive", "e"),
            ("Delete", "numbersign"),
            ("Star", "s"),
            ("Mark read", "<Shift>i"),
            ("Mark unread", "<Shift>u"),
            ("Select", "x"),
            ("Move to folder", "v"),
            ("Label as", "l"),
        ],
    ));

    let jumping = adw::ShortcutsSection::new(Some("Jumping"));
    let jump_item = adw::ShortcutsItem::new("Jump to a folder", "g");
    jump_item.set_subtitle("then i Inbox · t Sent · d Drafts · a All Mail");
    jumping.add(jump_item);
    dialog.add(jumping);

    dialog.add(section(
        "Application",
        &[
            ("Search", "slash"),
            ("Settings", "<Shift>comma"),
            ("Keyboard shortcuts", "question"),
        ],
    ));

    dialog.present(parent);
}

fn section(title: &str, items: &[(&str, &str)]) -> adw::ShortcutsSection {
    let section = adw::ShortcutsSection::new(Some(title));
    for &(label, accelerator) in items {
        section.add(adw::ShortcutsItem::new(label, accelerator));
    }
    section
}

fn accounts_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Accounts")
        .icon_name("avatar-default-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Mail Accounts")
        .description("Accounts set up in GNOME Online Accounts appear here automatically. Other IMAP/SMTP servers can be added with a password.")
        .build();
    let accounts = AccountsGroup::new(group.clone());
    accounts.refresh();

    page.add(&group);
    page
}

/// Handle to the accounts preferences group.
///
/// `AdwPreferencesGroup` cannot enumerate the widgets added to it, so we keep
/// our own list of rows in order to remove exactly those when refreshing.
#[derive(Clone)]
struct AccountsGroup {
    group: adw::PreferencesGroup,
    rows: std::rc::Rc<std::cell::RefCell<Vec<gtk::Widget>>>,
}

impl AccountsGroup {
    fn new(group: adw::PreferencesGroup) -> Self {
        Self {
            group,
            rows: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        }
    }

    fn clear(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
    }

    fn push(&self, row: &gtk::Widget) {
        self.group.add(row);
        self.rows.borrow_mut().push(row.clone());
    }

    /// (Re)build the account rows: an "Add Account…" row plus one row per account.
    fn refresh(&self) {
        self.clear();

        let add = adw::ActionRow::builder()
            .title("Add Account…")
            .subtitle("IMAP/SMTP with a password")
            .build();
        let add_button = gtk::Button::with_label("Add");
        add_button.set_valign(gtk::Align::Center);
        add_button.add_css_class("suggested-action");
        {
            let accounts = self.clone();
            add_button.connect_clicked(move |button| {
                let parent = button
                    .root()
                    .and_then(|root| root.downcast::<gtk::Window>().ok());
                let on_added: Rc<dyn Fn()> = {
                    let accounts = accounts.clone();
                    Rc::new(move || accounts.refresh())
                };
                show_account_dialog(parent.as_ref(), None, on_added);
            });
        }
        add.add_suffix(&add_button);
        self.push(add.upcast_ref());

        let Some(context) = crate::context::get() else {
            return;
        };
        let accounts = context.db.accounts().unwrap_or_default();
        for account in accounts {
            let row = adw::ActionRow::builder()
                .title(&account.email)
                .subtitle(account_subtitle(&account))
                .build();

            if account.goa_object_path.is_some() {
                // Accounts provided by GNOME Online Accounts are reconciled on
                // every launch, so they can be disabled but not removed here.
                let toggle = gtk::Switch::builder()
                    .valign(gtk::Align::Center)
                    .active(account.enabled)
                    .tooltip_text("Sync this account")
                    .build();
                {
                    let account_id = account.id;
                    let engine = context.engine.clone();
                    let db = context.db.clone();
                    let runtime = context.runtime.clone();
                    toggle.connect_state_set(move |_, active| {
                        if let Err(error) = db.set_account_enabled(account_id, active) {
                            tracing::error!(%error, "could not update account");
                        }
                        if active {
                            let engine = engine.clone();
                            runtime.spawn(async move {
                                let _ = engine.start_account(account_id).await;
                            });
                        } else {
                            engine.stop_account(account_id);
                        }
                        engine.notify_accounts_changed();
                        gtk::glib::Propagation::Proceed
                    });
                }
                row.add_suffix(&toggle);
            } else {
                let actions = gtk::Box::new(Orientation::Horizontal, 6);
                actions.set_valign(gtk::Align::Center);

                let edit = gtk::Button::with_label("Edit");
                {
                    let accounts = self.clone();
                    let account = account.clone();
                    edit.connect_clicked(move |button| {
                        let parent = button
                            .root()
                            .and_then(|root| root.downcast::<gtk::Window>().ok());
                        let accounts = accounts.clone();
                        present_edit_account(parent.as_ref(), account.clone(), move || {
                            accounts.refresh()
                        });
                    });
                }
                actions.append(&edit);

                let remove = gtk::Button::with_label("Remove");
                remove.add_css_class("destructive-action");
                {
                    let accounts = self.clone();
                    let account_id = account.id;
                    let engine = context.engine.clone();
                    let db = context.db.clone();
                    remove.connect_clicked(move |_| {
                        engine.stop_account(account_id);
                        if let Err(error) = jmail_core::secrets::delete_password(&db, account_id) {
                            tracing::warn!(%error, "could not delete stored password");
                        }
                        if let Err(error) = db.remove_account(account_id) {
                            tracing::error!(%error, "could not remove account");
                        }
                        engine.notify_accounts_changed();
                        accounts.refresh();
                    });
                }
                actions.append(&remove);
                row.add_suffix(&actions);
            }
            self.push(row.upcast_ref());
        }
    }
}

/// A short human-readable description of an account's servers.
fn account_subtitle(account: &Account) -> String {
    let security = match account.imap_security {
        Security::Tls => "TLS",
        Security::StartTls => "STARTTLS",
        Security::None => "No encryption",
    };
    let server = if account.imap_host.is_empty() {
        "no IMAP server configured".to_string()
    } else {
        format!("{}:{} · {security}", account.imap_host, account.imap_port)
    };
    if account.goa_object_path.is_some() {
        format!("Online Accounts · {server}")
    } else {
        server
    }
}

fn entry(placeholder: &str) -> gtk::Entry {
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    entry
}

fn labeled(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 8);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_size_request(90, -1);
    text.add_css_class("dim-label");
    row.append(&text);
    widget.set_hexpand(true);
    row.append(widget);
    row
}

/// Show the manual "Add Account" dialog directly (used by first-run
/// onboarding as well as the Settings page).
pub fn present_add_account(parent: Option<&gtk::Window>, on_saved: impl Fn() + 'static) {
    show_account_dialog(parent, None, Rc::new(on_saved));
}

/// Edit an existing manually-configured account.
pub fn present_edit_account(
    parent: Option<&gtk::Window>,
    account: Account,
    on_saved: impl Fn() + 'static,
) {
    show_account_dialog(parent, Some(account), Rc::new(on_saved));
}

fn show_account_dialog(
    parent: Option<&gtk::Window>,
    existing: Option<Account>,
    on_saved: Rc<dyn Fn()>,
) {
    let editing = existing.is_some();
    let parent = parent.cloned();

    let name = entry("Name");
    let email = entry("you@example.com");
    let host = entry("imap.example.com");
    let port = entry("993");
    let username = entry("Username (defaults to email)");
    let password = gtk::PasswordEntry::new();
    password.set_show_peek_icon(true);
    let security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
    security.set_selected(0);
    let smtp_host = entry("smtp.example.com");
    let smtp_port = entry("465");
    let smtp_security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
    smtp_security.set_selected(0);

    if let Some(account) = &existing {
        name.set_text(&account.name);
        email.set_text(&account.email);
        host.set_text(&account.imap_host);
        port.set_text(&account.imap_port.to_string());
        security.set_selected(security_index(account.imap_security));
        username.set_text(&account.username);
        password.set_placeholder_text(Some("Leave blank to keep current password"));
        smtp_host.set_text(&account.smtp_host);
        smtp_port.set_text(&account.smtp_port.to_string());
        smtp_security.set_selected(security_index(account.smtp_security));
    } else {
        password.set_placeholder_text(Some("Password"));
    }

    let form = gtk::Box::new(Orientation::Vertical, 8);
    form.set_margin_top(16);
    form.set_margin_bottom(16);
    form.set_margin_start(16);
    form.set_margin_end(16);
    form.append(&labeled("Name", &name));
    form.append(&labeled("Email", &email));
    form.append(&labeled("IMAP server", &host));
    form.append(&labeled("Port", &port));
    form.append(&labeled("Security", &security));
    form.append(&labeled("Username", &username));
    form.append(&labeled("Password", &password));
    form.append(&gtk::Separator::new(Orientation::Horizontal));
    form.append(&labeled("SMTP server", &smtp_host));
    form.append(&labeled("SMTP port", &smtp_port));
    form.append(&labeled("SMTP security", &smtp_security));

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    let status = gtk::Label::builder().xalign(0.0).wrap(true).visible(false).build();
    let status_row = gtk::Box::new(Orientation::Horizontal, 8);
    status_row.set_margin_start(16);
    status_row.set_margin_end(16);
    status_row.set_margin_bottom(8);
    status_row.append(&spinner);
    status_row.append(&status);

    let cancel = gtk::Button::with_label("Cancel");
    let test = gtk::Button::with_label("Test");
    let save = gtk::Button::with_label(if editing { "Save" } else { "Add Account" });
    save.add_css_class("suggested-action");
    let actions = gtk::Box::new(Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(8);
    actions.set_margin_bottom(12);
    actions.set_margin_start(12);
    actions.set_margin_end(12);
    actions.append(&cancel);
    actions.append(&test);
    actions.append(&save);

    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&form);
    content.append(&status_row);
    content.append(&gtk::Separator::new(Orientation::Horizontal));
    content.append(&actions);

    let dialog = adw::Dialog::builder()
        .title(if editing { "Edit Account" } else { "Add Account" })
        .content_width(440)
        .child(&content)
        .build();

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }

    let form = Rc::new(AccountForm {
        existing,
        name: name.clone(),
        email: email.clone(),
        host: host.clone(),
        port: port.clone(),
        security: security.clone(),
        username: username.clone(),
        password: password.clone(),
        smtp_host: smtp_host.clone(),
        smtp_port: smtp_port.clone(),
        smtp_security: smtp_security.clone(),
    });
    // Remembers that the user already accepted an untrusted certificate, so
    // Save does not ask a second time after a successful Test.
    let accepted_invalid = Rc::new(std::cell::Cell::new(false));

    {
        let form = form.clone();
        let status = status.clone();
        let spinner = spinner.clone();
        let test_button = test.clone();
        let save_button = save.clone();
        let parent = parent.clone();
        let accepted_invalid = accepted_invalid.clone();
        test.connect_clicked(move |_| {
            let account = form.verify_account();
            let credential = match form.credential() {
                Ok(credential) => credential,
                Err(message) => {
                    set_status(&status, &message, true);
                    return;
                }
            };
            set_busy(
                &test_button,
                &save_button,
                &spinner,
                &status,
                "Testing connection…",
            );
            let on_result: Rc<dyn Fn(std::result::Result<bool, String>)> = {
                let status = status.clone();
                let spinner = spinner.clone();
                let test_button = test_button.clone();
                let save_button = save_button.clone();
                Rc::new(move |result| {
                    set_idle(&test_button, &save_button, &spinner);
                    match result {
                        Ok(false) => set_status(&status, "Connection successful", false),
                        Ok(true) => set_status(
                            &status,
                            "Connection successful (certificate accepted)",
                            false,
                        ),
                        Err(message) => set_status(&status, &message, true),
                    }
                })
            };
            verify_account_async(
                parent.clone(),
                account,
                credential,
                accepted_invalid.clone(),
                on_result,
            );
        });
    }

    {
        let form = form.clone();
        let status = status.clone();
        let spinner = spinner.clone();
        let test_button = test.clone();
        let save_button = save.clone();
        let parent = parent.clone();
        let dialog = dialog.clone();
        let on_saved = on_saved.clone();
        let accepted_invalid = accepted_invalid.clone();
        save.connect_clicked(move |_| {
            let account = form.verify_account();
            let credential = match form.credential() {
                Ok(credential) => credential,
                Err(message) => {
                    set_status(&status, &message, true);
                    return;
                }
            };
            set_busy(
                &test_button,
                &save_button,
                &spinner,
                &status,
                "Testing connection…",
            );
            let entered_password = form.password.text().to_string();
            let on_result: Rc<dyn Fn(std::result::Result<bool, String>)> = {
                let status = status.clone();
                let spinner = spinner.clone();
                let test_button = test_button.clone();
                let save_button = save_button.clone();
                let dialog = dialog.clone();
                let on_saved = on_saved.clone();
                let form = form.clone();
                Rc::new(move |result| {
                    set_idle(&test_button, &save_button, &spinner);
                    match result {
                        Ok(accept_invalid) => {
                            let mut account = form.build_account();
                            if accept_invalid {
                                account.accept_invalid_certs = true;
                            }
                            if persist_account(&form, &account, &entered_password) {
                                on_saved();
                                dialog.close();
                            } else {
                                set_status(&status, "Could not save the account", true);
                            }
                        }
                        Err(message) => set_status(&status, &message, true),
                    }
                })
            };
            verify_account_async(
                parent.clone(),
                account,
                credential,
                accepted_invalid.clone(),
                on_result,
            );
        });
    }

    dialog.present(parent.as_ref());
}

/// The add/edit form fields plus the account being edited, if any.
struct AccountForm {
    existing: Option<Account>,
    name: gtk::Entry,
    email: gtk::Entry,
    host: gtk::Entry,
    port: gtk::Entry,
    security: gtk::DropDown,
    username: gtk::Entry,
    password: gtk::PasswordEntry,
    smtp_host: gtk::Entry,
    smtp_port: gtk::Entry,
    smtp_security: gtk::DropDown,
}

impl AccountForm {
    /// Build the account as it should be saved (with the SMTP host falling back
    /// to the IMAP host, matching the previous behaviour).
    fn build_account(&self) -> Account {
        let email = self.email.text().to_string();
        let mut account = match &self.existing {
            Some(existing) => existing.clone(),
            None => Account::generic(email.clone()),
        };
        account.auth = jmail_core::AuthKind::Password;
        account.provider = jmail_core::Provider::Generic;
        account.name = if self.name.text().is_empty() {
            email.clone()
        } else {
            self.name.text().to_string()
        };
        account.email = email.clone();
        account.imap_host = self.host.text().trim().to_string();
        account.imap_port = self.port.text().trim().parse().unwrap_or(993);
        account.imap_security = security_from_index(self.security.selected());
        let smtp = self.smtp_host.text().trim().to_string();
        account.smtp_host = if smtp.is_empty() {
            account.imap_host.clone()
        } else {
            smtp
        };
        account.smtp_port = self.smtp_port.text().trim().parse().unwrap_or(465);
        account.smtp_security = security_from_index(self.smtp_security.selected());
        account.username = if self.username.text().is_empty() {
            email
        } else {
            self.username.text().trim().to_string()
        };
        account
    }

    /// Like `build_account`, but leaves SMTP blank when the field is blank so
    /// the connection test does not probe the IMAP host as an SMTP server.
    fn verify_account(&self) -> Account {
        let mut account = self.build_account();
        if self.smtp_host.text().trim().is_empty() {
            account.smtp_host = String::new();
        }
        account
    }

    /// The credential to test with: the typed password, or the stored one when
    /// editing and left blank.
    fn credential(&self) -> std::result::Result<Credential, String> {
        let password = self.password.text().to_string();
        if !password.is_empty() {
            return Ok(Credential::Password(password));
        }
        if let Some(existing) = &self.existing {
            if let Some(context) = crate::context::get() {
                if let Ok(Some(stored)) = jmail_core::secrets::load_password(&context.db, existing.id)
                {
                    return Ok(Credential::Password(stored));
                }
            }
        }
        Err("Enter the account password".to_string())
    }
}

/// Persist a verified account. Returns `false` only when the account itself
/// could not be written.
fn persist_account(form: &AccountForm, account: &Account, entered_password: &str) -> bool {
    let Some(context) = crate::context::get() else {
        return false;
    };
    let db = &context.db;
    let id = match &form.existing {
        Some(_) => {
            if let Err(error) = db.update_account_config(account) {
                tracing::error!(%error, "could not update account");
                return false;
            }
            if !entered_password.is_empty() {
                if let Err(error) =
                    jmail_core::secrets::store_password(db, account.id, entered_password)
                {
                    tracing::warn!(%error, "could not store password");
                }
            }
            context.engine.stop_account(account.id);
            account.id
        }
        None => match db.insert_account(account) {
            Ok(id) => {
                if let Err(error) = jmail_core::secrets::store_password(db, id, entered_password) {
                    tracing::warn!(%error, "could not store password");
                }
                id
            }
            Err(error) => {
                tracing::error!(%error, "could not add account");
                return false;
            }
        },
    };
    let engine = context.engine.clone();
    context
        .runtime
        .spawn(async move {
            let _ = engine.start_account(id).await;
        });
    context.engine.notify_accounts_changed();
    true
}

/// Verify an account on the tokio runtime, offering to accept an untrusted
/// certificate when that is the only thing standing in the way.
///
/// Calls `on_result(Ok(accept_invalid))` on the main thread, where
/// `accept_invalid` says whether the user accepted an unverified certificate.
fn verify_account_async(
    parent: Option<gtk::Window>,
    mut account: Account,
    credential: Credential,
    accepted_invalid: Rc<std::cell::Cell<bool>>,
    on_result: Rc<dyn Fn(std::result::Result<bool, String>)>,
) {
    let Some(context) = crate::context::get() else {
        on_result(Err("application not ready".to_string()));
        return;
    };
    let already_accepted = accepted_invalid.get();
    if already_accepted {
        account.accept_invalid_certs = true;
    }
    let email = account.email.clone();

    let (tx, rx) = async_channel::bounded(1);
    context.runtime.spawn(async move {
        let result = if already_accepted {
            jmail_core::verify::account(&account, &credential)
                .await
                .map(|()| true)
                .map_err(|error| error.to_string())
        } else {
            match jmail_core::verify::account(&account, &credential).await {
                Ok(()) => Ok(false),
                Err(secure_error) => {
                    let mut insecure = account.clone();
                    insecure.accept_invalid_certs = true;
                    match jmail_core::verify::account(&insecure, &credential).await {
                        Ok(()) => Ok(true),
                        Err(_) => Err(secure_error.to_string()),
                    }
                }
            }
        };
        let _ = tx.send(result).await;
    });

    let accepted_for_confirm = accepted_invalid.clone();
    let on_result_for_confirm = on_result.clone();
    gtk::glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(false)) => on_result(Ok(false)),
            Ok(Ok(true)) if already_accepted => on_result(Ok(true)),
            Ok(Ok(true)) => {
                let dialog = adw::AlertDialog::new(
                    Some("Unverified certificate"),
                    Some(&format!(
                        "The server for {email} presented a certificate that could not be \
                         verified. Connect anyway and accept it for this account?"
                    )),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("accept", "Accept");
                dialog.set_response_appearance("accept", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                dialog.choose(
                    parent.as_ref(),
                    None::<&gtk::gio::Cancellable>,
                    move |response| {
                        if response.as_str() == "accept" {
                            accepted_for_confirm.set(true);
                            on_result_for_confirm(Ok(true));
                        } else {
                            on_result_for_confirm(Err("Certificate not accepted".to_string()));
                        }
                    },
                );
            }
            Ok(Err(message)) => on_result(Err(message)),
            Err(_) => on_result(Err("verification cancelled".to_string())),
        }
    });
}

fn security_index(security: Security) -> u32 {
    match security {
        Security::Tls => 0,
        Security::StartTls => 1,
        Security::None => 2,
    }
}

fn security_from_index(index: u32) -> Security {
    match index {
        1 => Security::StartTls,
        2 => Security::None,
        _ => Security::Tls,
    }
}

fn set_status(label: &gtk::Label, text: &str, error: bool) {
    label.set_text(text);
    label.set_visible(!text.is_empty());
    if error {
        label.remove_css_class("result-success");
        label.add_css_class("result-error");
    } else {
        label.remove_css_class("result-error");
        label.add_css_class("result-success");
    }
}

fn set_busy(
    test: &gtk::Button,
    save: &gtk::Button,
    spinner: &gtk::Spinner,
    status: &gtk::Label,
    text: &str,
) {
    test.set_sensitive(false);
    save.set_sensitive(false);
    spinner.set_visible(true);
    spinner.start();
    set_status(status, text, false);
}

fn set_idle(test: &gtk::Button, save: &gtk::Button, spinner: &gtk::Spinner) {
    test.set_sensitive(true);
    save.set_sensitive(true);
    spinner.stop();
    spinner.set_visible(false);
}

fn notifications_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Notifications")
        .icon_name("preferences-system-notifications-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Desktop Notifications")
        .build();

    let db = crate::context::get().map(|context| context.db.clone());

    let enable = adw::SwitchRow::builder()
        .title("Enable notifications")
        .subtitle("Show desktop notifications for new mail (off by default)")
        .build();
    enable.set_active(
        db.as_ref()
            .map(|db| crate::prefs::get_bool(db, crate::prefs::NOTIFICATIONS_ENABLED, false))
            .unwrap_or(false),
    );
    if let Some(db) = db.clone() {
        enable.connect_active_notify(move |row| {
            crate::prefs::set_bool(&db, crate::prefs::NOTIFICATIONS_ENABLED, row.is_active());
        });
    }
    group.add(&enable);

    let inbox_only = adw::SwitchRow::builder()
        .title("Inbox only")
        .subtitle("Only notify for new messages that land in the Inbox")
        .build();
    inbox_only.set_active(
        db.as_ref()
            .map(|db| crate::prefs::get_bool(db, crate::prefs::NOTIFICATIONS_INBOX_ONLY, true))
            .unwrap_or(true),
    );
    if let Some(db) = db.clone() {
        inbox_only.connect_active_notify(move |row| {
            crate::prefs::set_bool(&db, crate::prefs::NOTIFICATIONS_INBOX_ONLY, row.is_active());
        });
    }
    group.add(&inbox_only);

    let grouping = adw::SwitchRow::builder()
        .title("Group by account")
        .subtitle("Replace an account's notification instead of stacking them")
        .build();
    grouping.set_active(
        db.as_ref()
            .map(|db| crate::prefs::get_bool(db, crate::prefs::NOTIFICATIONS_GROUP, true))
            .unwrap_or(true),
    );
    if let Some(db) = db.clone() {
        grouping.connect_active_notify(move |row| {
            crate::prefs::set_bool(&db, crate::prefs::NOTIFICATIONS_GROUP, row.is_active());
        });
    }
    group.add(&grouping);

    page.add(&group);
    page
}

fn reading_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Reading")
        .icon_name("accessories-text-editor-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder().title("Messages").build();

    let db = crate::context::get().map(|context| context.db.clone());

    let remote = adw::SwitchRow::builder()
        .title("Load remote content")
        .subtitle("Show images and other remote content — may let senders track when you read them")
        .build();
    remote.set_active(
        db.as_ref()
            .map(|db| crate::prefs::get_bool(db, crate::prefs::REMOTE_IMAGES, true))
            .unwrap_or(true),
    );
    if let Some(db) = db.clone() {
        remote.connect_active_notify(move |row| {
            crate::prefs::set_bool(&db, crate::prefs::REMOTE_IMAGES, row.is_active());
            // Re-render the open conversation so the change is visible at once.
            crate::context::refresh_reading();
        });
    }
    group.add(&remote);

    let readable = adw::SwitchRow::builder()
        .title("Force readable message colors")
        .subtitle("Ignore colours chosen by the sender so text always contrasts with the theme")
        .build();
    readable.set_active(
        db.as_ref()
            .map(|db| crate::prefs::get_bool(db, crate::prefs::READABLE_COLORS, true))
            .unwrap_or(true),
    );
    if let Some(db) = db.clone() {
        readable.connect_active_notify(move |row| {
            crate::prefs::set_bool(&db, crate::prefs::READABLE_COLORS, row.is_active());
            // Re-render the open conversation so the change is visible at once.
            crate::context::refresh_reading();
        });
    }
    group.add(&readable);

    page.add(&group);
    page
}

fn sync_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Synchronisation")
        .icon_name("emblem-synchronizing-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder().title("Sync").build();
    let db = crate::context::get().map(|context| context.db.clone());

    let seconds = db
        .as_ref()
        .map(|db| crate::prefs::get_u64(db, crate::prefs::POLL_INTERVAL, 60))
        .unwrap_or(60);
    let adjustment = gtk::Adjustment::new(seconds as f64, 10.0, 3600.0, 10.0, 60.0, 0.0);
    let poll = adw::SpinRow::new(Some(&adjustment), 10.0, 0);
    poll.set_title("Fallback poll interval");
    poll.set_subtitle("Seconds between Inbox checks, applied on next launch");
    if let Some(db) = db.clone() {
        poll.connect_value_notify(move |row| {
            crate::prefs::set_u64(&db, crate::prefs::POLL_INTERVAL, row.value() as u64);
        });
    }
    group.add(&poll);

    page.add(&group);
    page
}
