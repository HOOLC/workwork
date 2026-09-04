use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use gpui::{
    A11ySubtreeBuilder, App, Bounds, Context, Element, ElementId, Entity, Global, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Render, Window,
};
use tokio::sync::watch;

use super::protocol::{
    AutomationRole, ElementInfo, Rect, UiSnapshot, Viewport, API_VERSION, COORDINATE_SPACE,
};

#[derive(Clone)]
pub(crate) struct AutomationRegistry {
    inner: Arc<Mutex<RegistryState>>,
    revision_tx: watch::Sender<u64>,
}

#[derive(Default)]
struct RegistryState {
    current: UiSnapshot,
    pending: Option<PendingFrame>,
}

struct PendingFrame {
    scale_factor: f32,
    viewport: Viewport,
    elements: BTreeMap<String, ElementInfo>,
}

impl AutomationRegistry {
    pub(crate) fn new() -> Self {
        let (revision_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(RegistryState::default())),
            revision_tx,
        }
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn begin_frame(&self, window: &Window) {
        let viewport = window.viewport_size();
        self.lock().pending = Some(PendingFrame {
            scale_factor: window.scale_factor(),
            viewport: Viewport {
                width: viewport.width.as_f32(),
                height: viewport.height.as_f32(),
            },
            elements: BTreeMap::new(),
        });
    }

    fn record(
        &self,
        metadata: &ElementMetadata,
        bounds: Bounds<Pixels>,
        visible_bounds: Bounds<Pixels>,
    ) {
        let bounds = rect(bounds);
        let visible_bounds = rect(visible_bounds);
        let visible = visible_bounds.is_visible();
        let center = if visible {
            visible_bounds.center()
        } else {
            bounds.center()
        };
        let info = ElementInfo {
            id: metadata.id.clone(),
            role: metadata.role,
            label: metadata.label.clone(),
            enabled: metadata.enabled,
            visible,
            bounds,
            visible_bounds,
            center,
            actions: metadata.role.actions(),
        };

        if let Some(pending) = self.lock().pending.as_mut() {
            pending.elements.insert(metadata.id.clone(), info);
        }
    }

    fn commit_frame(&self) {
        let revision = {
            let mut state = self.lock();
            let Some(pending) = state.pending.take() else {
                return;
            };
            let revision = state.current.revision.saturating_add(1);
            state.current = UiSnapshot {
                api_version: API_VERSION,
                revision,
                coordinate_space: COORDINATE_SPACE,
                scale_factor: pending.scale_factor,
                viewport: pending.viewport,
                elements: pending.elements.into_values().collect(),
            };
            revision
        };
        self.revision_tx.send_replace(revision);
    }

    pub(crate) fn snapshot(&self, include_hidden: bool) -> UiSnapshot {
        let mut snapshot = self.lock().current.clone();
        if !include_hidden {
            snapshot.elements.retain(|element| element.visible);
        }
        snapshot
    }

    pub(crate) fn element(&self, id: &str) -> Option<ElementInfo> {
        self.lock()
            .current
            .elements
            .iter()
            .find(|element| element.id == id)
            .cloned()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.lock().current.revision
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }
}

fn rect(bounds: Bounds<Pixels>) -> Rect {
    Rect {
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        width: bounds.size.width.as_f32(),
        height: bounds.size.height.as_f32(),
    }
}

pub(crate) struct AutomationRegistryGlobal(pub(crate) AutomationRegistry);

impl Global for AutomationRegistryGlobal {}

#[derive(Clone)]
struct ElementMetadata {
    id: String,
    role: AutomationRole,
    label: String,
    enabled: bool,
    register: bool,
}

enum Instrumentation {
    Frame,
    Target(ElementMetadata),
}

/// A transparent GPUI element wrapper used only to observe layout bounds.
/// It delegates all rendering and input behavior to the wrapped element.
pub struct AutomationElement<E: Element> {
    inner: E,
    instrumentation: Instrumentation,
}

impl<E: Element> IntoElement for AutomationElement<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: Element> Element for AutomationElement<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.inner.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.inner.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let registry = cx
            .try_global::<AutomationRegistryGlobal>()
            .map(|global| global.0.clone());

        if matches!(self.instrumentation, Instrumentation::Frame) {
            if let Some(registry) = registry.as_ref() {
                registry.begin_frame(window);
            }
        }

        if let Instrumentation::Target(metadata) = &self.instrumentation {
            if metadata.register {
                if let Some(registry) = registry.as_ref() {
                    let visible_bounds = bounds.intersect(&window.content_mask().bounds);
                    registry.record(metadata, bounds, visible_bounds);
                }
            }
        }

        let state = self
            .inner
            .prepaint(id, inspector_id, bounds, request_layout, window, cx);

        if matches!(self.instrumentation, Instrumentation::Frame) {
            if let Some(registry) = registry.as_ref() {
                registry.commit_frame();
            }
        }

        state
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }

    fn a11y_role(&self) -> Option<gpui::accesskit::Role> {
        self.inner.a11y_role()
    }

    fn write_a11y_info(&self, node: &mut gpui::accesskit::Node) {
        self.inner.write_a11y_info(node);
    }

    fn a11y_synthetic_children(
        &mut self,
        prepaint: &mut Self::PrepaintState,
        builder: &mut A11ySubtreeBuilder,
    ) {
        self.inner.a11y_synthetic_children(prepaint, builder);
    }
}

pub trait AutomationElementExt: IntoElement + Sized {
    fn automation(
        self,
        role: AutomationRole,
        label: impl Into<String>,
    ) -> AutomationElement<Self::Element> {
        self.automation_when(true, true, role, label)
    }

    fn automation_enabled(
        self,
        enabled: bool,
        role: AutomationRole,
        label: impl Into<String>,
    ) -> AutomationElement<Self::Element> {
        self.automation_when(true, enabled, role, label)
    }

    fn automation_when(
        self,
        register: bool,
        enabled: bool,
        role: AutomationRole,
        label: impl Into<String>,
    ) -> AutomationElement<Self::Element> {
        let inner = self.into_element();
        let id = inner
            .id()
            .unwrap_or_else(|| panic!("automated GPUI elements must have an id"))
            .to_string();
        AutomationElement {
            inner,
            instrumentation: Instrumentation::Target(ElementMetadata {
                id,
                role,
                label: label.into(),
                enabled,
                register,
            }),
        }
    }
}

impl<T: IntoElement> AutomationElementExt for T {}

pub struct AutomationRoot<V: Render> {
    inner: Entity<V>,
}

impl<V: Render> AutomationRoot<V> {
    pub fn new(inner: Entity<V>) -> Self {
        Self { inner }
    }
}

impl<V: Render> Render for AutomationRoot<V> {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        AutomationElement {
            inner: self.inner.clone().into_element(),
            instrumentation: Instrumentation::Frame,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_commits_a_stable_sorted_snapshot() {
        let registry = AutomationRegistry::new();
        {
            let mut state = registry.lock();
            state.pending = Some(PendingFrame {
                scale_factor: 2.0,
                viewport: Viewport {
                    width: 100.0,
                    height: 80.0,
                },
                elements: BTreeMap::new(),
            });
        }
        registry.record(
            &ElementMetadata {
                id: "z".to_owned(),
                role: AutomationRole::Button,
                label: "Last".to_owned(),
                enabled: true,
                register: true,
            },
            Bounds::new(
                gpui::point(gpui::px(10.0), gpui::px(10.0)),
                gpui::size(gpui::px(20.0), gpui::px(20.0)),
            ),
            Bounds::new(
                gpui::point(gpui::px(10.0), gpui::px(10.0)),
                gpui::size(gpui::px(20.0), gpui::px(20.0)),
            ),
        );
        registry.record(
            &ElementMetadata {
                id: "a".to_owned(),
                role: AutomationRole::TextInput,
                label: "First".to_owned(),
                enabled: true,
                register: true,
            },
            Bounds::new(
                gpui::point(gpui::px(-50.0), gpui::px(-50.0)),
                gpui::size(gpui::px(10.0), gpui::px(10.0)),
            ),
            Bounds::default(),
        );
        registry.commit_frame();

        let all = registry.snapshot(true);
        assert_eq!(all.revision, 1);
        assert_eq!(all.elements[0].id, "a");
        assert_eq!(all.elements[1].id, "z");
        assert!(!all.elements[0].visible);

        let visible = registry.snapshot(false);
        assert_eq!(visible.elements.len(), 1);
        assert_eq!(visible.elements[0].id, "z");
    }
}
