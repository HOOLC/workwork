//! Embedded visual assets used by the native GPUI renderer.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

#[derive(Clone, Copy, Debug, Default)]
pub struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/phosphor-arrow-up.svg" => {
                Some(include_bytes!("../assets/icons/phosphor-arrow-up.svg"))
            }
            "icons/phosphor-caret-down.svg" => {
                Some(include_bytes!("../assets/icons/phosphor-caret-down.svg"))
            }
            "icons/phosphor-brain.svg" => {
                Some(include_bytes!("../assets/icons/phosphor-brain.svg"))
            }
            "icons/phosphor-cube.svg" => Some(include_bytes!("../assets/icons/phosphor-cube.svg")),
            "icons/phosphor-folder-simple.svg" => {
                Some(include_bytes!("../assets/icons/phosphor-folder-simple.svg"))
            }
            "icons/phosphor-stop-fill.svg" => {
                Some(include_bytes!("../assets/icons/phosphor-stop-fill.svg"))
            }
            "icons/phosphor-terminal-window.svg" => Some(include_bytes!(
                "../assets/icons/phosphor-terminal-window.svg"
            )),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path != "icons" {
            return Ok(Vec::new());
        }
        Ok(vec![
            "phosphor-arrow-up.svg".into(),
            "phosphor-brain.svg".into(),
            "phosphor-caret-down.svg".into(),
            "phosphor-cube.svg".into(),
            "phosphor-folder-simple.svg".into(),
            "phosphor-stop-fill.svg".into(),
            "phosphor-terminal-window.svg".into(),
        ])
    }
}
