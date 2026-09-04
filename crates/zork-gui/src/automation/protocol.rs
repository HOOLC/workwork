use serde::{Deserialize, Serialize};

pub const API_VERSION: &str = "v1";
pub const COORDINATE_SPACE: &str = "window_content_logical_pixels";

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub(crate) fn center(self) -> Point {
        Point {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }

    pub(crate) fn is_visible(self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub(crate) fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRole {
    Button,
    Link,
    Option,
    ScrollArea,
    TextInput,
}

impl AutomationRole {
    pub(crate) fn actions(self) -> Vec<String> {
        let actions: &[&str] = match self {
            Self::Button | Self::Link | Self::Option => &["click", "move"],
            Self::ScrollArea => &["move", "scroll"],
            Self::TextInput => &["click", "move", "type_text"],
        };
        actions.iter().map(|action| (*action).to_owned()).collect()
    }

    pub(crate) fn supports(self, action: &str) -> bool {
        match self {
            Self::Button | Self::Link | Self::Option => matches!(action, "click" | "move"),
            Self::ScrollArea => matches!(action, "move" | "scroll"),
            Self::TextInput => matches!(action, "click" | "move" | "type_text"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ElementInfo {
    pub id: String,
    pub role: AutomationRole,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub bounds: Rect,
    pub visible_bounds: Rect,
    pub center: Point,
    pub actions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiSnapshot {
    pub api_version: &'static str,
    pub revision: u64,
    pub coordinate_space: &'static str,
    pub scale_factor: f32,
    pub viewport: Viewport,
    pub elements: Vec<ElementInfo>,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            api_version: API_VERSION,
            revision: 0,
            coordinate_space: COORDINATE_SPACE,
            scale_factor: 1.0,
            viewport: Viewport::default(),
            elements: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ActionTarget {
    Element(ElementTarget),
    Point(PointTarget),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ElementTarget {
    pub element_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PointTarget {
    pub x: f32,
    pub y: f32,
}

impl From<PointTarget> for Point {
    fn from(value: PointTarget) -> Self {
        Self {
            x: value.x,
            y: value.y,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseButtonName {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModifierState {
    pub alt: bool,
    pub control: bool,
    pub function: bool,
    pub platform: bool,
    pub shift: bool,
}

fn default_click_count() -> usize {
    1
}

fn default_drag_steps() -> usize {
    12
}

/// Every mutation in the dev API is represented by physical user input.
/// There are deliberately no business-domain commands in this protocol.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserAction {
    Click {
        target: ActionTarget,
        #[serde(default)]
        button: MouseButtonName,
        #[serde(default = "default_click_count")]
        click_count: usize,
        #[serde(default)]
        modifiers: ModifierState,
    },
    Drag {
        from: ActionTarget,
        to: ActionTarget,
        #[serde(default = "default_drag_steps")]
        steps: usize,
        #[serde(default)]
        modifiers: ModifierState,
    },
    Key {
        keystroke: String,
    },
    Move {
        target: ActionTarget,
        #[serde(default)]
        modifiers: ModifierState,
    },
    Scroll {
        target: ActionTarget,
        #[serde(default)]
        delta_x: f32,
        #[serde(default)]
        delta_y: f32,
        #[serde(default)]
        modifiers: ModifierState,
    },
    TypeText {
        text: String,
        #[serde(default)]
        target: Option<ActionTarget>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActionResult {
    pub accepted: bool,
    pub action: &'static str,
    pub dispatched_events: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Point>,
    pub revision_before: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ElementsResponse {
    #[serde(flatten)]
    pub snapshot: UiSnapshot,
    pub timed_out: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_protocol_contains_only_user_input_variants() {
        let cases = [
            r#"{"type":"click","target":{"element_id":"send-button"}}"#,
            r#"{"type":"move","target":{"x":10,"y":20}}"#,
            r#"{"type":"type_text","target":{"element_id":"composer-input"},"text":"hello"}"#,
            r#"{"type":"key","keystroke":"shift-enter"}"#,
            r#"{"type":"scroll","target":{"element_id":"session-list"},"delta_y":-120}"#,
            r#"{"type":"drag","from":{"x":1,"y":2},"to":{"x":3,"y":4}}"#,
        ];

        for json in cases {
            serde_json::from_str::<UserAction>(json).expect("user input action should parse");
        }

        let direct_business_command = r#"{"type":"send_message","text":"bypass UI"}"#;
        assert!(serde_json::from_str::<UserAction>(direct_business_command).is_err());
    }

    #[test]
    fn element_targets_do_not_accept_extra_coordinates() {
        let ambiguous = r#"{"type":"click","target":{"element_id":"x","x":1,"y":2}}"#;
        assert!(serde_json::from_str::<UserAction>(ambiguous).is_err());
    }
}
