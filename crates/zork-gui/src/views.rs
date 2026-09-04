//! Root view driven by the gateway-owned local IM entry.
//!
//! Left pane: compact workspace-grouped task navigation.
//! Center pane: virtualized delivered-message history with composer (IM send, cancel,
//! profile/model/thinking selectors).
//! Agent state is driven by rich `status` SSE events with periodic
//! `GET /v1/sessions` polling as a coarse connectivity fallback.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use gpui::{
    div, prelude::*, px, rgb, rgba, svg, BoxShadow, Context, Div, Empty, Entity, FocusHandle,
    FollowMode, FontWeight, KeyDownEvent, ListAlignment, ListState, Render, Styled, Task, Window,
};

use crate::api::{
    AgentStatus, ContextConfig, ContextStrategy, GatewayClient, MessagePage, ProfileInfo,
    ProfileModel, Role, SessionStatus, SessionSummary, SseEvent, TranscriptMessage,
};
use crate::automation::{AutomationElementExt, AutomationRole};
use crate::components::message::render_markdown;
use crate::components::selector_menu::{SelectorKind, SelectorMenuState};
use crate::components::text_input::{ComposerInput, ComposerSubmit};
use crate::design::CODEX_UI;
pub use crate::transcript::TranscriptLine;
use crate::transcript::{
    begin_optimistic_user, prepend_older_lines, project_page_with_pending,
    rollback_optimistic_user, should_render_live_activity, stable_task_title,
    take_pending_user_echo,
};

const MESSAGE_PAGE_LIMIT: u32 = 100;

const BG: u32 = CODEX_UI.palette.canvas;
const PANEL: u32 = CODEX_UI.palette.sidebar;
const PANEL_HOVER: u32 = CODEX_UI.palette.sidebar_hover;
const SELECTED: u32 = CODEX_UI.palette.selected;
const PROMPT: u32 = CODEX_UI.palette.prompt;
const BORDER: u32 = CODEX_UI.palette.border;
const BORDER_STRONG: u32 = CODEX_UI.palette.border_strong;
const TEXT: u32 = CODEX_UI.palette.text;
const DIM: u32 = CODEX_UI.palette.muted;
const SUBTLE: u32 = CODEX_UI.palette.subtle;
const GREEN: u32 = CODEX_UI.palette.success;
const AMBER: u32 = CODEX_UI.palette.warning;
const RED: u32 = CODEX_UI.palette.danger;

#[derive(Debug, PartialEq)]
enum DecodedSseEvent {
    Transcript(TranscriptMessage),
    Status(AgentStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComposerMode {
    NewTask,
    FollowUp,
}

fn decode_sse_event(event: &SseEvent) -> Result<Option<DecodedSseEvent>, serde_json::Error> {
    match event.name.as_str() {
        "message" => serde_json::from_str(&event.data)
            .map(DecodedSseEvent::Transcript)
            .map(Some),
        "status" => serde_json::from_str(&event.data)
            .map(DecodedSseEvent::Status)
            .map(Some),
        _ => Ok(None),
    }
}

pub struct RootView {
    client: Arc<GatewayClient>,

    // Left pane: sessions
    sessions: Vec<SessionSummary>,
    selected_session: Option<String>,
    selected_status_cache: Option<SessionStatus>,
    agent_online: bool,

    // Selection selectors (profile / model / thinking)
    profiles: Vec<ProfileInfo>,
    sel_profile: usize,
    sel_model: usize,
    sel_thinking: usize,
    selection_dirty: bool,
    context_config: Option<ContextConfig>,
    context_busy: bool,
    context_request: u64,
    context_feedback: Option<String>,
    selector_menu: SelectorMenuState,

    // Transcript
    lines: Vec<TranscriptLine>,
    pending_user: Vec<String>,
    older_cursor: Option<String>,
    has_older: bool,
    loading_older: bool,
    activity: Option<AgentStatus>,

    // Composer / new-session form
    composer_input: Entity<ComposerInput>,
    workspace_input: String,
    workspace_focus: FocusHandle,
    selector_focus: FocusHandle,
    focus_initialized: bool,
    sending: bool,
    creating: bool,
    canceling: bool,
    connection_error: Option<String>,
    error: Option<String>,

    transcript_list: ListState,
    poll_task: Option<Task<()>>,
    sse_task: Option<Task<()>>,
}

impl RootView {
    pub fn new(client: Arc<GatewayClient>, cx: &mut Context<Self>) -> Self {
        let workspace_input = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_owned());
        let composer_input = cx.new(|cx| ComposerInput::new("Ask anything", cx));
        cx.subscribe(
            &composer_input,
            |view, _input, _event: &ComposerSubmit, cx| {
                if view.selected_session.is_none() {
                    view.create_session(cx);
                } else {
                    view.send_composer(cx);
                }
            },
        )
        .detach();
        Self {
            client,
            sessions: Vec::new(),
            selected_session: None,
            selected_status_cache: None,
            agent_online: false,
            profiles: Vec::new(),
            sel_profile: 0,
            sel_model: 0,
            sel_thinking: 0,
            selection_dirty: false,
            context_config: None,
            context_busy: false,
            context_request: 0,
            context_feedback: None,
            selector_menu: SelectorMenuState::default(),
            lines: Vec::new(),
            pending_user: Vec::new(),
            older_cursor: None,
            has_older: false,
            loading_older: false,
            activity: None,
            composer_input,
            workspace_input,
            workspace_focus: cx.focus_handle(),
            selector_focus: cx.focus_handle(),
            focus_initialized: false,
            sending: false,
            creating: false,
            canceling: false,
            connection_error: None,
            error: None,
            transcript_list: ListState::new(0, ListAlignment::Bottom, px(500.)),
            poll_task: None,
            sse_task: None,
        }
    }

    // ---------------------------------------------------------------- tasks

    fn start_background(&mut self, cx: &mut Context<Self>) {
        if self.poll_task.is_some() {
            return;
        }
        let client = self.client.clone();
        self.poll_task = Some(cx.spawn(async move |this, cx| loop {
            match client.list_sessions().await {
                Ok(sessions) => {
                    this.update(cx, |view, cx| {
                        view.agent_online = true;
                        view.connection_error = None;
                        if should_adopt_session_workspace(&view.workspace_input) {
                            if let Some(workspace) = sessions
                                .first()
                                .map(|session| session.workspace.as_str())
                                .filter(|workspace| !workspace.is_empty())
                            {
                                view.workspace_input = workspace.to_owned();
                            }
                        }
                        view.sessions = sessions;
                        view.refresh_selected_status();
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    this.update(cx, |view, cx| {
                        view.agent_online = false;
                        view.connection_error = Some(format!("sessions: {error}"));
                        cx.notify();
                    })
                    .ok();
                }
            }
            if this.upgrade().is_none() {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }));
        // Retry profile discovery until the gateway is reachable. Starting
        // the GUI before the gateway must not require an application restart.
        let client = self.client.clone();
        cx.spawn(async move |this, cx| loop {
            match client.list_profiles().await {
                Ok(profiles) => {
                    this.update(cx, |view, cx| {
                        view.profiles = profiles;
                        view.sync_selections();
                        cx.notify();
                    })
                    .ok();
                    break;
                }
                Err(_) if this.upgrade().is_some() => {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                }
                Err(_) => break,
            }
        })
        .detach();
    }

    fn refresh_selected_status(&mut self) {
        let Some(id) = self.selected_session.as_deref() else {
            self.selected_status_cache = None;
            return;
        };
        let status = self
            .sessions
            .iter()
            .find(|session| session.session_id == id)
            .map(|session| session.status);
        match status {
            Some(status) => {
                self.selected_status_cache = Some(status);
            }
            None => self.selected_status_cache = None,
        }
    }

    fn start_sse(&mut self, cx: &mut Context<Self>) {
        let id = match self.selected_session.clone() {
            Some(id) => id,
            None => return,
        };
        let client = self.client.clone();
        self.sse_task = Some(cx.spawn(async move |this, cx| {
            loop {
                if this.upgrade().is_none() {
                    break;
                }
                match client.stream_events(&id).await {
                    Ok(mut stream) => {
                        // Catch up: reload the latest page after (re)connecting,
                        // since the agent's SSE broadcast is live-only.
                        if let Ok(page) = client.list_messages(&id, None, MESSAGE_PAGE_LIMIT).await
                        {
                            this.update(cx, |view, cx| {
                                if view.selected_session.as_deref() == Some(id.as_str()) {
                                    view.apply_message_page(&page);
                                    cx.notify();
                                }
                            })
                            .ok();
                        }
                        while let Some(event) = stream.next().await {
                            match event {
                                Ok(ev) => {
                                    this.update(cx, |view, cx| {
                                        if view.selected_session.as_deref() == Some(id.as_str()) {
                                            view.apply_sse(&ev);
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                                }
                                Err(err) => {
                                    this.update(cx, |view, cx| {
                                        if view.selected_session.as_deref() == Some(id.as_str()) {
                                            view.activity = None;
                                            view.error = Some(format!("stream: {err}"));
                                        }
                                        cx.notify();
                                    })
                                    .ok();
                                    break;
                                }
                            }
                        }
                        this.update(cx, |view, cx| {
                            if view.selected_session.as_deref() == Some(id.as_str()) {
                                view.activity = None;
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                    Err(err) => {
                        this.update(cx, |view, cx| {
                            if view.selected_session.as_deref() == Some(id.as_str()) {
                                view.activity = None;
                                view.error = Some(format!("sse: {err}"));
                            }
                            cx.notify();
                        })
                        .ok();
                    }
                }
                // Brief backoff before reconnecting.
                cx.background_executor()
                    .timer(Duration::from_millis(750))
                    .await;
            }
        }));
    }

    // ------------------------------------------------------------- actions

    fn select_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected_session.as_deref() == Some(id) {
            return;
        }
        self.selected_session = Some(id.to_owned());
        self.context_request = self.context_request.wrapping_add(1);
        self.context_config = None;
        self.context_busy = false;
        self.context_feedback = None;
        self.selector_menu.dismiss();
        self.load_context(None, cx);
        let old = self.lines.len();
        self.lines.clear();
        self.transcript_list.splice(0..old, 0);
        self.transcript_list.set_follow_mode(FollowMode::Tail);
        self.transcript_list.scroll_to_end();
        self.pending_user.clear();
        self.older_cursor = None;
        self.has_older = false;
        self.loading_older = false;
        self.activity = None;
        self.sending = false;
        self.canceling = false;
        self.selection_dirty = false;
        self.error = None;
        self.refresh_selected_status();
        self.sync_selections();
        self.start_sse(cx);

        let client = self.client.clone();
        let sid = id.to_owned();
        cx.spawn(async move |this, cx| {
            match client.list_messages(&sid, None, MESSAGE_PAGE_LIMIT).await {
                Ok(page) => {
                    this.update(cx, |view, cx| {
                        if view.selected_session.as_deref() == Some(sid.as_str()) {
                            view.apply_message_page(&page);
                            view.transcript_list.scroll_to_end();
                            cx.notify();
                        }
                    })
                    .ok();
                }
                Err(err) => {
                    this.update(cx, |view, cx| {
                        view.error = Some(format!("load messages: {err}"));
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn deselect_session(&mut self, cx: &mut Context<Self>) {
        self.selected_session = None;
        self.context_request = self.context_request.wrapping_add(1);
        self.context_config = None;
        self.context_busy = false;
        self.context_feedback = None;
        self.selector_menu.dismiss();
        self.selected_status_cache = None;
        self.sse_task = None;
        let old = self.lines.len();
        self.lines.clear();
        self.transcript_list.splice(0..old, 0);
        self.pending_user.clear();
        self.older_cursor = None;
        self.has_older = false;
        self.loading_older = false;
        self.activity = None;
        self.sending = false;
        self.canceling = false;
        self.error = None;
        self.selection_dirty = false;
        cx.notify();
    }

    fn apply_message_page(&mut self, page: &MessagePage) {
        let old = self.lines.len();
        // Gateway-visible pages arrive oldest-first; keep that order.
        self.lines = project_page_with_pending(&page.items, &mut self.pending_user);
        self.older_cursor = page.older_cursor.clone();
        self.has_older = page.older_cursor.is_some();
        self.loading_older = false;
        self.transcript_list.splice(0..old, self.lines.len());
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(cursor) = self.older_cursor.clone() else {
            return;
        };
        if self.loading_older {
            return;
        }
        self.loading_older = true;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            match client
                .list_messages(&id, Some(&cursor), MESSAGE_PAGE_LIMIT)
                .await
            {
                Ok(page) => {
                    this.update(cx, |view, cx| {
                        if view.selected_session.as_deref() == Some(id.as_str()) {
                            view.older_cursor = page.older_cursor.clone();
                            view.has_older = page.older_cursor.is_some();
                            view.loading_older = false;
                            let inserted = prepend_older_lines(&mut view.lines, &page.items);
                            if inserted > 0 {
                                view.transcript_list.splice(0..0, inserted);
                            }
                            cx.notify();
                        }
                    })
                    .ok();
                }
                Err(err) => {
                    this.update(cx, |view, cx| {
                        view.loading_older = false;
                        view.error = Some(format!("load older: {err}"));
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn apply_sse(&mut self, ev: &SseEvent) {
        let Ok(Some(event)) = decode_sse_event(ev) else {
            return;
        };
        match event {
            DecodedSseEvent::Transcript(message) => {
                let TranscriptMessage::Message { role, content } = message;
                if role == Role::User && take_pending_user_echo(&mut self.pending_user, &content) {
                    return;
                }
                self.push_line(TranscriptLine::Message { role, content });
            }
            DecodedSseEvent::Status(status) => self.apply_agent_status(status),
        }
    }

    fn apply_agent_status(&mut self, status: AgentStatus) {
        self.activity = Some(status);
    }

    fn push_line(&mut self, line: TranscriptLine) {
        let n = self.lines.len();
        self.lines.push(line);
        self.transcript_list.splice(n..n, 1);
    }

    fn send_composer(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let content = self.composer_input.read(cx).value().trim().to_owned();
        if content.is_empty() || self.sending {
            return;
        }
        let need_selection = self.selection_dirty;
        let selection = need_selection.then(|| self.current_selection()).flatten();
        if need_selection && selection.is_none() {
            self.error = Some("profile, model, and thinking are required".to_owned());
            cx.notify();
            return;
        }
        self.composer_input.update(cx, |input, cx| input.clear(cx));
        self.sending = true;
        self.error = None;
        let client = self.client.clone();
        let user_text = content;
        let optimistic_index = self.lines.len();
        begin_optimistic_user(&mut self.lines, &mut self.pending_user, user_text.clone());
        self.transcript_list
            .splice(optimistic_index..optimistic_index, 1);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut applied_selection = None;
            let result = if let Some((profile_id, model, thinking)) = selection.as_ref() {
                match client
                    .update_selection(&id, profile_id, model, thinking)
                    .await
                {
                    Ok(_) => {
                        applied_selection = selection.clone();
                        client.post_message(&id, &user_text).await
                    }
                    Err(error) => Err(error),
                }
            } else {
                client.post_message(&id, &user_text).await
            };
            this.update(cx, |view, cx| {
                if view.selected_session.as_deref() != Some(id.as_str()) {
                    return;
                }
                view.sending = false;
                if applied_selection.is_some()
                    && view.current_selection().as_ref() == applied_selection.as_ref()
                {
                    view.selection_dirty = false;
                }
                match result {
                    Ok(()) => {}
                    Err(err) => {
                        let old_count = view.lines.len();
                        if rollback_optimistic_user(
                            &mut view.lines,
                            &mut view.pending_user,
                            &user_text,
                        ) {
                            let new_count = view.lines.len();
                            view.transcript_list.splice(0..old_count, new_count);
                        }
                        view.composer_input
                            .update(cx, |input, cx| input.set_value(user_text, cx));
                        view.error = Some(format!("send: {err}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel_session(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        if self.canceling {
            return;
        }
        self.canceling = true;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.cancel_session(&id).await;
            this.update(cx, |view, cx| {
                view.canceling = false;
                if let Err(err) = result {
                    view.error = Some(format!("cancel: {err}"));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn create_session(&mut self, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let selection = self.current_selection();
        let Some((profile_id, model, thinking)) = selection else {
            self.error = Some("select a profile and model first".to_owned());
            cx.notify();
            return;
        };
        let prompt = self.composer_input.read(cx).value().to_owned();
        let (workspace, initial_prompt) =
            match validate_new_task_inputs(&self.workspace_input, &prompt) {
                Ok(inputs) => inputs,
                Err(message) => {
                    self.error = Some(message.to_owned());
                    cx.notify();
                    return;
                }
            };
        self.creating = true;
        self.error = None;
        self.composer_input.update(cx, |input, cx| input.clear(cx));
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client
                .create_session(&profile_id, &model, &thinking, &workspace)
                .await;
            let outcome = match result {
                Ok(session) => {
                    let append = client
                        .post_message(&session.session_id, &initial_prompt)
                        .await;
                    Ok((session, append))
                }
                Err(error) => Err(error),
            };
            this.update(cx, |view, cx| {
                view.creating = false;
                match outcome {
                    Ok((session, Ok(()))) => {
                        view.select_session(&session.session_id, cx);
                    }
                    Ok((session, Err(err))) => {
                        view.select_session(&session.session_id, cx);
                        view.composer_input
                            .update(cx, |input, cx| input.set_value(initial_prompt, cx));
                        view.error = Some(format!("start task: {err}"));
                    }
                    Err(err) => {
                        view.composer_input
                            .update(cx, |input, cx| input.set_value(initial_prompt, cx));
                        view.error = Some(format!("create: {err}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ----------------------------------------------------------- selectors

    fn current_model(&self) -> Option<&ProfileModel> {
        self.profiles
            .get(self.sel_profile)
            .and_then(|p| p.models.get(self.sel_model))
    }

    fn thinking_options(&self, model: &ProfileModel) -> Vec<String> {
        if model.thinking.is_empty() {
            vec![model.default_thinking.clone()]
        } else {
            model.thinking.clone()
        }
    }

    fn current_selection(&self) -> Option<(String, String, String)> {
        let profile = self.profiles.get(self.sel_profile)?;
        let model = profile.models.get(self.sel_model)?;
        let options = self.thinking_options(model);
        let thinking = options
            .get(self.sel_thinking.clamp(0, options.len().saturating_sub(1)))
            .cloned()
            .or_else(|| options.into_iter().next())?;
        Some((profile.profile_id.clone(), model.id.clone(), thinking))
    }

    fn selector_labels(&self) -> (String, String, String) {
        match self.current_selection() {
            Some((profile, model, thinking)) => {
                let short = |name: &str| name.rsplit('/').next().unwrap_or(name).to_owned();
                (short(&profile), model, thinking)
            }
            None => (
                "profiles".to_owned(),
                "models".to_owned(),
                "thinking".to_owned(),
            ),
        }
    }

    fn sync_selections(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        if let Some(id) = self.selected_session.as_deref() {
            if let Some(session) = self.sessions.iter().find(|s| s.session_id == id) {
                if let Some(p) = self
                    .profiles
                    .iter()
                    .position(|p| p.profile_id == session.profile_id)
                {
                    self.sel_profile = p;
                }
                let profile = self.profiles.get(self.sel_profile);
                if let Some(profile) = profile {
                    if let Some(m) = profile.models.iter().position(|m| m.id == session.model) {
                        self.sel_model = m;
                    }
                    if let Some(model) = profile.models.get(self.sel_model) {
                        if let Some(t) = model.thinking.iter().position(|t| t == &session.thinking)
                        {
                            self.sel_thinking = t;
                        }
                    }
                }
            }
        }
    }

    fn load_context(&mut self, update: Option<ContextConfig>, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        if self.context_busy {
            return;
        }
        self.context_request = self.context_request.wrapping_add(1);
        let request = self.context_request;
        let saving = update.is_some();
        self.context_busy = true;
        self.context_feedback = None;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.session_context(&id, update).await;
            this.update(cx, |view, cx| {
                if view.selected_session.as_deref() != Some(&id) || view.context_request != request
                {
                    return;
                }
                view.context_busy = false;
                match result {
                    Ok(config) => {
                        view.context_config = Some(config);
                        if saving {
                            view.context_feedback = Some(
                                "Context setting saved; applies at the next transition".into(),
                            );
                        }
                    }
                    Err(error) => view.error = Some(format!("context: {error}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn context_options(&self) -> Vec<(String, ContextConfig)> {
        let Some(current) = self.context_config.as_ref() else {
            return Vec::new();
        };
        let mut counts = vec![0, 8_000, 20_000, 40_000];
        if !counts.contains(&current.keep_recent_tokens) {
            counts.push(current.keep_recent_tokens);
        }
        counts.sort_unstable();
        let mut options = counts
            .into_iter()
            .map(|keep_recent_tokens| {
                (
                    format!("Summary + {keep_recent_tokens} recent tokens"),
                    ContextConfig {
                        strategy: ContextStrategy::Compaction,
                        keep_recent_tokens,
                    },
                )
            })
            .collect::<Vec<_>>();
        options.push((
            "Handoff document".into(),
            ContextConfig {
                strategy: ContextStrategy::Handoff,
                keep_recent_tokens: current.keep_recent_tokens,
            },
        ));
        options
    }

    fn mark_selection_dirty(&mut self) {
        if self.selected_session.is_some() {
            self.selection_dirty = true;
        }
    }

    fn selector_option_count(&self, kind: SelectorKind) -> usize {
        match kind {
            SelectorKind::Context => {
                if self.context_busy {
                    0
                } else {
                    self.context_options().len()
                }
            }
            SelectorKind::Profile => self.profiles.len(),
            SelectorKind::Model => self
                .profiles
                .get(self.sel_profile)
                .map(|profile| profile.models.len())
                .unwrap_or(0),
            SelectorKind::Thinking => self
                .current_model()
                .map(|model| self.thinking_options(model).len())
                .unwrap_or(0),
        }
    }

    fn selected_selector_index(&self, kind: SelectorKind) -> usize {
        match kind {
            SelectorKind::Context => self
                .context_options()
                .iter()
                .position(|(_, config)| Some(config) == self.context_config.as_ref())
                .unwrap_or(0),
            SelectorKind::Profile => self.sel_profile,
            SelectorKind::Model => self.sel_model,
            SelectorKind::Thinking => self.sel_thinking,
        }
    }

    fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus_handle = self.composer_input.read(cx).focus_handle();
        window.focus(&focus_handle, cx);
    }

    fn toggle_selector_menu(
        &mut self,
        kind: SelectorKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if kind == SelectorKind::Context && (self.context_busy || self.context_config.is_none()) {
            self.load_context(None, cx);
            return;
        }
        if self.selector_menu.open() == Some(kind) {
            self.selector_menu.dismiss();
            self.focus_composer(window, cx);
        } else {
            self.selector_menu
                .open_at(kind, self.selected_selector_index(kind));
            window.focus(&self.selector_focus, cx);
        }
        cx.notify();
    }

    fn choose_selector(
        &mut self,
        kind: SelectorKind,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let option_count = self.selector_option_count(kind);
        let Some(index) = self.selector_menu.choose(kind, index, option_count) else {
            return;
        };

        let changed = match kind {
            SelectorKind::Context => {
                if let Some((_, config)) = self.context_options().get(index).cloned() {
                    self.load_context(Some(config), cx);
                }
                false
            }
            SelectorKind::Profile => {
                let changed = self.sel_profile != index;
                self.sel_profile = index;
                if changed {
                    self.sel_model = 0;
                    self.sel_thinking = 0;
                }
                changed
            }
            SelectorKind::Model => {
                let changed = self.sel_model != index;
                self.sel_model = index;
                if changed {
                    self.sel_thinking = 0;
                }
                changed
            }
            SelectorKind::Thinking => {
                let changed = self.sel_thinking != index;
                self.sel_thinking = index;
                changed
            }
        };
        if changed {
            self.mark_selection_dirty();
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn dismiss_selector_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selector_menu.open().is_some() {
            self.selector_menu.dismiss();
            self.focus_composer(window, cx);
            cx.notify();
        }
    }

    fn selector_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = self.selector_menu.open() else {
            return;
        };
        let option_count = self.selector_option_count(kind);
        let handled = match event.keystroke.key.as_str() {
            "up" => {
                self.selector_menu.move_highlight(-1, option_count);
                cx.notify();
                true
            }
            "down" => {
                self.selector_menu.move_highlight(1, option_count);
                cx.notify();
                true
            }
            "enter" => {
                if let Some(index) = self.selector_menu.highlighted() {
                    self.choose_selector(kind, index, window, cx);
                }
                true
            }
            "escape" => {
                self.dismiss_selector_menu(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    // ------------------------------------------------------------ input

    fn workspace_key_down(&mut self, e: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = e.keystroke.key.as_str();
        if key == "backspace" {
            self.workspace_input.pop();
        } else if key == "enter" {
            self.create_session(cx);
        } else if !e.keystroke.modifiers.modified() {
            if let Some(ch) = e.keystroke.key_char.as_deref() {
                self.workspace_input.push_str(ch);
            }
        }
        cx.notify();
    }

    fn apply_home_suggestion(
        &mut self,
        prompt: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_input
            .update(cx, |input, cx| input.set_value(prompt, cx));
        self.error = None;
        let focus_handle = self.composer_input.read(cx).focus_handle();
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    fn selected_status(&self) -> Option<SessionStatus> {
        self.selected_status_cache
    }
}

// ---------------------------------------------------------------- rendering

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start_background(cx);
        if !self.focus_initialized {
            self.focus_initialized = true;
            let focus_handle = self.composer_input.read(cx).focus_handle();
            window.focus(&focus_handle, cx);
        }

        div()
            .size_full()
            .flex()
            .flex_row()
            .track_focus(&self.selector_focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                view.selector_key_down(event, window, cx);
            }))
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(14.))
            .child(self.render_left_pane(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .bg(rgb(BG))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(0.), rgba(0x1A1C1F1E).into())
                            .spread_radius(px(0.5)),
                        BoxShadow::new(px(0.), px(3.), rgba(0x0000000A).into())
                            .blur_radius(px(7.5)),
                        BoxShadow::new(px(0.), px(0.), rgba(0x0000000D).into())
                            .blur_radius(px(20.)),
                    ])
                    .child(self.render_center_pane(cx)),
            )
    }
}

impl RootView {
    fn render_left_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        type TaskRow = (String, String, bool);
        let mut groups: Vec<(String, String, Vec<TaskRow>)> = Vec::new();
        for session in &self.sessions {
            let workspace_path = session.workspace.clone();
            let workspace_label = short_path(&session.workspace).to_owned();
            let selected_title = (self.selected_session.as_deref()
                == Some(session.session_id.as_str()))
            .then(|| stable_task_title(&session.session_id, &self.lines, self.has_older));
            let row = (
                session.session_id.clone(),
                selected_title.unwrap_or_else(|| format!("Task {}", short_id(&session.session_id))),
                session.status == SessionStatus::Working,
            );
            if let Some((_, _, rows)) = groups
                .iter_mut()
                .find(|(path, _, _)| path == &workspace_path)
            {
                rows.push(row);
            } else {
                groups.push((workspace_path, workspace_label, vec![row]));
            }
        }
        if groups.is_empty() && !self.workspace_input.is_empty() {
            groups.push((
                self.workspace_input.clone(),
                short_path(&self.workspace_input).to_owned(),
                Vec::new(),
            ));
        }
        let selected = self.selected_session.clone();
        let online = self.agent_online;
        let has_sessions = !self.sessions.is_empty();
        let current_workspace = self.workspace_input.clone();
        let mut task_groups = Vec::new();
        for (workspace_path, workspace_label, rows) in groups {
            let workspace_selected = rows
                .iter()
                .any(|(id, _, _)| selected.as_deref() == Some(id.as_str()))
                || (selected.is_none() && current_workspace == workspace_path);
            let selected_workspace = workspace_path.clone();
            let mut group = div().flex().flex_col().gap(px(1.)).mb_3().child(
                div()
                    .id(format!("workspace-{workspace_label}"))
                    .h(px(CODEX_UI.sidebar.row_height))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded(px(CODEX_UI.sidebar.row_radius))
                    .bg(rgb(if workspace_selected {
                        CODEX_UI.sidebar.selected_fill
                    } else {
                        PANEL
                    }))
                    .hover(|style| style.bg(rgb(PANEL_HOVER)))
                    .on_click(cx.listener(move |v, _, _window, cx| {
                        v.workspace_input = selected_workspace.clone();
                        v.deselect_session(cx);
                    }))
                    .child(
                        svg()
                            .path("icons/phosphor-folder-simple.svg")
                            .size(px(16.))
                            .text_color(rgb(TEXT)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(CODEX_UI.sidebar.item_font_size))
                            .line_height(px(CODEX_UI.sidebar.item_line_height))
                            .text_color(rgb(TEXT))
                            .child(truncate(&workspace_label, 28).to_owned()),
                    )
                    .automation(
                        AutomationRole::Button,
                        format!("Workspace {workspace_label}"),
                    ),
            );
            for (id, title, working) in rows {
                let is_selected = selected.as_deref() == Some(id.as_str());
                let row_id = id.clone();
                group = group.child(
                    div()
                        .id(id)
                        .h(px(CODEX_UI.sidebar.row_height))
                        .flex()
                        .flex_row()
                        .items_center()
                        .pl_8()
                        .pr_2()
                        .rounded(px(CODEX_UI.sidebar.row_radius))
                        .bg(rgb(if is_selected { SELECTED } else { PANEL }))
                        .hover(|style| style.bg(rgb(PANEL_HOVER)))
                        .on_click(cx.listener(move |v, _, _window, cx| {
                            v.select_session(&row_id, cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(CODEX_UI.sidebar.item_font_size))
                                .line_height(px(CODEX_UI.sidebar.item_line_height))
                                .text_color(rgb(TEXT))
                                .child(truncate(&title, 28).to_owned()),
                        )
                        .when(working, |row| {
                            row.child(
                                div()
                                    .size(px(6.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(rgb(GREEN)),
                            )
                        })
                        .automation(AutomationRole::Button, title),
                );
            }
            task_groups.push(group);
        }

        div()
            .w(px(CODEX_UI.layout.sidebar_width))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(div().h(px(CODEX_UI.sidebar.toolbar_height)).flex_shrink_0())
            .child(
                div()
                    .h(px(40.))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(17.))
                            .line_height(px(24.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("zork")
                            .child(
                                svg()
                                    .path("icons/phosphor-caret-down.svg")
                                    .size(px(12.))
                                    .text_color(rgba(0x1A1C1F7E)),
                            ),
                    ),
            )
            .child(
                div().px(px(CODEX_UI.sidebar.inline_inset)).pb_2().child(
                    div()
                        .id("new-session")
                        .h(px(CODEX_UI.sidebar.row_height))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .rounded(px(CODEX_UI.sidebar.row_radius))
                        .text_size(px(CODEX_UI.sidebar.item_font_size))
                        .line_height(px(CODEX_UI.sidebar.item_line_height))
                        .hover(|style| style.bg(rgb(PANEL_HOVER)))
                        .on_click(cx.listener(move |v, _, _window, cx| {
                            v.deselect_session(cx);
                        }))
                        .child(
                            svg()
                                .path("icons/phosphor-terminal-window.svg")
                                .size(px(16.))
                                .text_color(rgb(TEXT)),
                        )
                        .child("New task")
                        .automation(AutomationRole::Button, "New task"),
                ),
            )
            .child(
                div()
                    .id("session-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(CODEX_UI.sidebar.inline_inset))
                    .pb_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_2()
                                    .pt_4()
                                    .pb_1()
                                    .text_size(px(CODEX_UI.sidebar.section_label_font_size))
                                    .line_height(px(CODEX_UI.sidebar.section_label_line_height))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgba(0x1A1C1F7E))
                                    .child("Projects"),
                            )
                            .children(task_groups)
                            .when(!has_sessions, |list| {
                                list.child(
                                    div()
                                        .px_2()
                                        .py_2()
                                        .text_size(px(13.))
                                        .line_height(px(20.))
                                        .text_color(rgba(0x1A1C1F7E))
                                        .child(if self.agent_online {
                                            "No recent tasks"
                                        } else {
                                            "Connecting to the agent…"
                                        }),
                                )
                            }),
                    )
                    .automation(AutomationRole::ScrollArea, "Task list"),
            )
            .child(
                div()
                    .h(px(CODEX_UI.sidebar.footer_height))
                    .flex()
                    .items_center()
                    .px(px(CODEX_UI.sidebar.inline_inset))
                    .child(
                        div()
                            .h(px(CODEX_UI.sidebar.row_height))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .rounded(px(CODEX_UI.sidebar.row_radius))
                            .hover(|style| style.bg(rgb(PANEL_HOVER)))
                            .child(
                                div()
                                    .size(px(18.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(rgb(if online { 0xE7F5EC } else { 0xFBE9E8 }))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().size(px(6.)).rounded_full().bg(rgb(if online {
                                        GREEN
                                    } else {
                                        RED
                                    }))),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(CODEX_UI.sidebar.item_font_size))
                                    .line_height(px(CODEX_UI.sidebar.item_line_height))
                                    .text_color(rgb(TEXT))
                                    .child(if online {
                                        "Local agent"
                                    } else {
                                        "Agent unavailable"
                                    }),
                            ),
                    ),
            )
    }

    fn render_center_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.selected_session.clone() {
            Some(id) => {
                let status = self.selected_status();
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.render_session_header(&id, status))
                    .child(self.render_transcript(cx))
                    .child(self.render_composer(cx))
            }
            None => self.render_new_session_form(cx),
        }
    }

    fn render_session_header(
        &mut self,
        id: &str,
        status: Option<SessionStatus>,
    ) -> impl IntoElement {
        let title = stable_task_title(id, &self.lines, self.has_older);
        let (status_label, status_color) = self.visible_status(status);
        div()
            .h(px(CODEX_UI.thread.header_height))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_4()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                svg()
                    .path("icons/phosphor-folder-simple.svg")
                    .size(px(16.))
                    .text_color(rgb(TEXT)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(CODEX_UI.thread.title_font_size))
                    .line_height(px(CODEX_UI.thread.title_line_height))
                    .font_weight(FontWeight::MEDIUM)
                    .child(truncate(&title, 72).to_owned()),
            )
            .child(
                div()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded(px(12.5))
                    .bg(rgb(PROMPT))
                    .child(div().size(px(6.)).rounded_full().bg(rgb(status_color)))
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(status_color))
                            .child(status_label),
                    ),
            )
    }

    fn render_transcript(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let lines: Rc<Vec<TranscriptLine>> = Rc::new(self.lines.clone());
        let item_count = lines.len();

        let list = gpui::list(self.transcript_list.clone(), move |ix, _window, _cx| {
            if ix < lines.len() {
                render_line(ix, &lines[ix])
            } else {
                div().into_any()
            }
        });

        let history = if item_count == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_size(px(14.))
                .text_color(rgb(SUBTLE))
                .child(if self.connection_error.is_some() {
                    "This task is temporarily unavailable"
                } else {
                    "Waiting for the first update…"
                })
                .into_any_element()
        } else {
            list.flex_1().min_h_0().into_any_element()
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().child(match (self.has_older, self.loading_older) {
                    (true, false) => div()
                        .py_3()
                        .flex()
                        .pl(px(CODEX_UI.thread.content_left_inset))
                        .child(
                            div()
                                .w(px(CODEX_UI.layout.transcript_max_width))
                                .max_w_full()
                                .flex()
                                .justify_center()
                                .child(
                                    div()
                                        .id("load-older")
                                        .px_3()
                                        .py_1()
                                        .rounded_lg()
                                        .text_size(px(12.))
                                        .text_color(rgb(DIM))
                                        .hover(|style| style.bg(rgb(PROMPT)))
                                        .on_click(cx.listener(move |v, _, _window, cx| {
                                            v.load_older(cx);
                                        }))
                                        .child("Load earlier activity")
                                        .automation(
                                            AutomationRole::Button,
                                            "Load earlier activity",
                                        ),
                                ),
                        )
                        .into_any_element(),
                    (_, true) => div()
                        .py_3()
                        .flex()
                        .pl(px(CODEX_UI.thread.content_left_inset))
                        .child(
                            div()
                                .w(px(CODEX_UI.layout.transcript_max_width))
                                .max_w_full()
                                .flex()
                                .justify_center()
                                .text_size(px(12.))
                                .text_color(rgb(DIM))
                                .child("Loading earlier activity…"),
                        )
                        .into_any_element(),
                    _ => Empty.into_any_element(),
                }),
            )
            .child(history)
            .child(self.render_live_activity())
    }

    fn render_composer(&mut self, cx: &mut Context<Self>) -> Div {
        self.render_composer_frame(ComposerMode::FollowUp, cx)
    }

    fn render_composer_frame(&mut self, mode: ComposerMode, cx: &mut Context<Self>) -> Div {
        let frame = div()
            .flex_shrink_0()
            .flex()
            .pb(px(CODEX_UI.layout.composer_bottom_inset));
        let frame = match mode {
            ComposerMode::NewTask => frame.justify_center().px_6(),
            ComposerMode::FollowUp => frame
                .justify_start()
                .pl(px(CODEX_UI.thread.content_left_inset))
                .pr_6(),
        };
        frame.child(self.render_composer_surface(mode, cx))
    }

    fn render_composer_surface(&mut self, mode: ComposerMode, cx: &mut Context<Self>) -> Div {
        let (profile_label, model_label, thinking_label) = self.selector_labels();
        let is_new_task = mode == ComposerMode::NewTask;
        let workspace = if is_new_task {
            self.workspace_input.as_str()
        } else {
            self.selected_session
                .as_deref()
                .and_then(|id| {
                    self.sessions
                        .iter()
                        .find(|session| session.session_id == id)
                })
                .map(|session| session.workspace.as_str())
                .unwrap_or_default()
        };
        let workspace_text = if workspace.is_empty() {
            "Choose workspace".to_owned()
        } else {
            short_path(workspace).to_owned()
        };
        let workspace_color = if workspace.is_empty() {
            rgba(0x1A1C1F7E)
        } else {
            rgb(CODEX_UI.composer.primary_text_color)
        };
        let is_working = !is_new_task && self.selected_status() == Some(SessionStatus::Working);
        let busy = self.creating || self.sending || self.canceling;
        let action_available = is_working || (!busy && self.current_selection().is_some());
        let action_bg = if action_available {
            CODEX_UI.composer.primary_text_color
        } else {
            0xB8B8B8
        };
        let action_icon = if is_working || self.canceling {
            "icons/phosphor-stop-fill.svg"
        } else {
            "icons/phosphor-arrow-up.svg"
        };
        let action_icon_size = if is_working || self.canceling {
            10.0
        } else {
            16.0
        };
        let action_label = if is_working || self.canceling {
            "Stop task"
        } else if is_new_task {
            "Start task"
        } else {
            "Send message"
        };
        let (feedback, feedback_color) = if self.canceling {
            ("Stopping…".to_owned(), DIM)
        } else if self.creating {
            ("Starting…".to_owned(), DIM)
        } else if self.sending {
            ("Sending…".to_owned(), DIM)
        } else if let Some(message) = self.error.as_ref().or(self.connection_error.as_ref()) {
            (truncate(message, 60).to_owned(), RED)
        } else if self.context_busy {
            ("Loading context setting…".to_owned(), DIM)
        } else if let Some(message) = &self.context_feedback {
            (message.clone(), DIM)
        } else if self.selection_dirty {
            ("Model change applies next message".to_owned(), AMBER)
        } else {
            (String::new(), SUBTLE)
        };

        let workspace_button = div()
            .h(px(CODEX_UI.composer.control_height))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .px_2()
            .rounded_full()
            .child(
                svg()
                    .path("icons/phosphor-folder-simple.svg")
                    .size(px(16.))
                    .text_color(rgb(CODEX_UI.composer.primary_text_color)),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(CODEX_UI.composer.control_font_size))
                    .line_height(px(18.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(workspace_color)
                    .child(truncate(&workspace_text, 42).to_owned()),
            );

        let workspace_tray = div()
            .id("workspace-input")
            .h(px(CODEX_UI.composer.workspace_tray_height))
            .relative()
            .top(px(CODEX_UI.composer.workspace_tray_top_inset))
            .mx(px(CODEX_UI.composer.workspace_tray_inline_inset))
            .flex()
            .flex_shrink_0()
            .flex_row()
            .items_start()
            .gap_2()
            .px(px(6.))
            .pt(px(6.))
            .pb(px(27.))
            .rounded_t(px(20.))
            .overflow_hidden()
            .bg(rgb(CODEX_UI.composer.project_surface_color))
            .child(workspace_button)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .h(px(CODEX_UI.composer.control_height))
                    .flex()
                    .items_center()
                    .text_size(px(CODEX_UI.composer.control_font_size))
                    .line_height(px(18.))
                    .text_color(rgb(feedback_color))
                    .child(feedback),
            );
        let workspace_tray = if is_new_task {
            workspace_tray
                .track_focus(&self.workspace_focus)
                .focus_visible(|style| style.bg(rgb(0xF1F6FE)))
                .on_click(cx.listener(move |v, _, window, cx| {
                    window.focus(&v.workspace_focus, cx);
                }))
                .on_key_down(cx.listener(move |v, e: &KeyDownEvent, _window, cx| {
                    v.workspace_key_down(e, cx);
                }))
        } else {
            workspace_tray
        };

        let action_id = if is_new_task {
            "start-session"
        } else {
            "send-button"
        };
        let action_button = div()
            .id(action_id)
            .size(px(CODEX_UI.composer.action_size))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(action_bg))
            .ml_1()
            .hover(|style| style.bg(rgb(if action_available { 0x303030 } else { 0xA8A8A8 })))
            .on_click(cx.listener(move |v, _, _window, cx| match mode {
                ComposerMode::NewTask => v.create_session(cx),
                ComposerMode::FollowUp if v.selected_status() == Some(SessionStatus::Working) => {
                    v.cancel_session(cx);
                }
                ComposerMode::FollowUp => v.send_composer(cx),
            }))
            .child(
                svg()
                    .path(action_icon)
                    .size(px(action_icon_size))
                    .text_color(rgb(BG)),
            );
        let open_kind = self.selector_menu.open();
        let open_menu = open_kind.map(|kind| self.render_selector_menu(kind, cx));

        div()
            .w(px(CODEX_UI.layout.composer_width))
            .max_w_full()
            .h(px(CODEX_UI.layout.composer_height))
            .flex()
            .flex_col()
            .child(workspace_tray.automation_when(
                is_new_task,
                true,
                AutomationRole::TextInput,
                "Workspace path",
            ))
            .child(
                div()
                    .h(px(CODEX_UI.composer.input_surface_height))
                    .relative()
                    .top(px(-CODEX_UI.composer.tray_overlap))
                    .flex()
                    .flex_shrink_0()
                    .flex_col()
                    .rounded(px(CODEX_UI.composer.surface_radius))
                    .bg(rgba(0xFFFFFFF5))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(0.), rgba(0x1A1C1F1E).into())
                            .spread_radius(px(0.5)),
                        BoxShadow::new(px(0.), px(3.), rgba(0x0000000A).into())
                            .blur_radius(px(7.5)),
                        BoxShadow::new(px(0.), px(0.), rgba(0x0000000D).into())
                            .blur_radius(px(20.)),
                        BoxShadow::new(px(0.), px(0.), rgb(0xFFFFFF).into()).spread_radius(px(0.5)),
                    ])
                    .child(div().h(px(14.)).flex_shrink_0())
                    .child(
                        div()
                            .id("composer-input")
                            .h(px(CODEX_UI.composer.editor_height))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .flex()
                            .items_start()
                            .px(px(CODEX_UI.composer.editor_horizontal_inset))
                            .mb_1()
                            .text_size(px(CODEX_UI.composer.placeholder_font_size))
                            .line_height(px(20.))
                            .text_color(rgb(CODEX_UI.composer.primary_text_color))
                            .child(self.composer_input.clone())
                            .automation(AutomationRole::TextInput, "Message composer"),
                    )
                    .child(
                        div()
                            .h(px(CODEX_UI.composer.control_height))
                            .flex()
                            .flex_shrink_0()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .mx_2()
                            .mb_2()
                            .child(selector_button(
                                cx,
                                SelectorKind::Profile,
                                &profile_label,
                                open_kind == Some(SelectorKind::Profile),
                            ))
                            .child(selector_button(
                                cx,
                                SelectorKind::Thinking,
                                &thinking_label,
                                open_kind == Some(SelectorKind::Thinking),
                            ))
                            .when(!is_new_task, |row| {
                                row.child(selector_button(
                                    cx,
                                    SelectorKind::Context,
                                    &self
                                        .context_config
                                        .as_ref()
                                        .map(|config| match config.strategy {
                                            ContextStrategy::Compaction => format!(
                                                "Summary · {}k",
                                                config.keep_recent_tokens as f64 / 1000.0
                                            ),
                                            ContextStrategy::Handoff => "Handoff".to_owned(),
                                        })
                                        .unwrap_or_else(|| "Context".into()),
                                    open_kind == Some(SelectorKind::Context),
                                ))
                            })
                            .child(div().flex_1())
                            .child(selector_button(
                                cx,
                                SelectorKind::Model,
                                &model_label,
                                open_kind == Some(SelectorKind::Model),
                            ))
                            .child(action_button.automation_enabled(
                                action_available,
                                AutomationRole::Button,
                                action_label,
                            )),
                    )
                    .when_some(open_menu, |surface, menu| surface.child(menu)),
            )
    }

    fn render_selector_menu(&self, kind: SelectorKind, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (options, selected, width) = match kind {
            SelectorKind::Context => (
                self.context_options()
                    .into_iter()
                    .map(|(label, _)| label)
                    .collect(),
                self.selected_selector_index(kind),
                280.0,
            ),
            SelectorKind::Profile => (
                self.profiles
                    .iter()
                    .map(|profile| {
                        profile
                            .profile_id
                            .rsplit('/')
                            .next()
                            .unwrap_or(&profile.profile_id)
                            .to_owned()
                    })
                    .collect::<Vec<_>>(),
                self.sel_profile,
                220.0,
            ),
            SelectorKind::Model => (
                self.profiles
                    .get(self.sel_profile)
                    .map(|profile| {
                        profile
                            .models
                            .iter()
                            .map(|model| model.id.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                self.sel_model,
                236.0,
            ),
            SelectorKind::Thinking => (
                self.current_model()
                    .map(|model| self.thinking_options(model))
                    .unwrap_or_default(),
                self.sel_thinking,
                172.0,
            ),
        };
        let slug = kind.slug();
        let highlighted = self.selector_menu.highlighted();
        let rows = options.into_iter().enumerate().map(|(index, label)| {
            let is_selected = selected == index;
            let is_highlighted = highlighted == Some(index);
            let automation_label = label.clone();
            div()
                .id(format!("selector-option-{slug}-{index}"))
                .h(px(30.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .px_2()
                .rounded(px(8.))
                .bg(if is_highlighted {
                    rgb(PROMPT)
                } else if is_selected {
                    rgb(SELECTED)
                } else {
                    rgb(BG)
                })
                .hover(|style| style.bg(rgb(PROMPT)))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.choose_selector(kind, index, window, cx);
                    cx.stop_propagation();
                }))
                .child(
                    div()
                        .w(px(14.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .when(is_selected, |marker| {
                            marker.child(div().size(px(5.)).rounded_full().bg(rgb(TEXT)))
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .text_color(rgb(TEXT))
                        .child(label),
                )
                .automation(AutomationRole::Option, automation_label)
        });

        let menu = div()
            .id(format!("selector-menu-{slug}"))
            .absolute()
            .bottom(px(42.))
            .w(px(width))
            .p_1()
            .rounded(px(12.))
            .bg(rgb(BG))
            .shadow(vec![
                BoxShadow::new(px(0.), px(8.), rgba(0x00000024).into()).blur_radius(px(24.)),
                BoxShadow::new(px(0.), px(0.), rgba(0x1A1C1F24).into()).spread_radius(px(0.5)),
            ])
            .child(
                div()
                    .id(format!("selector-menu-scroll-{slug}"))
                    .max_h(px(220.))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows)
                    .automation(AutomationRole::ScrollArea, format!("{slug} choices")),
            );
        match kind {
            SelectorKind::Context => menu.left(px(180.)).into_any_element(),
            SelectorKind::Profile => menu.left(px(8.)).into_any_element(),
            SelectorKind::Thinking => menu.left(px(104.)).into_any_element(),
            SelectorKind::Model => menu.right(px(44.)).into_any_element(),
        }
    }

    fn render_new_session_form(&mut self, cx: &mut Context<Self>) -> Div {
        let workspace = short_path(&self.workspace_input);
        let workspace = if workspace.is_empty() {
            "this project"
        } else {
            workspace
        };
        let heading = format!("What should we get done in {workspace}?");
        let cards = home_prompt_suggestions().into_iter().enumerate().map(
            |(ix, (label, prompt, icon, color))| {
                div()
                    .id(format!("home-suggestion-{ix}"))
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .px(px(CODEX_UI.home.card_padding_x))
                    .py(px(CODEX_UI.home.card_padding_y))
                    .rounded(px(CODEX_UI.home.card_radius))
                    .bg(rgb(BG))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(0.), rgba(0x1A1C1F1E).into())
                            .spread_radius(px(0.5)),
                        BoxShadow::new(px(0.), px(2.), rgba(0x0000001A).into()).blur_radius(px(4.)),
                    ])
                    .hover(|style| style.bg(rgb(0xFAFAFA)))
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.apply_home_suggestion(prompt, window, cx);
                    }))
                    .child(svg().path(icon).size(px(20.)).text_color(rgb(color)))
                    .child(
                        div()
                            .text_size(px(CODEX_UI.home.card_label_font_size))
                            .line_height(px(CODEX_UI.home.card_label_line_height))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(TEXT))
                            .child(label),
                    )
                    .automation(AutomationRole::Button, label)
            },
        );

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().h(px(CODEX_UI.layout.header_height)).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_end()
                            .pb(px(36.))
                            .child(
                                svg()
                                    .path("icons/phosphor-terminal-window.svg")
                                    .size(px(48.))
                                    .text_color(rgba(0x1A1C1F3F)),
                            )
                            .child(
                                div()
                                    .mt(px(24.))
                                    .text_size(px(CODEX_UI.home.heading_font_size))
                                    .line_height(px(CODEX_UI.home.heading_line_height))
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(rgb(TEXT))
                                    .child(heading),
                            ),
                    )
                    .child(
                        div()
                            .w(px(CODEX_UI.home.suggestion_grid_width))
                            .max_w_full()
                            .h(px(CODEX_UI.home.suggestion_grid_height))
                            .flex_shrink_0()
                            .flex()
                            .gap(px(CODEX_UI.home.suggestion_gap))
                            .children(cards),
                    )
                    .child(div().h(px(129.)).flex_shrink_0()),
            )
            .child(self.render_composer_frame(ComposerMode::NewTask, cx))
    }

    fn visible_status(&self, status: Option<SessionStatus>) -> (String, u32) {
        if !self.agent_online {
            return ("Agent offline".to_owned(), RED);
        }
        if let Some(activity) = self.activity.as_ref() {
            return (agent_status_label(activity), agent_status_color(activity));
        }
        match status {
            Some(SessionStatus::Working) => ("Working".to_owned(), GREEN),
            Some(SessionStatus::Wait) => ("Ready".to_owned(), DIM),
            None => ("Loading".to_owned(), DIM),
        }
    }

    fn render_live_activity(&self) -> Div {
        let Some(status) = self.activity.as_ref() else {
            return div();
        };
        if !should_render_live_activity(status, self.lines.last()) {
            return div();
        }
        let label = agent_status_label(status);
        let color = agent_status_color(status);
        div()
            .flex_shrink_0()
            .flex()
            .justify_start()
            .pl(px(CODEX_UI.thread.content_left_inset))
            .pr_6()
            .pb_3()
            .child(
                div()
                    .w(px(CODEX_UI.layout.transcript_max_width))
                    .max_w_full()
                    .border_l_1()
                    .border_color(rgb(BORDER_STRONG))
                    .pl_3()
                    .text_size(px(12.))
                    .text_color(rgb(color))
                    .child(label),
            )
    }
}

// ------------------------------------------------------------- line render

fn selector_button(
    cx: &Context<RootView>,
    kind: SelectorKind,
    label: &str,
    menu_open: bool,
) -> impl IntoElement {
    let kind_slug = kind.slug();
    let icon_path = match kind {
        SelectorKind::Profile => "icons/phosphor-terminal-window.svg",
        SelectorKind::Model => "icons/phosphor-cube.svg",
        SelectorKind::Thinking => "icons/phosphor-brain.svg",
        SelectorKind::Context => "icons/phosphor-cube.svg",
    };

    div()
        .id(format!("selector-{kind_slug}"))
        .h(px(CODEX_UI.composer.control_height))
        .flex()
        .items_center()
        .gap_1()
        .when(kind == SelectorKind::Model, |button| button.px_2())
        .when(kind != SelectorKind::Model, |button| button.px(px(6.)))
        .rounded_lg()
        .text_size(px(CODEX_UI.composer.control_font_size))
        .line_height(px(18.))
        .text_color(rgba(0x1A1C1F7E))
        .when(menu_open, |button| {
            button.bg(rgb(PROMPT)).text_color(rgb(TEXT))
        })
        .hover(|style| style.bg(rgb(PROMPT)).text_color(rgb(TEXT)))
        .on_click(cx.listener(move |v, _, window, cx| {
            v.toggle_selector_menu(kind, window, cx);
            cx.stop_propagation();
        }))
        .child(
            svg()
                .path(icon_path)
                .size(px(16.))
                .text_color(rgba(0x1A1C1F7E)),
        )
        .child(label.to_owned())
        .when(kind == SelectorKind::Model, |button| {
            button.child(
                svg()
                    .path("icons/phosphor-caret-down.svg")
                    .size(px(14.))
                    .text_color(rgba(0x1A1C1F7E)),
            )
        })
        .automation(AutomationRole::Button, format!("{label} selector"))
}

fn home_prompt_suggestions() -> [(&'static str, &'static str, &'static str, u32); 4] {
    [
        (
            "Explore and understand the code",
            "Explore this codebase and explain its architecture, important flows, and likely risks.",
            "icons/phosphor-terminal-window.svg",
            0x2388FF,
        ),
        (
            "Build a new feature",
            "Inspect the existing patterns, then design and implement the next useful feature end to end.",
            "icons/phosphor-cube.svg",
            0xA63CFF,
        ),
        (
            "Review recent changes",
            "Review the recent code changes for correctness, regressions, maintainability, and missing tests.",
            "icons/phosphor-brain.svg",
            0x0DAA5B,
        ),
        (
            "Organize or fix the project",
            "Find the most important broken or confusing part of this project and fix it with regression coverage.",
            "icons/phosphor-folder-simple.svg",
            0xF05A16,
        ),
    ]
}

fn render_line(index: usize, line: &TranscriptLine) -> gpui::AnyElement {
    match line {
        TranscriptLine::Message { role, content } => match role {
            Role::User => transcript_row()
                .flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(CODEX_UI.layout.transcript_max_width
                            * CODEX_UI.thread.user_max_width_ratio))
                        .max_w_full()
                        .rounded(px(CODEX_UI.thread.user_radius))
                        .bg(rgb(CODEX_UI.thread.user_fill))
                        .px(px(CODEX_UI.thread.user_padding_x))
                        .py(px(CODEX_UI.thread.user_padding_y))
                        .text_size(px(CODEX_UI.thread.user_font_size))
                        .line_height(px(CODEX_UI.thread.user_line_height))
                        .child(content.clone()),
                )
                .into_any(),
            Role::Assistant => transcript_row()
                .child(
                    div()
                        .text_size(px(CODEX_UI.thread.assistant_font_size))
                        .line_height(px(CODEX_UI.thread.assistant_line_height))
                        .child(render_markdown(
                            &format!("assistant-message-{index}"),
                            content,
                        )),
                )
                .into_any(),
        },
    }
}

fn transcript_row() -> Div {
    div()
        .w(px(
            CODEX_UI.layout.transcript_max_width + CODEX_UI.thread.content_left_inset
        ))
        .max_w_full()
        .pl(px(CODEX_UI.thread.content_left_inset))
        .py_3()
}

fn agent_status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::Clear => "wait".to_owned(),
        AgentStatus::Thinking => "thinking".to_owned(),
        AgentStatus::ToolsStarted { calls } if calls.is_empty() => "tools".to_owned(),
        AgentStatus::ToolsStarted { calls } => format!(
            "tools · {}",
            calls
                .iter()
                .map(|call| call.tool_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AgentStatus::ToolFinished { tool_call_id } => {
            format!("tool finished · {}", truncate(tool_call_id, 16))
        }
        AgentStatus::Waiting { reason, .. } => format!("wait · {reason}"),
        AgentStatus::Failed { reason } => format!("failed · {reason}"),
        AgentStatus::Finished => "finished".to_owned(),
        AgentStatus::Interrupted => "interrupted".to_owned(),
    }
}

fn agent_status_color(status: &AgentStatus) -> u32 {
    match status {
        AgentStatus::Failed { .. } => RED,
        AgentStatus::Waiting { .. } | AgentStatus::Clear | AgentStatus::Interrupted => AMBER,
        AgentStatus::Thinking
        | AgentStatus::ToolsStarted { .. }
        | AgentStatus::ToolFinished { .. }
        | AgentStatus::Finished => GREEN,
    }
}

fn validate_new_task_inputs(
    workspace: &str,
    prompt: &str,
) -> Result<(String, String), &'static str> {
    let workspace = workspace.trim();
    if workspace.is_empty() {
        return Err("workspace path is required");
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("describe the task first");
    }
    Ok((workspace.to_owned(), prompt.to_owned()))
}

fn truncate(text: &str, max_chars: usize) -> &str {
    let boundary = text
        .char_indices()
        .nth(max_chars)
        .map(|(byte_ix, _)| byte_ix)
        .unwrap_or(text.len());
    &text[..boundary]
}

fn short_path(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn should_adopt_session_workspace(current: &str) -> bool {
    matches!(current.trim(), "" | "." | "/")
}

fn short_id(id: &str) -> &str {
    truncate(id, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_agent_internal_transcript_event_names() {
        for name in ["wait", "assistant_delta", "tool"] {
            let decoded = decode_sse_event(&SseEvent {
                name: name.to_owned(),
                data: "{}".to_owned(),
            })
            .expect("unknown internal events are ignored without parsing");
            assert_eq!(decoded, None);
        }
    }

    #[test]
    fn decodes_and_labels_rich_tool_status() {
        let decoded = decode_sse_event(&SseEvent {
            name: "status".to_owned(),
            data: r#"{"state":"tools_started","calls":[{"tool_call_id":"call-1","tool_name":"read_file"}]}"#
                .to_owned(),
        })
        .expect("valid status payload")
        .expect("known event");

        let DecodedSseEvent::Status(status) = decoded else {
            panic!("expected status event");
        };
        assert_eq!(agent_status_label(&status), "tools · read_file");
        assert_eq!(agent_status_color(&status), GREEN);
    }

    #[test]
    fn truncates_at_the_requested_character_count() {
        assert_eq!(truncate("你好世界", 2), "你好");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn a_new_task_requires_both_workspace_and_prompt() {
        assert_eq!(
            validate_new_task_inputs("", "Build the feature"),
            Err("workspace path is required")
        );
        assert_eq!(
            validate_new_task_inputs("/workspace", "  "),
            Err("describe the task first")
        );
        assert_eq!(
            validate_new_task_inputs(" /workspace ", " Build it "),
            Ok(("/workspace".to_owned(), "Build it".to_owned()))
        );
    }

    #[test]
    fn home_suggestions_are_real_prompts_with_library_icons() {
        let suggestions = home_prompt_suggestions();

        assert_eq!(suggestions.len(), 4);
        assert!(suggestions.iter().all(|(label, prompt, icon, _)| {
            !label.is_empty()
                && !prompt.is_empty()
                && icon.starts_with("icons/phosphor-")
                && icon.ends_with(".svg")
        }));
    }

    #[test]
    fn app_bundle_root_cwd_adopts_the_agents_real_workspace() {
        assert!(should_adopt_session_workspace("/"));
        assert!(should_adopt_session_workspace("."));
        assert!(should_adopt_session_workspace(""));
        assert!(!should_adopt_session_workspace("/workspace/open-worker"));
    }
}
