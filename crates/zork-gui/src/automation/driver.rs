use std::io::Cursor;

use gpui::{
    point, px, AnyWindowHandle, App, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, Window,
};
use image::{DynamicImage, ImageFormat};
use tokio::sync::{mpsc, oneshot};
use unicode_segmentation::UnicodeSegmentation;

use super::element::AutomationRegistry;
use super::protocol::{
    ActionResult, ActionTarget, ModifierState, MouseButtonName, Point, UserAction,
};

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_KEYSTROKE_BYTES: usize = 128;
const MAX_DRAG_STEPS: usize = 120;

pub(crate) struct DriverEnvelope {
    pub(crate) command: DriverCommand,
    pub(crate) response: oneshot::Sender<Result<DriverOutput, DriverError>>,
}

pub(crate) enum DriverCommand {
    Action(UserAction),
    Screenshot,
}

pub(crate) enum DriverOutput {
    Action(ActionResult),
    Screenshot(Screenshot),
}

pub(crate) struct Screenshot {
    pub(crate) bytes: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct DriverError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl DriverError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub(crate) fn attach_driver(
    window_handle: AnyWindowHandle,
    registry: AutomationRegistry,
    mut receiver: mpsc::UnboundedReceiver<DriverEnvelope>,
    cx: &App,
) {
    cx.spawn(async move |cx| {
        while let Some(envelope) = receiver.recv().await {
            let result = window_handle
                .update(cx, |_, window, cx| {
                    execute(envelope.command, window, cx, &registry)
                })
                .map_err(|error| DriverError::new("window_unavailable", error.to_string()))
                .and_then(std::convert::identity);
            let _ = envelope.response.send(result);
        }
    })
    .detach();
}

fn execute(
    command: DriverCommand,
    window: &mut Window,
    cx: &mut App,
    registry: &AutomationRegistry,
) -> Result<DriverOutput, DriverError> {
    match command {
        DriverCommand::Action(action) => {
            execute_action(action, window, cx, registry).map(DriverOutput::Action)
        }
        DriverCommand::Screenshot => capture_screenshot(window, registry),
    }
}

fn capture_screenshot(
    window: &Window,
    registry: &AutomationRegistry,
) -> Result<DriverOutput, DriverError> {
    let image = window.render_to_image().map_err(|error| {
        DriverError::new(
            "screenshot_failed",
            format!("GPUI could not render the current window: {error}"),
        )
    })?;
    let width = image.width();
    let height = image.height();
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|error| {
            DriverError::new(
                "screenshot_encode_failed",
                format!("could not encode PNG: {error}"),
            )
        })?;
    Ok(DriverOutput::Screenshot(Screenshot {
        bytes: cursor.into_inner(),
        width,
        height,
        revision: registry.revision(),
    }))
}

fn execute_action(
    action: UserAction,
    window: &mut Window,
    cx: &mut App,
    registry: &AutomationRegistry,
) -> Result<ActionResult, DriverError> {
    let revision_before = registry.revision();
    match action {
        UserAction::Click {
            target,
            button,
            click_count,
            modifiers,
        } => {
            if !(1..=3).contains(&click_count) {
                return Err(DriverError::new(
                    "invalid_click_count",
                    "click_count must be between 1 and 3",
                ));
            }
            let position = resolve_target(&target, "click", window, registry)?;
            dispatch_click(
                position,
                button.into(),
                click_count,
                modifiers.into(),
                window,
                cx,
            );
            Ok(ActionResult {
                accepted: true,
                action: "click",
                dispatched_events: 1 + click_count * 2,
                position: Some(position),
                revision_before,
            })
        }
        UserAction::Move { target, modifiers } => {
            let position = resolve_target(&target, "move", window, registry)?;
            dispatch_move(position, None, modifiers.into(), window, cx);
            Ok(ActionResult {
                accepted: true,
                action: "move",
                dispatched_events: 1,
                position: Some(position),
                revision_before,
            })
        }
        UserAction::TypeText { text, target } => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(DriverError::new(
                    "text_too_large",
                    format!("text must not exceed {MAX_TEXT_BYTES} UTF-8 bytes"),
                ));
            }
            let mut dispatched_events = 0;
            let mut position = None;
            if let Some(target) = target.as_ref() {
                let target_position = resolve_target(target, "type_text", window, registry)?;
                dispatch_click(
                    target_position,
                    MouseButton::Left,
                    1,
                    Modifiers::default(),
                    window,
                    cx,
                );
                position = Some(target_position);
                dispatched_events += 3;
            }
            dispatched_events += dispatch_text(&text, window, cx)?;
            Ok(ActionResult {
                accepted: true,
                action: "type_text",
                dispatched_events,
                position,
                revision_before,
            })
        }
        UserAction::Key { keystroke } => {
            if keystroke.is_empty() || keystroke.len() > MAX_KEYSTROKE_BYTES {
                return Err(DriverError::new(
                    "invalid_keystroke",
                    format!("keystroke must contain 1 to {MAX_KEYSTROKE_BYTES} UTF-8 bytes"),
                ));
            }
            let keystroke = Keystroke::parse(&keystroke)
                .map_err(|error| DriverError::new("invalid_keystroke", error.to_string()))?;
            dispatch_keystroke(keystroke, window, cx);
            Ok(ActionResult {
                accepted: true,
                action: "key",
                dispatched_events: 2,
                position: None,
                revision_before,
            })
        }
        UserAction::Scroll {
            target,
            delta_x,
            delta_y,
            modifiers,
        } => {
            if !delta_x.is_finite() || !delta_y.is_finite() {
                return Err(DriverError::new(
                    "invalid_scroll_delta",
                    "scroll deltas must be finite numbers",
                ));
            }
            let position = resolve_target(&target, "scroll", window, registry)?;
            let modifiers = modifiers.into();
            dispatch_move(position, None, modifiers, window, cx);
            window.dispatch_event(
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: gpui_point(position),
                    delta: ScrollDelta::Pixels(point(px(delta_x), px(delta_y))),
                    modifiers,
                    touch_phase: TouchPhase::Moved,
                }),
                cx,
            );
            Ok(ActionResult {
                accepted: true,
                action: "scroll",
                dispatched_events: 2,
                position: Some(position),
                revision_before,
            })
        }
        UserAction::Drag {
            from,
            to,
            steps,
            modifiers,
        } => {
            if !(1..=MAX_DRAG_STEPS).contains(&steps) {
                return Err(DriverError::new(
                    "invalid_drag_steps",
                    format!("steps must be between 1 and {MAX_DRAG_STEPS}"),
                ));
            }
            let from = resolve_target(&from, "move", window, registry)?;
            let to = resolve_target(&to, "move", window, registry)?;
            let modifiers = modifiers.into();
            dispatch_drag(from, to, steps, modifiers, window, cx);
            Ok(ActionResult {
                accepted: true,
                action: "drag",
                dispatched_events: steps + 3,
                position: Some(to),
                revision_before,
            })
        }
    }
}

fn resolve_target(
    target: &ActionTarget,
    action: &str,
    window: &Window,
    registry: &AutomationRegistry,
) -> Result<Point, DriverError> {
    let point = match target {
        ActionTarget::Element(target) => {
            let element = registry.element(&target.element_id).ok_or_else(|| {
                DriverError::new(
                    "element_not_found",
                    format!("no rendered element has id {:?}", target.element_id),
                )
            })?;
            if !element.visible {
                return Err(DriverError::new(
                    "element_not_visible",
                    format!("element {:?} is clipped or off-screen", target.element_id),
                ));
            }
            if !element.role.supports(action) {
                return Err(DriverError::new(
                    "unsupported_element_action",
                    format!(
                        "element {:?} does not expose the {action:?} user action",
                        target.element_id
                    ),
                ));
            }
            element.center
        }
        ActionTarget::Point(target) => (*target).into(),
    };

    validate_point(point, window)?;
    Ok(point)
}

fn validate_point(position: Point, window: &Window) -> Result<(), DriverError> {
    if !position.is_finite() {
        return Err(DriverError::new(
            "invalid_coordinate",
            "coordinates must be finite numbers",
        ));
    }
    let viewport = window.viewport_size();
    if position.x < 0.0
        || position.y < 0.0
        || position.x > viewport.width.as_f32()
        || position.y > viewport.height.as_f32()
    {
        return Err(DriverError::new(
            "coordinate_out_of_bounds",
            format!(
                "point ({}, {}) is outside the {} x {} window content area",
                position.x,
                position.y,
                viewport.width.as_f32(),
                viewport.height.as_f32()
            ),
        ));
    }
    Ok(())
}

fn dispatch_click(
    position: Point,
    button: MouseButton,
    click_count: usize,
    modifiers: Modifiers,
    window: &mut Window,
    cx: &mut App,
) {
    dispatch_move(position, None, modifiers, window, cx);
    for current_count in 1..=click_count {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: gpui_point(position),
                modifiers,
                click_count: current_count,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: gpui_point(position),
                modifiers,
                click_count: current_count,
            }),
            cx,
        );
    }
}

fn dispatch_move(
    position: Point,
    pressed_button: Option<MouseButton>,
    modifiers: Modifiers,
    window: &mut Window,
    cx: &mut App,
) {
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position: gpui_point(position),
            pressed_button,
            modifiers,
        }),
        cx,
    );
}

fn dispatch_drag(
    from: Point,
    to: Point,
    steps: usize,
    modifiers: Modifiers,
    window: &mut Window,
    cx: &mut App,
) {
    dispatch_move(from, None, modifiers, window, cx);
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: gpui_point(from),
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        cx,
    );
    for step in 1..=steps {
        let progress = step as f32 / steps as f32;
        let position = Point {
            x: from.x + (to.x - from.x) * progress,
            y: from.y + (to.y - from.y) * progress,
        };
        dispatch_move(position, Some(MouseButton::Left), modifiers, window, cx);
    }
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: gpui_point(to),
            modifiers,
            click_count: 1,
        }),
        cx,
    );
}

fn dispatch_text(text: &str, window: &mut Window, cx: &mut App) -> Result<usize, DriverError> {
    let mut dispatched = 0;
    for grapheme in text.graphemes(true) {
        let keystroke = match grapheme {
            "\n" | "\r\n" => Keystroke::parse("shift-enter")
                .map_err(|error| DriverError::new("invalid_keystroke", error.to_string()))?,
            "\t" => Keystroke::parse("tab")
                .map_err(|error| DriverError::new("invalid_keystroke", error.to_string()))?,
            _ => Keystroke {
                modifiers: Modifiers::default(),
                key: grapheme.to_owned(),
                key_char: Some(grapheme.to_owned()),
            },
        };
        dispatch_keystroke(keystroke, window, cx);
        dispatched += 2;
    }
    Ok(dispatched)
}

fn dispatch_keystroke(keystroke: Keystroke, window: &mut Window, cx: &mut App) {
    window.dispatch_keystroke(keystroke.clone(), cx);
    window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
}

fn gpui_point(position: Point) -> gpui::Point<gpui::Pixels> {
    point(px(position.x), px(position.y))
}

impl From<ModifierState> for Modifiers {
    fn from(value: ModifierState) -> Self {
        Self {
            control: value.control,
            alt: value.alt,
            shift: value.shift,
            platform: value.platform,
            function: value.function,
        }
    }
}

impl From<MouseButtonName> for MouseButton {
    fn from(value: MouseButtonName) -> Self {
        match value {
            MouseButtonName::Left => Self::Left,
            MouseButtonName::Right => Self::Right,
            MouseButtonName::Middle => Self::Middle,
        }
    }
}
