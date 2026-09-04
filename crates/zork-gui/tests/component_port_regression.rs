use std::ops::Range;

use zork_gui::components::{
    selector_menu::{SelectorKind, SelectorMenuState},
    text_input::{EditOutcome, TextBuffer},
};

#[test]
fn backward_and_forward_delete_whole_grapheme_clusters() {
    let mut backward = TextBuffer::new("A👨‍👩‍👧‍👦e\u{301}中");
    backward.set_selection(backward.value().len()..backward.value().len());

    assert_eq!(backward.delete_backward(), EditOutcome::Changed);
    assert_eq!(backward.value(), "A👨‍👩‍👧‍👦e\u{301}");
    assert_eq!(backward.delete_backward(), EditOutcome::Changed);
    assert_eq!(backward.value(), "A👨‍👩‍👧‍👦");
    assert_eq!(backward.delete_backward(), EditOutcome::Changed);
    assert_eq!(backward.value(), "A");

    let mut forward = TextBuffer::new("A👨‍👩‍👧‍👦B");
    forward.set_selection(1..1);
    assert_eq!(forward.delete_forward(), EditOutcome::Changed);
    assert_eq!(forward.value(), "AB");
    assert_eq!(forward.selection(), 1..1);
}

#[test]
fn utf16_ranges_round_trip_and_ime_replacement_preserves_unicode() {
    let mut buffer = TextBuffer::new("a😀中");
    buffer.set_selection(1..5);
    assert_eq!(buffer.selection_utf16(), 1..3);

    buffer.set_selection_utf16(3..4);
    assert_eq!(buffer.selection(), 5..8);

    assert_eq!(
        buffer.replace_utf16_range(Some(Range { start: 1, end: 3 }), "文"),
        EditOutcome::Changed
    );
    assert_eq!(buffer.value(), "a文中");
    assert_eq!(buffer.selection(), 4..4);
    assert_eq!(buffer.selection_utf16(), 2..2);
}

#[test]
fn marked_text_tracks_utf16_selection_relative_to_the_insert() {
    let mut buffer = TextBuffer::new("ab");
    buffer.set_selection(1..2);

    assert_eq!(
        buffer.replace_and_mark_utf16_range(None, "😀文", Some(2..3)),
        EditOutcome::Changed
    );
    assert_eq!(buffer.value(), "a😀文");
    assert_eq!(buffer.marked_range_utf16(), Some(1..4));
    assert_eq!(buffer.selection(), 5..8);
    assert_eq!(buffer.selection_utf16(), 3..4);

    buffer.unmark();
    assert_eq!(buffer.marked_range_utf16(), None);
}

#[test]
fn enter_submits_while_shift_enter_inserts_a_newline() {
    let mut buffer = TextBuffer::new("first line");
    buffer.set_selection(buffer.value().len()..buffer.value().len());

    assert_eq!(buffer.handle_enter(false), EditOutcome::Submit);
    assert_eq!(buffer.value(), "first line");

    assert_eq!(buffer.handle_enter(true), EditOutcome::Changed);
    assert_eq!(buffer.value(), "first line\n");
}

#[test]
fn selector_menu_opens_one_kind_and_returns_the_exact_choice() {
    let mut menu = SelectorMenuState::default();
    assert_eq!(menu.open(), None);

    menu.toggle(SelectorKind::Profile);
    assert_eq!(menu.open(), Some(SelectorKind::Profile));

    menu.toggle(SelectorKind::Model);
    assert_eq!(menu.open(), Some(SelectorKind::Model));
    assert_eq!(menu.choose(SelectorKind::Model, 2, 3), Some(2));
    assert_eq!(menu.open(), None);

    menu.toggle(SelectorKind::Thinking);
    assert_eq!(menu.choose(SelectorKind::Model, 1, 3), None);
    assert_eq!(menu.open(), Some(SelectorKind::Thinking));
    menu.dismiss();
    assert_eq!(menu.open(), None);
}

#[test]
fn selector_menu_keyboard_highlight_wraps_and_is_the_enter_choice() {
    let mut menu = SelectorMenuState::default();
    menu.open_at(SelectorKind::Model, 1);
    assert_eq!(menu.highlighted(), Some(1));

    assert_eq!(menu.move_highlight(1, 3), Some(2));
    assert_eq!(menu.move_highlight(1, 3), Some(0));
    assert_eq!(menu.move_highlight(-1, 3), Some(2));
    assert_eq!(menu.choose_highlighted(SelectorKind::Model, 3), Some(2));
    assert_eq!(menu.open(), None);
}
