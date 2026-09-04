//! Testable visual contract for the Codex-style native shell.
//!
//! Rendering stays in `views`, but the durable palette, hierarchy, and default
//! geometry live here so regressions do not silently turn the app back into a
//! generic dashboard.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptTreatment {
    PromptPill,
    PlainProse,
    InlineActivity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub canvas: u32,
    pub sidebar: u32,
    pub sidebar_hover: u32,
    pub selected: u32,
    pub elevated: u32,
    pub prompt: u32,
    pub border: u32,
    pub border_strong: u32,
    pub text: u32,
    pub muted: u32,
    pub subtle: u32,
    pub accent: u32,
    pub success: u32,
    pub warning: u32,
    pub danger: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutSpec {
    pub sidebar_width: f32,
    pub header_height: f32,
    pub transcript_max_width: f32,
    pub composer_width: f32,
    pub composer_height: f32,
    pub composer_bottom_inset: f32,
    pub has_global_status_bar: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedGeometry {
    pub sidebar: Rect,
    pub main: Rect,
    pub header: Rect,
    pub transcript: Rect,
    pub composer: Rect,
}

impl LayoutSpec {
    pub fn resolve(self, width: f32, height: f32) -> Option<ResolvedGeometry> {
        if width < 900.0 || height < 600.0 {
            return None;
        }

        let main = Rect {
            left: self.sidebar_width,
            top: 0.0,
            right: width,
            bottom: height,
        };
        let sidebar = Rect {
            left: 0.0,
            top: 0.0,
            right: self.sidebar_width,
            bottom: height,
        };
        let header = Rect {
            left: main.left,
            top: 0.0,
            right: main.right,
            bottom: self.header_height,
        };
        let main_width = main.right - main.left;
        let composer_width = self.composer_width.min(main_width - 48.0);
        let composer_left = main.left + (main_width - composer_width) / 2.0;
        let composer = Rect {
            left: composer_left,
            top: height - self.composer_bottom_inset - self.composer_height,
            right: composer_left + composer_width,
            bottom: height - self.composer_bottom_inset,
        };
        let transcript_width = self.transcript_max_width.min(main_width - 64.0);
        let transcript_left = main.left + (main_width - transcript_width) / 2.0;
        let transcript = Rect {
            left: transcript_left,
            top: header.bottom + 28.0,
            right: transcript_left + transcript_width,
            bottom: composer.top - 20.0,
        };

        if transcript.bottom <= transcript.top {
            return None;
        }

        Some(ResolvedGeometry {
            sidebar,
            main,
            header,
            transcript,
            composer,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptSpec {
    pub user: TranscriptTreatment,
    pub assistant: TranscriptTreatment,
    pub activity: TranscriptTreatment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerActionShape {
    Circle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComposerSpec {
    pub fixed_to_bottom: bool,
    pub multiline: bool,
    pub controls_inside_surface: bool,
    pub send_becomes_stop: bool,
    pub visible_keyboard_focus: bool,
    pub workspace_tray_height: f32,
    pub input_surface_height: f32,
    pub tray_overlap: f32,
    pub workspace_tray_inline_inset: f32,
    pub workspace_tray_top_inset: f32,
    pub surface_radius: f32,
    pub editor_height: f32,
    pub editor_horizontal_inset: f32,
    pub control_height: f32,
    pub action_size: f32,
    pub placeholder_font_size: f32,
    pub control_font_size: f32,
    pub project_surface_color: u32,
    pub primary_text_color: u32,
    pub surface_has_prominent_shadow: bool,
    pub has_hard_divider: bool,
    pub compact_split_controls: bool,
    pub shared_new_and_followup_structure: bool,
    pub action_shape: ComposerActionShape,
    pub action_uses_icon_asset: bool,
    pub model_selector_uses_icon_asset: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SidebarSpec {
    pub toolbar_height: f32,
    pub footer_height: f32,
    pub inline_inset: f32,
    pub row_height: f32,
    pub row_radius: f32,
    pub item_font_size: f32,
    pub item_line_height: f32,
    pub section_label_font_size: f32,
    pub section_label_line_height: f32,
    pub section_label_weight: u16,
    pub selected_fill: u32,
    pub has_hard_divider: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HomeSpec {
    pub heading_font_size: f32,
    pub heading_line_height: f32,
    pub heading_weight: u16,
    pub suggestion_grid_width: f32,
    pub suggestion_grid_height: f32,
    pub suggestion_gap: f32,
    pub suggestion_count: usize,
    pub card_radius: f32,
    pub card_padding_x: f32,
    pub card_padding_y: f32,
    pub card_label_font_size: f32,
    pub card_label_line_height: f32,
    pub card_label_weight: u16,
    pub suggestions_fill_composer: bool,
    pub uses_real_icon_assets: bool,
    pub counterfeits_codex_mark: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThreadSpec {
    pub header_height: f32,
    pub title_font_size: f32,
    pub title_line_height: f32,
    pub title_weight: u16,
    pub assistant_font_size: f32,
    pub assistant_line_height: f32,
    pub content_left_inset: f32,
    pub aligns_followup_composer: bool,
    pub user_font_size: f32,
    pub user_line_height: f32,
    pub user_max_width_ratio: f32,
    pub user_padding_x: f32,
    pub user_padding_y: f32,
    pub user_radius: f32,
    pub user_fill: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskRowSpec {
    pub group_by_workspace: bool,
    pub selected_uses_fill: bool,
    pub draw_card_borders: bool,
    pub show_model_suffix: bool,
    pub show_leading_status_dot: bool,
    pub workspace_uses_icon_asset: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CodexUiSpec {
    pub force_light_window_chrome: bool,
    pub palette: Palette,
    pub layout: LayoutSpec,
    pub transcript: TranscriptSpec,
    pub composer: ComposerSpec,
    pub sidebar: SidebarSpec,
    pub home: HomeSpec,
    pub thread: ThreadSpec,
    pub task_rows: TaskRowSpec,
}

pub const CODEX_UI: CodexUiSpec = CodexUiSpec {
    force_light_window_chrome: true,
    palette: Palette {
        canvas: 0xFFFFFF,
        sidebar: 0xFBFBFB,
        sidebar_hover: 0xF2F2F2,
        selected: 0xF2F2F2,
        elevated: 0xFFFFFF,
        prompt: 0xF2F2F2,
        border: 0xE7E7E7,
        border_strong: 0xD4D4D4,
        text: 0x1A1C1F,
        muted: 0x737373,
        subtle: 0xA3A3A3,
        accent: 0x2F6FEB,
        success: 0x168A55,
        warning: 0xA16207,
        danger: 0xC2413B,
    },
    layout: LayoutSpec {
        sidebar_width: 275.0,
        header_height: 46.0,
        transcript_max_width: 736.0,
        composer_width: 736.0,
        composer_height: 141.0,
        composer_bottom_inset: 16.0,
        has_global_status_bar: false,
    },
    transcript: TranscriptSpec {
        user: TranscriptTreatment::PromptPill,
        assistant: TranscriptTreatment::PlainProse,
        activity: TranscriptTreatment::InlineActivity,
    },
    composer: ComposerSpec {
        fixed_to_bottom: true,
        multiline: true,
        controls_inside_surface: true,
        send_becomes_stop: true,
        visible_keyboard_focus: true,
        workspace_tray_height: 61.0,
        input_surface_height: 98.0,
        tray_overlap: 18.0,
        workspace_tray_inline_inset: 13.0,
        workspace_tray_top_inset: 4.0,
        surface_radius: 25.0,
        editor_height: 44.0,
        editor_horizontal_inset: 12.0,
        control_height: 28.0,
        action_size: 28.0,
        placeholder_font_size: 14.0,
        control_font_size: 13.0,
        project_surface_color: 0xF6F6F6,
        primary_text_color: 0x1A1C1F,
        surface_has_prominent_shadow: true,
        has_hard_divider: false,
        compact_split_controls: true,
        shared_new_and_followup_structure: true,
        action_shape: ComposerActionShape::Circle,
        action_uses_icon_asset: true,
        model_selector_uses_icon_asset: true,
    },
    sidebar: SidebarSpec {
        toolbar_height: 46.0,
        footer_height: 46.0,
        inline_inset: 8.0,
        row_height: 30.0,
        row_radius: 12.5,
        item_font_size: 14.0,
        item_line_height: 21.0,
        section_label_font_size: 14.0,
        section_label_line_height: 21.0,
        section_label_weight: 500,
        selected_fill: 0xF2F2F2,
        has_hard_divider: false,
    },
    home: HomeSpec {
        heading_font_size: 28.0,
        heading_line_height: 33.6,
        heading_weight: 400,
        suggestion_grid_width: 710.0,
        suggestion_grid_height: 104.0,
        suggestion_gap: 12.0,
        suggestion_count: 4,
        card_radius: 20.0,
        card_padding_x: 16.0,
        card_padding_y: 12.0,
        card_label_font_size: 13.0,
        card_label_line_height: 20.0,
        card_label_weight: 500,
        suggestions_fill_composer: true,
        uses_real_icon_assets: true,
        counterfeits_codex_mark: false,
    },
    thread: ThreadSpec {
        header_height: 46.0,
        title_font_size: 14.0,
        title_line_height: 24.0,
        title_weight: 500,
        assistant_font_size: 14.0,
        assistant_line_height: 22.0,
        content_left_inset: 76.0,
        aligns_followup_composer: true,
        user_font_size: 16.0,
        user_line_height: 24.0,
        user_max_width_ratio: 0.77,
        user_padding_x: 12.0,
        user_padding_y: 8.0,
        user_radius: 20.0,
        user_fill: 0xF2F2F2,
    },
    task_rows: TaskRowSpec {
        group_by_workspace: true,
        selected_uses_fill: true,
        draw_card_borders: false,
        show_model_suffix: false,
        show_leading_status_dot: false,
        workspace_uses_icon_asset: true,
    },
};

pub fn codex_ui_spec() -> CodexUiSpec {
    CODEX_UI
}
