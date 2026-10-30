//! A recipient entry with inline suggestions.
//!
//! GTK4 has no entry completion, and a native popover inside the composer's
//! Adwaita dialog keeps stealing keyboard focus. Instead this wraps a
//! [`gtk::Entry`] and a temporary list directly below it. The list is never
//! focusable, so typing stays in the entry.
//!
//! Suggestions come from the shared [`jmail_core::ContactIndex`] on the main
//! window, which is refreshed in the background (Sent mail) and on composer
//! open (GNOME contacts).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{Align, Orientation};

use jmail_core::{
    accept_suggestion, insertable_address, parse_address_list, recipient_token, Address,
};

use crate::window::Ui;

/// An address field (To/Cc/Bcc) with recipient autocomplete.
pub struct AddressEntry {
    pub entry: gtk::Entry,
    /// The floating suggestion list; the composer adds this to its `GtkOverlay`.
    pub suggestions_widget: gtk::ScrolledWindow,
    _inner: Rc<Inner>,
}

struct Inner {
    ui: Ui,
    entry: gtk::Entry,
    list: gtk::ListBox,
    list_frame: gtk::ScrolledWindow,
    suggestions: RefCell<Vec<Address>>,
    /// Overlay that draws the floating list; set by the composer.
    overlay: RefCell<Option<gtk::Overlay>>,
    /// Index of the highlighted suggestion, independent of GTK's selection
    /// state (which can be cleared while the list is hidden/rebuilt).
    selected: Cell<usize>,
    /// Set while accepting a suggestion, so `set_text` does not immediately
    /// reopen the list for the address we just inserted.
    quiet: Cell<bool>,
    open: Cell<bool>,
    /// Window-level handlers, installed when the entry is mapped and removed
    /// when this entry is dropped.
    focus_handler: RefCell<Option<(gtk::Window, gtk::glib::SignalHandlerId)>>,
    key_controller: RefCell<Option<(gtk::Window, gtk::EventControllerKey)>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some((window, id)) = self.focus_handler.borrow_mut().take() {
            window.disconnect(id);
        }
        if let Some((window, controller)) = self.key_controller.borrow_mut().take() {
            window.remove_controller(&controller);
        }
    }
}

impl AddressEntry {
    /// Replace the text without offering suggestions (used for reply/draft
    /// recipients that are filled in when the composer opens).
    pub fn set_text(&self, text: &str) {
        self._inner.quiet.set(true);
        self.entry.set_text(text);
        self._inner.quiet.set(false);
    }

    pub fn new(ui: &Ui, placeholder: &str) -> Self {
        let entry = gtk::Entry::builder()
            .placeholder_text(placeholder)
            .hexpand(true)
            .build();

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.set_can_focus(false);
        list.set_focus_on_click(false);

        let list_frame = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(320)
            .build();
        list_frame.set_can_focus(false);
        list_frame.set_focus_on_click(false);
        list_frame.set_visible(false);
        list_frame.set_halign(Align::Start);
        list_frame.set_valign(Align::Start);
        list_frame.set_hexpand(false);
        list_frame.set_vexpand(false);
        list_frame.add_css_class("address-suggestions");

        let inner = Rc::new(Inner {
            ui: ui.clone(),
            entry: entry.clone(),
            list: list.clone(),
            list_frame: list_frame.clone(),
            suggestions: RefCell::new(Vec::new()),
            overlay: RefCell::new(None),
            selected: Cell::new(0),
            quiet: Cell::new(false),
            open: Cell::new(false),
            focus_handler: RefCell::new(None),
            key_controller: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&inner);
            entry.connect_changed(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.refresh();
                }
            });
        }
        {
            let weak = Rc::downgrade(&inner);
            list.connect_row_activated(move |_, row| {
                if let Some(inner) = weak.upgrade() {
                    let index = row.index() as usize;
                    inner.selected.set(index);
                    inner.accept(index);
                }
            });
        }
        {
            // `GtkEntry`'s internal text is the focus widget, so a window-level
            // key controller can miss Return in some dialog surfaces. The
            // entry's own `activate` signal is the reliable Enter path.
            let weak = Rc::downgrade(&inner);
            entry.connect_activate(move |_| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                tracing::debug!(
                    open = inner.open.get(),
                    "address completion: entry activate"
                );
                if inner.open.get() {
                    inner.accept_current();
                }
            });
        }
        {
            // Key handling and focus tracking live on the toplevel window: a
            // GtkEntry's focus is on its internal GtkText child, so a key
            // controller on the entry can miss keys the entry appears to own.
            let weak = Rc::downgrade(&inner);
            entry.connect_map(move |entry| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.install_window_handlers(entry);
            });
        }

        Self {
            entry,
            suggestions_widget: list_frame,
            _inner: inner,
        }
    }

    /// Add the floating suggestion list to the composer's overlay.
    pub fn attach_overlay(&self, overlay: &gtk::Overlay) {
        overlay.add_overlay(&self.suggestions_widget);
        overlay.set_measure_overlay(&self.suggestions_widget, false);
        overlay.set_clip_overlay(&self.suggestions_widget, false);
        self._inner.overlay.replace(Some(overlay.clone()));
        self._inner.reposition();
    }
}

impl Inner {
    fn install_window_handlers(self: &Rc<Self>, entry: &gtk::Entry) {
        if self.focus_handler.borrow().is_some() {
            return;
        }
        let Some(root) = entry.root() else {
            return;
        };
        let Ok(window) = root.downcast::<gtk::Window>() else {
            return;
        };

        {
            let weak = Rc::downgrade(self);
            let id = window.connect_focus_widget_notify(move |window| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let focused = gtk::prelude::GtkWindowExt::focus(window);
                let inside = focused.as_ref().is_some_and(|widget| {
                    widget == &inner.entry.clone().upcast::<gtk::Widget>()
                        || widget.is_ancestor(&inner.entry)
                });
                if !inside {
                    inner.hide();
                }
            });
            *self.focus_handler.borrow_mut() = Some((window.clone(), id));
        }

        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        controller.connect_key_pressed(move |_, key, _, _| {
            let Some(inner) = weak.upgrade() else {
                return gtk::glib::Propagation::Proceed;
            };
            if !inner.open.get() {
                return gtk::glib::Propagation::Proceed;
            }
            let Some(root) = inner.entry.root() else {
                return gtk::glib::Propagation::Proceed;
            };
            let Ok(window) = root.downcast::<gtk::Window>() else {
                return gtk::glib::Propagation::Proceed;
            };
            let inside = gtk::prelude::GtkWindowExt::focus(&window).is_some_and(|widget| {
                widget == inner.entry.clone().upcast::<gtk::Widget>()
                    || widget.is_ancestor(&inner.entry)
            });
            if inside {
                inner.key_pressed(key)
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        window.add_controller(controller.clone());
        *self.key_controller.borrow_mut() = Some((window, controller));
    }

    fn refresh(&self) {
        if self.quiet.get() {
            tracing::debug!("address completion: suppressed while setting text");
            return;
        }
        if !self.entry.is_mapped() {
            tracing::debug!("address completion: entry is not mapped");
            return;
        }
        let text = self.entry.text();
        let caret = self.entry.position().max(0) as usize;
        let token = recipient_token(&text, caret);
        // Once the user is inside `Name <...>`, complete the address only.
        let query = token
            .text
            .rsplit('<')
            .next()
            .unwrap_or("")
            .trim_end_matches('>')
            .trim();
        tracing::debug!(
            text_chars = text.chars().count(),
            caret,
            query_chars = query.chars().count(),
            "address completion: refresh"
        );
        if query.is_empty() {
            self.hide();
            return;
        }

        // Refresh the Sent side at most every couple of minutes (debounced in
        // `Ui`), so mail sent from another device shows up without a restart.
        self.ui.ensure_contacts_fresh();

        let exclude: Vec<String> = parse_address_list(&text)
            .into_iter()
            .map(|address| address.email)
            .collect();
        let suggestions = self.ui.contacts.borrow().suggest(query, &exclude, 6);
        tracing::debug!(
            count = suggestions.len(),
            "address completion: suggestions"
        );
        if suggestions.is_empty() {
            self.hide();
            return;
        }
        self.fill(suggestions);
        self.show();
    }

    fn fill(&self, suggestions: Vec<Address>) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        self.suggestions.replace(suggestions);

        for address in self.suggestions.borrow().iter() {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);
            row.set_can_focus(false);
            row.set_focus_on_click(false);
            row.set_tooltip_text(Some(&insertable_address(address)));

            let content = gtk::Box::new(Orientation::Vertical, 1);
            content.set_margin_top(6);
            content.set_margin_bottom(6);
            content.set_margin_start(8);
            content.set_margin_end(8);

            let name = if address.name.trim().is_empty() {
                address.email.clone()
            } else {
                address.name.clone()
            };
            let primary = gtk::Label::builder()
                .label(name)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            primary.set_halign(Align::Start);
            primary.add_css_class("suggestion-name");
            content.append(&primary);

            if !address.name.trim().is_empty() {
                let secondary = gtk::Label::builder()
                    .label(address.email.clone())
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                secondary.set_halign(Align::Start);
                secondary.add_css_class("dim-label");
                secondary.add_css_class("caption");
                content.append(&secondary);
            }

            row.set_child(Some(&content));
            self.list.append(&row);
        }
        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }
        self.selected.set(0);
    }

    fn accept(&self, index: usize) {
        let Some(address) = self.suggestions.borrow().get(index).cloned() else {
            return;
        };
        let value = insertable_address(&address);
        let (text, caret) = accept_suggestion(
            &self.entry.text(),
            self.entry.position().max(0) as usize,
            &value,
        );
        self.quiet.set(true);
        self.entry.set_text(&text);
        self.entry.set_position(caret as i32);
        self.quiet.set(false);
        self.hide();
        self.entry.grab_focus();
    }

    fn key_pressed(&self, key: gtk::gdk::Key) -> gtk::glib::Propagation {
        match key {
            gtk::gdk::Key::Escape => {
                self.hide();
                gtk::glib::Propagation::Stop
            }
            gtk::gdk::Key::Up | gtk::gdk::Key::KP_Up => {
                self.move_selection(-1);
                gtk::glib::Propagation::Stop
            }
            gtk::gdk::Key::Down | gtk::gdk::Key::KP_Down => {
                self.move_selection(1);
                gtk::glib::Propagation::Stop
            }
            gtk::gdk::Key::Tab => {
                // While suggestions are open, Tab walks the list instead of
                // leaving the field. Escape first to move to the next field.
                self.move_selection(1);
                gtk::glib::Propagation::Stop
            }
            gtk::gdk::Key::ISO_Left_Tab => {
                self.move_selection(-1);
                gtk::glib::Propagation::Stop
            }
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                tracing::debug!("address completion: Return key");
                if self.accept_current() {
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            }
            _ => gtk::glib::Propagation::Proceed,
        }
    }

    /// Insert the highlighted suggestion. Returns false when the list is
    /// empty, so Enter can fall through to the normal entry action.
    fn accept_current(&self) -> bool {
        let count = self.suggestions.borrow().len();
        tracing::debug!(
            selected = self.selected.get(),
            count,
            "address completion: accept current"
        );
        if count == 0 {
            return false;
        }
        self.accept(self.selected.get());
        true
    }

    fn move_selection(&self, delta: i32) {
        let count = self.suggestions.borrow().len();
        if count == 0 {
            return;
        }
        let current = self.selected.get().min(count - 1) as i32;
        let next = (current + delta).clamp(0, count as i32 - 1) as usize;
        self.selected.set(next);
        if let Some(row) = self.list.row_at_index(next as i32) {
            self.list.select_row(Some(&row));
        }
    }

    fn reposition(&self) {
        let Some(overlay) = self.overlay.borrow().clone() else {
            return;
        };
        let Some(bounds) = self.entry.compute_bounds(&overlay) else {
            return;
        };
        let x = bounds.x().round().max(0.0) as i32;
        let y = (bounds.y() + bounds.height()).round().max(0.0) as i32 + 4;
        let width = bounds.width().round().max(180.0) as i32;
        self.list_frame.set_margin_start(x);
        self.list_frame.set_margin_top(y);
        self.list_frame.set_size_request(width, -1);
    }

    fn show(&self) {
        self.reposition();
        if self.open.get() {
            return;
        }
        self.open.set(true);
        self.list_frame.set_visible(true);
        tracing::debug!("address completion: inline list shown");
    }

    fn hide(&self) {
        self.open.set(false);
        self.list_frame.set_visible(false);
    }
}
