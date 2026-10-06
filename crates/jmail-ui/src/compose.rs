use adw::prelude::*;
use gtk::{Align, Orientation};

/// Which kind of message the compose window was opened for.
#[derive(Clone, Copy)]
pub enum Mode {
    New,
    Reply,
    ReplyAll,
    Forward,
}

impl Mode {
    fn title(self) -> &'static str {
        match self {
            Self::New => "New Message",
            Self::Reply => "Reply",
            Self::ReplyAll => "Reply All",
            Self::Forward => "Forward",
        }
    }

    fn subject_prefix(self) -> Option<&'static str> {
        match self {
            Self::New => None,
            Self::Reply | Self::ReplyAll => Some("Re: "),
            Self::Forward => Some("Fwd: "),
        }
    }
}

/// Show the compose dialog. The send button is a stub until SMTP lands in M4.
pub fn present(parent: Option<&impl IsA<gtk::Widget>>, mode: Mode, subject: Option<String>) {
    let to = gtk::Entry::builder().placeholder_text("To").build();
    let subject_entry = gtk::Entry::builder().placeholder_text("Subject").build();
    if let Some(subject) = subject {
        let prefix = mode.subject_prefix().unwrap_or("");
        subject_entry.set_text(&format!("{prefix}{subject}"));
    }

    let body = gtk::TextView::new();
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    body.set_top_margin(8);
    body.set_bottom_margin(8);
    body.set_left_margin(8);
    body.set_right_margin(8);
    let body_scrolled = gtk::ScrolledWindow::builder()
        .child(&body)
        .vexpand(true)
        .build();

    let form = gtk::Box::new(Orientation::Vertical, 0);
    form.append(&to);
    form.append(&gtk::Separator::new(Orientation::Horizontal));
    form.append(&subject_entry);
    form.append(&gtk::Separator::new(Orientation::Horizontal));
    form.append(&body_scrolled);

    let cancel = gtk::Button::with_label("Cancel");
    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested-action");

    let actions = gtk::Box::new(Orientation::Horizontal, 6);
    actions.set_halign(Align::End);
    actions.set_margin_top(8);
    actions.set_margin_bottom(8);
    actions.set_margin_start(12);
    actions.set_margin_end(12);
    actions.append(&cancel);
    actions.append(&send);

    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&form);
    content.append(&gtk::Separator::new(Orientation::Horizontal));
    content.append(&actions);

    let dialog = adw::Dialog::builder()
        .title(mode.title())
        .content_width(640)
        .content_height(520)
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
        send.connect_clicked(move |_| {
            tracing::info!("send requested (stub)");
            dialog.close();
        });
    }

    dialog.present(parent);
}
