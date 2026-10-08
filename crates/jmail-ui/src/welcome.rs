//! First-run onboarding.
//!
//! Jmail signs in through GNOME Online Accounts rather than an in-app OAuth
//! flow. On the first launch we greet the user, adapting to whether accounts
//! are already configured: if they are, we simply welcome them; if not, we
//! point at the *system* Online Accounts settings and offer a manual fallback.

use adw::prelude::*;

use jmail_core::Db;

use crate::window::Ui;

/// Show the welcome dialog unless onboarding has already been completed.
pub fn maybe_show(ui: &Ui) {
    if let Some(has_accounts) = onboarding_state(&ui.db) {
        present(ui, has_accounts);
    }
}

/// Whether to greet on startup and whether accounts are already configured.
/// `None` means onboarding has already been completed.
fn onboarding_state(db: &Db) -> Option<bool> {
    let onboarded = db
        .get_setting("onboarding_done")
        .ok()
        .flatten()
        .as_deref()
        == Some("1");
    if onboarded {
        return None;
    }
    Some(!db.accounts().unwrap_or_default().is_empty())
}

fn present(ui: &Ui, has_accounts: bool) {
    let window = adw::Window::builder()
        .transient_for(&ui.window)
        .modal(true)
        .default_width(440)
        .default_height(480)
        .build();

    // Dismissing the dialog at all counts as having seen it.
    {
        let db = ui.db.clone();
        window.connect_close_request(move |_| {
            let _ = db.set_setting("onboarding_done", "1");
            gtk::glib::Propagation::Proceed
        });
    }

    let header = adw::HeaderBar::new();
    let page = adw::StatusPage::builder()
        .icon_name(crate::APP_ID)
        .title("Welcome to Jmail")
        .build();

    let actions = gtk::Box::new(gtk::Orientation::Vertical, 8);
    actions.set_halign(gtk::Align::Center);

    if has_accounts {
        page.set_description(Some(
            "We found your mail accounts and they're ready to go. \
             Your folders are syncing now.",
        ));
        let start = pill("Get Started", true);
        {
            let window = window.clone();
            start.connect_clicked(move |_| window.close());
        }
        actions.append(&start);
    } else {
        page.set_description(Some(
            "Jmail signs in through GNOME Online Accounts. Add your email in \
             System Settings and it will appear here automatically — or connect \
             an IMAP/SMTP server manually.",
        ));
        let open = pill("Open Online Accounts Settings", true);
        {
            let window = window.clone();
            open.connect_clicked(move |_| {
                open_system_online_accounts();
                window.close();
            });
        }
        actions.append(&open);

        let manual = pill("Set up manually", false);
        {
            let window = window.clone();
            let ui = ui.clone();
            manual.connect_clicked(move |_| {
                let parent = window.clone();
                crate::settings::present_add_account(Some(parent.upcast_ref()), {
                    let window = window.clone();
                    let ui = ui.clone();
                    move || {
                        window.close();
                        ui.refresh_accounts(false);
                    }
                });
            });
        }
        actions.append(&manual);
    }

    page.set_child(Some(&actions));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    window.set_content(Some(&toolbar));
    window.present();
}

fn pill(label: &str, suggested: bool) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("pill");
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

/// Launch the GNOME Settings Online Accounts panel (system settings, not ours).
fn open_system_online_accounts() {
    if let Err(error) = std::process::Command::new("gnome-control-center")
        .arg("online-accounts")
        .spawn()
    {
        tracing::warn!(%error, "could not open Online Accounts settings");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_once_then_stays_quiet() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(onboarding_state(&db), Some(false));
        db.set_setting("onboarding_done", "1").unwrap();
        assert_eq!(onboarding_state(&db), None);
    }

    #[test]
    fn reports_existing_accounts() {
        let db = Db::open_in_memory().unwrap();
        db.insert_account(&jmail_core::Account::generic("you@example.com"))
            .unwrap();
        assert_eq!(onboarding_state(&db), Some(true));
    }
}
