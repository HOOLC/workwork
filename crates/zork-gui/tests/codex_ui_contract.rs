use gpui::{point, px, AssetSource};
use zork_gui::assets::EmbeddedAssets;
use zork_gui::design::codex_ui_spec;
use zork_gui::window_chrome::native_titlebar_options;

#[test]
fn native_window_uses_codex_full_size_transparent_chrome() {
    let titlebar = native_titlebar_options();

    assert!(titlebar.title.is_none());
    assert!(titlebar.appears_transparent);
    assert_eq!(
        titlebar.traffic_light_position,
        Some(point(px(16.0), px(16.0)))
    );
}

#[test]
fn primary_regions_fit_the_default_window_without_overlap() {
    let geometry = codex_ui_spec()
        .layout
        .resolve(1280.0, 800.0)
        .expect("the supported default window must resolve");

    assert_eq!(geometry.sidebar.right, geometry.main.left);
    assert!(geometry.transcript.right <= geometry.main.right);
    assert!(geometry.composer.left >= geometry.main.left);
    assert!(geometry.composer.right <= geometry.main.right);
    assert!(geometry.composer.top > geometry.header.bottom);
    assert!(geometry.transcript.bottom <= geometry.composer.top);
}

#[test]
fn composer_icons_are_real_embedded_library_assets() {
    let assets = EmbeddedAssets;

    for path in [
        "icons/phosphor-arrow-up.svg",
        "icons/phosphor-caret-down.svg",
        "icons/phosphor-brain.svg",
        "icons/phosphor-cube.svg",
        "icons/phosphor-folder-simple.svg",
        "icons/phosphor-stop-fill.svg",
        "icons/phosphor-terminal-window.svg",
    ] {
        assert!(assets.load(path).expect("asset load succeeds").is_some());
    }
}
