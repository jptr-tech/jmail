use gtk::prelude::*;

/// Depth-first search for the first descendant carrying `css`.
pub fn find_by_css(root: &gtk::Widget, css: &str) -> Option<gtk::Widget> {
    if root.has_css_class(css) {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(current) = child {
        if let Some(found) = find_by_css(&current, css) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}
