use adw::prelude::*;
use gtk::Orientation;

use jmail_core::{Account, Security};

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
        .description("Connect Gmail with OAuth, or any IMAP/SMTP server with a password.")
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
                show_add_account_dialog(parent.as_ref(), accounts.clone());
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
                .subtitle(format!(
                    "{}:{} · {:?}",
                    account.imap_host, account.imap_port, account.imap_security
                ))
                .build();
            let remove = gtk::Button::with_label("Remove");
            remove.set_valign(gtk::Align::Center);
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
                    accounts.refresh();
                });
            }
            row.add_suffix(&remove);
            self.push(row.upcast_ref());
        }
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

fn show_add_account_dialog(parent: Option<&gtk::Window>, accounts: AccountsGroup) {
    let name = entry("Name");
    let email = entry("you@example.com");
    let host = entry("imap.example.com");
    let port = entry("993");
    let username = entry("Username (defaults to email)");
    let password = gtk::PasswordEntry::new();
    password.set_placeholder_text(Some("Password"));
    password.set_show_peek_icon(true);

    let security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
    security.set_selected(0);

    let smtp_host = entry("smtp.example.com");
    let smtp_port = entry("465");
    let smtp_security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
    smtp_security.set_selected(0);

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

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Add Account");
    save.add_css_class("suggested-action");
    let actions = gtk::Box::new(Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(8);
    actions.set_margin_bottom(12);
    actions.set_margin_start(12);
    actions.set_margin_end(12);
    actions.append(&cancel);
    actions.append(&save);

    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&form);
    content.append(&gtk::Separator::new(Orientation::Horizontal));
    content.append(&actions);

    let dialog = adw::Dialog::builder()
        .title("Add Account")
        .content_width(440)
        .child(&content)
        .build();

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }

    {
        let dialog = dialog.clone();
        save.connect_clicked(move |_| {
            let Some(context) = crate::context::get() else {
                return;
            };
            let email_text = email.text().to_string();
            let mut account = Account::generic(email_text.clone());
            account.name = if name.text().is_empty() {
                email_text.clone()
            } else {
                name.text().to_string()
            };
            account.email = email_text.clone();
            account.imap_host = host.text().to_string();
            account.imap_port = port.text().parse().unwrap_or(993);
            account.imap_security = match security.selected() {
                1 => Security::StartTls,
                2 => Security::None,
                _ => Security::Tls,
            };
            account.smtp_host = if smtp_host.text().is_empty() {
                host.text().to_string()
            } else {
                smtp_host.text().to_string()
            };
            account.smtp_port = smtp_port.text().parse().unwrap_or(465);
            account.smtp_security = match smtp_security.selected() {
                1 => Security::StartTls,
                2 => Security::None,
                _ => Security::Tls,
            };
            account.username = if username.text().is_empty() {
                email_text
            } else {
                username.text().to_string()
            };

            match context.db.insert_account(&account) {
                Ok(id) => {
                    if let Err(error) = jmail_core::secrets::store_password(
                        &context.db,
                        id,
                        password.text().as_str(),
                    ) {
                        tracing::error!(%error, "could not store password");
                    }
                    let engine = context.engine.clone();
                    context.runtime.spawn(async move {
                        let _ = engine.start_account(id).await;
                    });
                    accounts.refresh();
                    dialog.close();
                }
                Err(error) => tracing::error!(%error, "could not add account"),
            }
        });
    }

    dialog.present(parent);
}

fn notifications_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Notifications")
        .icon_name("preferences-system-notifications-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Desktop Notifications")
        .build();

    let enable = adw::SwitchRow::builder()
        .title("Enable notifications")
        .subtitle("Show desktop notifications for new mail (off by default)")
        .build();
    enable.set_active(false);
    group.add(&enable);

    let inbox_only = adw::SwitchRow::builder()
        .title("Inbox only")
        .subtitle("Only notify for new messages that land in the Inbox")
        .build();
    inbox_only.set_active(true);
    inbox_only.set_sensitive(false);
    group.add(&inbox_only);

    let grouping = adw::SwitchRow::builder()
        .title("Group by account")
        .subtitle("Replace an account's notification instead of stacking them")
        .build();
    grouping.set_active(true);
    grouping.set_sensitive(false);
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

    let conversation = adw::SwitchRow::builder()
        .title("Conversation view")
        .subtitle("Show a Gmail-style back-and-forth with the newest message at the bottom")
        .build();
    conversation.set_active(true);
    group.add(&conversation);

    let remote = adw::SwitchRow::builder()
        .title("Load remote images")
        .subtitle("Allow tracking pixels to load automatically")
        .build();
    remote.set_active(false);
    group.add(&remote);

    page.add(&group);
    page
}

fn sync_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Synchronisation")
        .icon_name("emblem-synchronizing-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder().title("Sync").build();

    let idle = adw::SwitchRow::builder()
        .title("Use IMAP IDLE")
        .subtitle("Ask the server to push changes when supported")
        .build();
    idle.set_active(true);
    group.add(&idle);

    let poll = adw::ActionRow::builder()
        .title("Fallback poll interval")
        .subtitle("60 seconds")
        .build();
    group.add(&poll);

    page.add(&group);
    page
}
