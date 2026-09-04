//! Native window chrome shared by the app and its visual regression contract.

use gpui::{point, px, TitlebarOptions};

/// Match Codex Desktop's macOS full-size content window: AppKit owns the
/// traffic lights, while the app content extends beneath a transparent,
/// untitled titlebar.
pub fn native_titlebar_options() -> TitlebarOptions {
    TitlebarOptions {
        title: None,
        appears_transparent: true,
        traffic_light_position: Some(point(px(16.0), px(16.0))),
    }
}
