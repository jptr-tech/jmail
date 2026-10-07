use std::cell::Cell;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib::Propagation;
use gtk::prelude::*;

use crate::window::Ui;

/// Install the Gmail-compatible keyboard layer on the main window.
///
/// A single capture-phase key controller keeps the mapping in one place and
/// lets us implement `g`-prefixed sequences and focus-aware suppression.
pub fn install(ui: &Ui) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let window = ui.window.clone();
    let pending_g = Rc::new(Cell::new(false));

    controller.connect_key_pressed(move |_, key, _, state| {
        // Never steal keys while the user is typing in a text field.
        if let Some(focus) = gtk::prelude::GtkWindowExt::focus(&window) {
            let in_editor = focus.is::<gtk::TextView>();
            if focus.is::<gtk::Editable>() || in_editor {
                pending_g.set(false);
                if key == gdk::Key::Escape && in_editor {
                    activate(&window, "win.discard-reply");
                    return Propagation::Stop;
                }
                return Propagation::Proceed;
            }
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);

        // `g`-prefixed jumps: g i, g t, g d, g a.
        if pending_g.replace(false) {
            let action = match key {
                gdk::Key::i => Some("win.go-inbox"),
                gdk::Key::t => Some("win.go-sent"),
                gdk::Key::d => Some("win.go-drafts"),
                gdk::Key::a => Some("win.go-all"),
                _ => None,
            };
            if let Some(action) = action {
                activate(&window, action);
                return Propagation::Stop;
            }
        }

        if key == gdk::Key::g && !ctrl {
            pending_g.set(true);
            return Propagation::Stop;
        }

        // Shift+, produces `<` on most layouts.
        if key == gdk::Key::less || (shift && key == gdk::Key::comma) {
            activate(&window, "app.settings");
            return Propagation::Stop;
        }

        let action: Option<&str> = if ctrl && key == gdk::Key::Return {
            Some("win.send")
        } else if shift && key == gdk::Key::I {
            Some("win.mark-read")
        } else if shift && key == gdk::Key::U {
            Some("win.mark-unread")
        } else if key == gdk::Key::numbersign {
            Some("win.delete")
        } else {
            match key {
                gdk::Key::j | gdk::Key::Down => Some("win.next-message"),
                gdk::Key::k | gdk::Key::Up => Some("win.prev-message"),
                gdk::Key::o | gdk::Key::Return => Some("win.open-window"),
                gdk::Key::u | gdk::Key::Escape => Some("win.back"),
                gdk::Key::c if !ctrl => Some("win.compose"),
                gdk::Key::r => Some("win.reply"),
                gdk::Key::a => Some("win.reply-all"),
                gdk::Key::f => Some("win.forward"),
                gdk::Key::e => Some("win.archive"),
                gdk::Key::s => Some("win.star"),
                gdk::Key::x => Some("win.select"),
                gdk::Key::v => Some("win.move"),
                gdk::Key::l => Some("win.label"),
                gdk::Key::slash => Some("win.search"),
                gdk::Key::question => Some("win.help"),
                _ => None,
            }
        };

        match action {
            Some(action) => {
                activate(&window, action);
                Propagation::Stop
            }
            None => Propagation::Proceed,
        }
    });

    ui.window.add_controller(controller);
}

/// Activate a prefixed action, logging if it does not exist.
fn activate(window: &adw::ApplicationWindow, name: &str) {
    if let Err(error) = gtk::prelude::WidgetExt::activate_action(window, name, None) {
        tracing::warn!(action = name, %error, "failed to activate action");
    }
}
