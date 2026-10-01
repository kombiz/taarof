//! Child clearing for the three containers used by rebuildable GTK views.

use gtk::prelude::*;

/// Containers whose children can be removed with their own GTK removal API.
///
/// Deliberately implemented only for Box, ListBox and FlowBox, never Widget:
/// unparenting an arbitrary widget bypasses its container's bookkeeping.
pub(crate) trait ChildContainer: IsA<gtk::Widget> {
    fn remove_child(&self, child: &gtk::Widget);
}

impl ChildContainer for gtk::Box {
    fn remove_child(&self, child: &gtk::Widget) {
        self.remove(child);
    }
}

impl ChildContainer for gtk::ListBox {
    fn remove_child(&self, child: &gtk::Widget) {
        self.remove(child);
    }
}

impl ChildContainer for gtk::FlowBox {
    fn remove_child(&self, child: &gtk::Widget) {
        self.remove(child);
    }
}

/// Remove children in their current sibling order, retaining each child until
/// its container's removal finishes. Callers keep responsibility for their
/// surrounding borrows, retained widget references and focus restoration.
pub(crate) fn remove_all_children(container: &impl ChildContainer) {
    while let Some(child) = container.first_child() {
        container.remove_child(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an owned GTK display"]
    fn typed_child_clearing_gtk_lifecycle() {
        let _guard = crate::glib_main_context_test_guard();
        gtk::init().expect("owned GTK display");

        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
        remove_all_children(&box_);
        let first = gtk::Label::new(Some("first"));
        let second = gtk::Label::new(Some("second"));
        let removed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        for (name, label) in [("first", &first), ("second", &second)] {
            let removed = removed.clone();
            label.connect_parent_notify(move |label| {
                if label.parent().is_none() {
                    removed.borrow_mut().push(name);
                }
            });
        }
        box_.append(&first);
        box_.append(&second);
        assert_eq!(box_.first_child().as_ref(), Some(first.upcast_ref()));
        assert_eq!(first.next_sibling().as_ref(), Some(second.upcast_ref()));
        remove_all_children(&box_);
        assert!(box_.first_child().is_none());
        assert!(first.parent().is_none());
        assert!(second.parent().is_none());
        assert_eq!(*removed.borrow(), ["first", "second"]);
        box_.append(&second);
        box_.append(&first);
        assert_eq!(box_.first_child().as_ref(), Some(second.upcast_ref()));
        remove_all_children(&box_);
        remove_all_children(&box_);

        let list = gtk::ListBox::new();
        remove_all_children(&list);
        let row = gtk::ListBoxRow::new();
        let label = gtk::Label::new(Some("wrapped"));
        list.append(&row);
        list.append(&label);
        let wrapper = list.row_at_index(1).expect("implicit ListBoxRow");
        list.select_row(Some(&row));
        remove_all_children(&list);
        assert!(list.first_child().is_none());
        assert!(list.selected_row().is_none());
        assert!(row.parent().is_none());
        assert!(wrapper.parent().is_none());
        assert_eq!(label.parent().as_ref(), Some(wrapper.upcast_ref()));
        list.append(&row);
        assert_eq!(list.row_at_index(0), Some(row));
        remove_all_children(&list);
        remove_all_children(&list);

        let flow = gtk::FlowBox::new();
        remove_all_children(&flow);
        let child = gtk::FlowBoxChild::new();
        let label = gtk::Label::new(Some("wrapped"));
        flow.insert(&child, -1);
        flow.insert(&label, -1);
        let wrapper = flow.child_at_index(1).expect("implicit FlowBoxChild");
        flow.select_child(&child);
        remove_all_children(&flow);
        assert!(flow.first_child().is_none());
        assert!(flow.selected_children().is_empty());
        assert!(child.parent().is_none());
        assert!(wrapper.parent().is_none());
        assert_eq!(label.parent().as_ref(), Some(wrapper.upcast_ref()));
        flow.insert(&child, -1);
        assert_eq!(flow.child_at_index(0), Some(child));
        remove_all_children(&flow);
        remove_all_children(&flow);
    }
}
