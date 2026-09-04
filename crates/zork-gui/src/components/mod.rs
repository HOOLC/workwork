//! Small GPUI components owned by zork-gui.

pub mod message;
pub mod selector_menu;
pub mod text_input;

use gpui::App;

/// Register component-scoped key bindings once during application startup.
pub fn init(cx: &mut App) {
    text_input::init(cx);
}
