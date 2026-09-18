mod codex_usage;
mod metrics;
mod token_usage;

use std::{
    env, fs,
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use codex_usage::{
    CodexActionKind, CodexActivity, CodexControl, CodexServiceTier, CodexUsageContent,
    CodexUsagePoller, CodexUsageState, CodexUsageStatus, QuotaBucket, QuotaWindow, ResetCredit,
};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect,
    RichText, Sense, Stroke, StrokeKind, TextStyle, Vec2,
};
use metrics::{MetricsSampler, Snapshot};
use serde::{Deserialize, Serialize};
use token_usage::{TokenUsageSampler, TokenUsageState};

const APP_TITLE: &str = "システムモニター";
const PREFERENCES_STORAGE_KEY: &str = "mini-system-monitor-rs.ui-preferences.v1";
const UI_FONT_SIZE_MIN_POINTS: u8 = 10;
const UI_FONT_SIZE_MAX_POINTS: u8 = 32;
const UI_FONT_SIZE_DEFAULT_POINTS: u8 = 16;
const FULL_WINDOW_SIZE: [f32; 2] = [480.0, 448.0];
const FULL_MIN_WINDOW_SIZE: [f32; 2] = [450.0, 428.0];
const COMPACT_WINDOW_SIZE: [f32; 2] = [480.0, 188.0];
const COMPACT_MIN_WINDOW_SIZE: [f32; 2] = [430.0, 168.0];
const CODEX_WINDOW_SIZE: [f32; 2] = [640.0, 820.0];
const CODEX_MIN_WINDOW_SIZE: [f32; 2] = [560.0, 640.0];
const CODEX_WINDOW_GAP: f32 = 12.0;
const CODEX_SCREEN_MARGIN: f32 = 8.0;
const CODEX_WINDOW_CHROME_ESTIMATE: [f32; 2] = [0.0, 40.0];
const RESET_CREDIT_LIST_MAX_HEIGHT: f32 = 300.0;
const JAPANESE_FONT_NAME: &str = "system_japanese";
const JAPANESE_FONT_PATHS: &[&str] = &[
    "/System/Library/Fonts/Hiragino Sans.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W5.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W6.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
    "/Library/Fonts/Osaka.ttf",
];

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum LanguageChoice {
    #[default]
    System,
    Ja,
    En,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    fn egui_preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct UiPreferences {
    language: LanguageChoice,
    theme: ThemeChoice,
    font_size_points: u8,
    auto_reset: bool,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            language: LanguageChoice::System,
            theme: ThemeChoice::System,
            font_size_points: UI_FONT_SIZE_DEFAULT_POINTS,
            auto_reset: false,
        }
    }
}

impl UiPreferences {
    fn from_json(json: &str) -> Option<Self> {
        let mut preferences: Self = serde_json::from_str(json).ok()?;
        preferences.font_size_points = preferences
            .font_size_points
            .clamp(UI_FONT_SIZE_MIN_POINTS, UI_FONT_SIZE_MAX_POINTS);
        Some(preferences)
    }

    fn zoom_factor(self) -> f32 {
        f32::from(self.font_size_points) / f32::from(UI_FONT_SIZE_DEFAULT_POINTS)
    }
}

fn increment_ui_font_size(points: u8) -> u8 {
    points.saturating_add(1).min(UI_FONT_SIZE_MAX_POINTS)
}

fn decrement_ui_font_size(points: u8) -> u8 {
    points.saturating_sub(1).max(UI_FONT_SIZE_MIN_POINTS)
}

fn commit_ui_font_size(applied_points: &mut u8, draft_points: u8) -> bool {
    let next_points = draft_points.clamp(UI_FONT_SIZE_MIN_POINTS, UI_FONT_SIZE_MAX_POINTS);
    let changed = *applied_points != next_points;
    *applied_points = next_points;
    changed
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Language {
    Japanese,
    English,
}

impl LanguageChoice {
    fn resolve(self, system_language: Language) -> Language {
        match self {
            Self::System => system_language,
            Self::Ja => Language::Japanese,
            Self::En => Language::English,
        }
    }
}

fn language_from_locale(locale: &str) -> Option<Language> {
    locale
        .split(|character: char| !character.is_ascii_alphabetic())
        .find(|part| !part.is_empty())
        .and_then(|code| match code.to_ascii_lowercase().as_str() {
            "ja" => Some(Language::Japanese),
            "en" => Some(Language::English),
            _ => None,
        })
}

fn detect_system_language() -> Language {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .find_map(|locale| language_from_locale(&locale))
        .or_else(|| {
            Command::new("defaults")
                .args(["read", "-g", "AppleLanguages"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|locale| language_from_locale(&locale))
        })
        .unwrap_or(Language::Japanese)
}

#[derive(Clone, Copy)]
struct Palette {
    accent_green: Color32,
    panel_bg: Color32,
    panel_stroke: Color32,
    card_bg: Color32,
    card_stroke: Color32,
    track_bg: Color32,
    text_main: Color32,
    text_muted: Color32,
    text_subtle: Color32,
    codex_accent: Color32,
    token_accent: Color32,
    warning_amber: Color32,
    error_red: Color32,
    extreme_bg: Color32,
}

impl Palette {
    fn for_theme(theme: egui::Theme) -> Self {
        match theme {
            egui::Theme::Dark => Self {
                accent_green: Color32::from_rgb(78, 210, 132),
                panel_bg: Color32::from_rgba_premultiplied(17, 19, 23, 246),
                panel_stroke: Color32::from_rgba_premultiplied(255, 255, 255, 14),
                card_bg: Color32::from_rgba_premultiplied(27, 30, 36, 238),
                card_stroke: Color32::from_rgba_premultiplied(255, 255, 255, 12),
                track_bg: Color32::from_rgba_premultiplied(255, 255, 255, 20),
                text_main: Color32::from_rgb(238, 243, 240),
                text_muted: Color32::from_rgb(151, 161, 156),
                text_subtle: Color32::from_rgb(104, 115, 110),
                codex_accent: Color32::from_rgb(91, 159, 255),
                token_accent: Color32::from_rgb(246, 190, 82),
                warning_amber: Color32::from_rgb(238, 170, 83),
                error_red: Color32::from_rgb(236, 100, 95),
                extreme_bg: Color32::from_rgb(12, 14, 17),
            },
            egui::Theme::Light => Self {
                accent_green: Color32::from_rgb(31, 143, 82),
                panel_bg: Color32::from_rgba_premultiplied(247, 249, 246, 246),
                panel_stroke: Color32::from_rgba_premultiplied(32, 41, 36, 28),
                card_bg: Color32::from_rgba_premultiplied(255, 255, 255, 238),
                card_stroke: Color32::from_rgba_premultiplied(32, 41, 36, 24),
                track_bg: Color32::from_rgba_premultiplied(32, 41, 36, 28),
                text_main: Color32::from_rgb(27, 35, 31),
                text_muted: Color32::from_rgb(94, 106, 100),
                text_subtle: Color32::from_rgb(128, 139, 133),
                codex_accent: Color32::from_rgb(34, 103, 198),
                token_accent: Color32::from_rgb(169, 111, 14),
                warning_amber: Color32::from_rgb(177, 112, 30),
                error_red: Color32::from_rgb(196, 61, 57),
                extreme_bg: Color32::from_rgb(240, 243, 239),
            },
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(FULL_WINDOW_SIZE)
            .with_min_inner_size(FULL_MIN_WINDOW_SIZE)
            .with_resizable(false)
            .with_transparent(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop),
        ..Default::default()
    };

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(MonitorApp::new(cc)))),
    )
}

struct MonitorApp {
    snapshot: Snapshot,
    rx: Receiver<Snapshot>,
    last_update: Instant,
    codex_usage: CodexUsageState,
    codex_rx: Receiver<CodexUsageState>,
    token_usage: Option<TokenUsageState>,
    token_rx: Receiver<TokenUsageState>,
    codex_control_tx: Sender<CodexControl>,
    codex_details_open: bool,
    codex_details_position: Option<Pos2>,
    codex_details_needs_exact_position: bool,
    codex_details_resize_pending: bool,
    display_mode: DisplayMode,
    preferences: UiPreferences,
    draft_font_size_points: u8,
    system_language: Language,
    display_resize_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisplayMode {
    Full,
    Compact,
}

impl DisplayMode {
    fn toggled(self) -> Self {
        match self {
            Self::Full => Self::Compact,
            Self::Compact => Self::Full,
        }
    }

    fn size(self) -> Vec2 {
        let [width, height] = match self {
            Self::Full => FULL_WINDOW_SIZE,
            Self::Compact => COMPACT_WINDOW_SIZE,
        };
        Vec2::new(width, height)
    }

    fn min_size(self) -> Vec2 {
        let [width, height] = match self {
            Self::Full => FULL_MIN_WINDOW_SIZE,
            Self::Compact => COMPACT_MIN_WINDOW_SIZE,
        };
        Vec2::new(width, height)
    }

    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Full, Language::Japanese) => "標準",
            (Self::Compact, Language::Japanese) => "コンパクト",
            (Self::Full, Language::English) => "Full",
            (Self::Compact, Language::English) => "Compact",
        }
    }
}

impl MonitorApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        register_japanese_font(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);

        let system_language = detect_system_language();
        let preferences = cc
            .storage
            .and_then(|storage| storage.get_string(PREFERENCES_STORAGE_KEY))
            .as_deref()
            .and_then(UiPreferences::from_json)
            .unwrap_or_default();
        let draft_font_size_points = preferences.font_size_points;
        apply_preferences(&cc.egui_ctx, preferences, system_language);
        let display_resize_pending =
            (preferences.zoom_factor() - cc.egui_ctx.zoom_factor()).abs() > f32::EPSILON;

        let (snapshot, rx) = start_metrics_sampler();
        let (codex_control_tx, codex_rx) = start_codex_usage_sampler(preferences.auto_reset);

        Self {
            snapshot,
            rx,
            last_update: Instant::now(),
            codex_usage: CodexUsageState::loading(),
            codex_rx,
            token_usage: None,
            token_rx: start_token_usage_sampler(),
            codex_control_tx,
            codex_details_open: false,
            codex_details_position: None,
            codex_details_needs_exact_position: false,
            codex_details_resize_pending: false,
            display_mode: DisplayMode::Full,
            preferences,
            draft_font_size_points,
            system_language,
            display_resize_pending,
        }
    }

    fn receive_latest_snapshot(&mut self) {
        while let Ok(snapshot) = self.rx.try_recv() {
            self.snapshot = snapshot;
            self.last_update = Instant::now();
        }
    }

    fn receive_latest_codex_usage(&mut self) {
        while let Ok(codex_usage) = self.codex_rx.try_recv() {
            self.codex_usage = codex_usage;
        }
        while let Ok(token_usage) = self.token_rx.try_recv() {
            self.token_usage = Some(token_usage);
        }
    }

    fn open_codex_details(&mut self) {
        self.codex_details_open = true;
        self.codex_details_position = None;
        self.codex_details_needs_exact_position = true;
    }

    fn close_codex_details(&mut self) {
        self.codex_details_open = false;
        self.codex_details_position = None;
        self.codex_details_needs_exact_position = false;
        self.codex_details_resize_pending = false;
    }

    fn toggle_codex_details(&mut self) {
        if self.codex_details_open {
            self.close_codex_details();
        } else {
            self.open_codex_details();
        }
    }

    fn draw_codex_details_window(&mut self, ctx: &egui::Context, language: Language) {
        if !self.codex_details_open {
            return;
        }

        let parent_rect = ctx.input(|input| input.viewport().outer_rect);
        let screen_rect = parent_rect.and_then(|parent_rect| {
            visible_screen_rects(ctx.zoom_factor())
                .into_iter()
                .max_by(|left, right| compare_screens_for_parent(*left, *right, parent_rect))
        });
        let chrome_estimate = Vec2::new(
            CODEX_WINDOW_CHROME_ESTIMATE[0],
            CODEX_WINDOW_CHROME_ESTIMATE[1] / ctx.zoom_factor().max(0.01),
        );
        let desired_inner_size = screen_rect
            .map(|screen_rect| {
                let available_width = (screen_rect.width() - CODEX_SCREEN_MARGIN * 2.0).max(1.0);
                let available_height =
                    (screen_rect.height() - chrome_estimate.y - CODEX_SCREEN_MARGIN * 2.0).max(1.0);
                Vec2::new(
                    CODEX_WINDOW_SIZE[0].min(available_width),
                    CODEX_WINDOW_SIZE[1].min(available_height),
                )
            })
            .unwrap_or_else(|| Vec2::new(CODEX_WINDOW_SIZE[0], CODEX_WINDOW_SIZE[1]));
        let minimum_inner_size = Vec2::new(
            CODEX_MIN_WINDOW_SIZE[0].min(desired_inner_size.x),
            CODEX_MIN_WINDOW_SIZE[1].min(desired_inner_size.y),
        );
        if self.codex_details_position.is_none()
            && let (Some(parent_rect), Some(screen_rect)) = (parent_rect, screen_rect)
        {
            let estimated_outer_size = desired_inner_size + chrome_estimate;
            self.codex_details_position = Some(adjacent_window_position(
                parent_rect,
                estimated_outer_size,
                screen_rect,
            ));
        }

        let state = self.codex_usage.clone();
        let token_usage = self.token_usage.clone();
        let auto_reset = self.preferences.auto_reset;
        let mut open = true;
        let mut close_requested = false;
        let mut actions = Vec::new();
        let mut measured_outer_size = None;
        let resize_ready = self.codex_details_resize_pending
            && (ctx.zoom_factor() - self.preferences.zoom_factor()).abs() < 0.001;
        let needs_exact_position =
            self.codex_details_needs_exact_position && !self.codex_details_resize_pending;
        let viewport_id = egui::ViewportId::from_hash_of("codex_management_window");
        let title = match language {
            Language::Japanese => "Codex 管理",
            Language::English => "Codex controls",
        };
        let mut viewport_builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(desired_inner_size)
            .with_min_inner_size(minimum_inner_size)
            .with_resizable(true)
            .with_window_level(egui::WindowLevel::AlwaysOnTop);
        if let Some(position) = self.codex_details_position {
            viewport_builder = viewport_builder.with_position(position);
        }

        ctx.show_viewport_immediate(viewport_id, viewport_builder, |ui, _class| {
            if ui.ctx().input(|input| input.viewport().close_requested()) {
                open = false;
            }
            if needs_exact_position {
                measured_outer_size = ui
                    .ctx()
                    .input(|input| input.viewport().outer_rect.map(|rect| rect.size()));
            }
            draw_codex_management(
                ui,
                &state,
                token_usage.as_ref(),
                auto_reset,
                language,
                &mut actions,
                &mut close_requested,
            );
        });

        if resize_ready {
            ctx.send_viewport_cmd_to(
                viewport_id,
                egui::ViewportCommand::MinInnerSize(minimum_inner_size),
            );
            ctx.send_viewport_cmd_to(
                viewport_id,
                egui::ViewportCommand::InnerSize(desired_inner_size),
            );
            self.codex_details_resize_pending = false;
        }

        if close_requested {
            open = false;
        }

        if let (Some(parent_rect), Some(screen_rect), Some(outer_size)) =
            (parent_rect, screen_rect, measured_outer_size)
        {
            let exact_position = adjacent_window_position(parent_rect, outer_size, screen_rect);
            ctx.send_viewport_cmd_to(
                viewport_id,
                egui::ViewportCommand::OuterPosition(exact_position),
            );
            self.codex_details_position = Some(exact_position);
            self.codex_details_needs_exact_position = false;
        }

        if open {
            self.codex_details_open = true;
        } else {
            self.close_codex_details();
        }
        for action in actions {
            let control = match action {
                CodexUiAction::Refresh => {
                    self.codex_usage.begin(CodexActionKind::Refresh);
                    CodexControl::Refresh
                }
                CodexUiAction::SetAutoReset(enabled) => {
                    self.preferences.auto_reset = enabled;
                    self.codex_usage.begin(CodexActionKind::AutoReset);
                    CodexControl::SetAutoReset(enabled)
                }
                CodexUiAction::SetServiceTier(service_tier) => {
                    self.codex_usage.begin(CodexActionKind::ServiceTier);
                    CodexControl::SetServiceTier(service_tier)
                }
            };

            if self.codex_control_tx.send(control).is_err() {
                self.codex_usage.activity = Some(CodexActivity::Error {
                    action: match control {
                        CodexControl::Refresh => CodexActionKind::Refresh,
                        CodexControl::SetAutoReset(_) => CodexActionKind::AutoReset,
                        CodexControl::SetServiceTier(_) => CodexActionKind::ServiceTier,
                    },
                    detail: "Codex background worker is unavailable".to_owned(),
                });
            }
        }
    }
}

#[derive(Clone, Copy)]
enum CodexUiAction {
    Refresh,
    SetAutoReset(bool),
    SetServiceTier(CodexServiceTier),
}

impl eframe::App for MonitorApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(json) = serde_json::to_string(&self.preferences) {
            storage.set_string(PREFERENCES_STORAGE_KEY, json);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        let theme = if visuals.dark_mode {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };

        Palette::for_theme(theme).panel_bg.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_latest_snapshot();
        self.receive_latest_codex_usage();
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.display_resize_pending
            && (ui.ctx().zoom_factor() - self.preferences.zoom_factor()).abs() < 0.001
        {
            apply_display_mode_size(ui.ctx(), self.display_mode);
            self.display_resize_pending = false;
        }

        let language = self.preferences.language.resolve(self.system_language);
        let palette = Palette::for_theme(ui.ctx().theme());
        let rect = ui.max_rect();
        let painter = ui.painter();

        painter.rect_filled(rect, 0.0, palette.panel_bg);

        let content_rect = rect.shrink2(Vec2::new(18.0, 15.0));
        let mut should_toggle_mode = draw_surface_toggle_targets(ui, rect, content_rect);

        ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
            should_toggle_mode |=
                draw_header(ui, self.last_update, self.display_mode, language, palette);

            match self.display_mode {
                DisplayMode::Full => {
                    should_toggle_mode |= draw_toggle_space(ui, 13.0);
                    draw_system_card(ui, &self.snapshot, language, palette);

                    should_toggle_mode |= draw_toggle_space(ui, 10.0);
                    if draw_codex_usage_card(
                        ui,
                        &self.codex_usage,
                        self.token_usage.as_ref(),
                        self.preferences.auto_reset,
                        self.codex_details_open,
                        language,
                        palette,
                    ) {
                        self.toggle_codex_details();
                    }
                }
                DisplayMode::Compact => {
                    should_toggle_mode |= draw_toggle_space(ui, 11.0);
                    if draw_compact_card(
                        ui,
                        &self.snapshot,
                        &self.codex_usage,
                        self.codex_details_open,
                        language,
                        palette,
                    ) {
                        self.toggle_codex_details();
                    }
                }
            }

            ui.add_space(8.0);
            let previous_font_size = self.preferences.font_size_points;
            if draw_preferences(
                ui,
                &mut self.preferences,
                &mut self.draft_font_size_points,
                language,
                palette,
            ) {
                apply_preferences(ui.ctx(), self.preferences, self.system_language);
                if self.preferences.font_size_points != previous_font_size {
                    self.display_resize_pending = true;
                    self.codex_details_position = None;
                    self.codex_details_needs_exact_position = self.codex_details_open;
                    self.codex_details_resize_pending = self.codex_details_open;
                }
            }

            should_toggle_mode |= draw_remaining_toggle_space(ui);
        });

        self.draw_codex_details_window(ui.ctx(), language);

        if should_toggle_mode {
            self.display_mode = self.display_mode.toggled();
            apply_display_mode_size(ui.ctx(), self.display_mode);
        }
    }
}

#[cfg(target_os = "macos")]
fn visible_screen_rects(zoom_factor: f32) -> Vec<Rect> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;

    let Some(main_thread_marker) = MainThreadMarker::new() else {
        return Vec::new();
    };
    let screens = NSScreen::screens(main_thread_marker);
    let main_screen_height = screens
        .iter()
        .map(|screen| screen.frame())
        .find(|frame| frame.origin.x.abs() < 0.5 && frame.origin.y.abs() < 0.5)
        .or_else(|| screens.firstObject().map(|screen| screen.frame()))
        .map(|frame| frame.size.height)
        .unwrap_or(0.0);
    if main_screen_height <= 0.0 {
        return Vec::new();
    }

    let zoom_factor = f64::from(zoom_factor.max(0.01));
    screens
        .iter()
        .filter_map(|screen| {
            let frame = screen.visibleFrame();
            if frame.size.width <= 0.0 || frame.size.height <= 0.0 {
                return None;
            }

            Some(Rect::from_min_size(
                Pos2::new(
                    (frame.origin.x / zoom_factor) as f32,
                    ((main_screen_height - frame.origin.y - frame.size.height) / zoom_factor)
                        as f32,
                ),
                Vec2::new(
                    (frame.size.width / zoom_factor) as f32,
                    (frame.size.height / zoom_factor) as f32,
                ),
            ))
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn visible_screen_rects(_zoom_factor: f32) -> Vec<Rect> {
    Vec::new()
}

fn compare_screens_for_parent(left: Rect, right: Rect, parent_rect: Rect) -> std::cmp::Ordering {
    let left_overlap = rect_overlap_area(left, parent_rect);
    let right_overlap = rect_overlap_area(right, parent_rect);
    left_overlap.total_cmp(&right_overlap).then_with(|| {
        rect_distance_squared(right, parent_rect)
            .total_cmp(&rect_distance_squared(left, parent_rect))
    })
}

fn adjacent_window_position(parent_rect: Rect, window_size: Vec2, screen_rect: Rect) -> Pos2 {
    let centered_x = parent_rect.center().x - window_size.x / 2.0;
    let centered_y = parent_rect.center().y - window_size.y / 2.0;
    let ideal_positions = [
        Pos2::new(parent_rect.right() + CODEX_WINDOW_GAP, centered_y),
        Pos2::new(
            parent_rect.left() - CODEX_WINDOW_GAP - window_size.x,
            centered_y,
        ),
        Pos2::new(centered_x, parent_rect.bottom() + CODEX_WINDOW_GAP),
        Pos2::new(
            centered_x,
            parent_rect.top() - CODEX_WINDOW_GAP - window_size.y,
        ),
    ];

    ideal_positions
        .into_iter()
        .enumerate()
        .map(|(preference, ideal_position)| {
            let position = clamp_window_origin(
                ideal_position,
                window_size,
                screen_rect,
                CODEX_SCREEN_MARGIN,
            );
            let window_rect = Rect::from_min_size(position, window_size);
            WindowPositionCandidate {
                position,
                overlap_area: rect_overlap_area(window_rect, parent_rect),
                distance_squared: rect_distance_squared(window_rect, parent_rect),
                correction_squared: (position - ideal_position).length_sq(),
                preference,
            }
        })
        .min_by(WindowPositionCandidate::compare)
        .map(|candidate| candidate.position)
        .unwrap_or(screen_rect.min)
}

struct WindowPositionCandidate {
    position: Pos2,
    overlap_area: f32,
    distance_squared: f32,
    correction_squared: f32,
    preference: usize,
}

impl WindowPositionCandidate {
    fn compare(left: &Self, right: &Self) -> std::cmp::Ordering {
        left.overlap_area
            .total_cmp(&right.overlap_area)
            .then_with(|| left.distance_squared.total_cmp(&right.distance_squared))
            .then_with(|| left.correction_squared.total_cmp(&right.correction_squared))
            .then_with(|| left.preference.cmp(&right.preference))
    }
}

fn clamp_window_origin(
    position: Pos2,
    window_size: Vec2,
    screen_rect: Rect,
    requested_margin: f32,
) -> Pos2 {
    let margin_x = requested_margin.min(((screen_rect.width() - window_size.x) / 2.0).max(0.0));
    let margin_y = requested_margin.min(((screen_rect.height() - window_size.y) / 2.0).max(0.0));
    let min_x = screen_rect.left() + margin_x;
    let max_x = screen_rect.right() - margin_x - window_size.x;
    let min_y = screen_rect.top() + margin_y;
    let max_y = screen_rect.bottom() - margin_y - window_size.y;

    Pos2::new(
        clamp_axis(position.x, min_x, max_x),
        clamp_axis(position.y, min_y, max_y),
    )
}

fn clamp_axis(value: f32, minimum: f32, maximum: f32) -> f32 {
    if minimum <= maximum {
        value.clamp(minimum, maximum)
    } else {
        (minimum + maximum) / 2.0
    }
}

fn rect_overlap_area(left: Rect, right: Rect) -> f32 {
    let intersection = left.intersect(right);
    intersection.width().max(0.0) * intersection.height().max(0.0)
}

fn rect_distance_squared(left: Rect, right: Rect) -> f32 {
    let horizontal = if left.right() < right.left() {
        right.left() - left.right()
    } else if right.right() < left.left() {
        left.left() - right.right()
    } else {
        0.0
    };
    let vertical = if left.bottom() < right.top() {
        right.top() - left.bottom()
    } else if right.bottom() < left.top() {
        left.top() - right.bottom()
    } else {
        0.0
    };
    horizontal * horizontal + vertical * vertical
}

fn apply_preferences(ctx: &egui::Context, preferences: UiPreferences, system_language: Language) {
    ctx.set_theme(preferences.theme.egui_preference());
    ctx.set_zoom_factor(preferences.zoom_factor());
    let title = match preferences.language.resolve(system_language) {
        Language::Japanese => APP_TITLE,
        Language::English => "System Monitor",
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.to_owned()));
}

fn draw_preferences(
    ui: &mut egui::Ui,
    preferences: &mut UiPreferences,
    draft_font_size_points: &mut u8,
    language: Language,
    palette: Palette,
) -> bool {
    let previous = *preferences;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 2.0);
        ui.spacing_mut().button_padding = Vec2::new(6.0, 1.0);
        ui.horizontal(|ui| {
            ui.label(match language {
                Language::Japanese => "言語",
                Language::English => "Language",
            });
            egui::ComboBox::from_id_salt("language_preference")
                .width(74.0)
                .selected_text(language_choice_label(preferences.language, language))
                .show_ui(ui, |ui| {
                    for choice in [
                        LanguageChoice::Ja,
                        LanguageChoice::En,
                        LanguageChoice::System,
                    ] {
                        ui.selectable_value(
                            &mut preferences.language,
                            choice,
                            language_choice_label(choice, language),
                        );
                    }
                });

            ui.label(match language {
                Language::Japanese => "テーマ",
                Language::English => "Theme",
            });
            egui::ComboBox::from_id_salt("theme_preference")
                .width(66.0)
                .selected_text(theme_choice_label(preferences.theme, language))
                .show_ui(ui, |ui| {
                    for choice in [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System] {
                        ui.selectable_value(
                            &mut preferences.theme,
                            choice,
                            theme_choice_label(choice, language),
                        );
                    }
                });

            ui.label(match language {
                Language::Japanese => "文字",
                Language::English => "Text",
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_sized(
                    [52.0, 22.0],
                    egui::DragValue::new(draft_font_size_points)
                        .range(UI_FONT_SIZE_MIN_POINTS..=UI_FONT_SIZE_MAX_POINTS)
                        .speed(1.0)
                        .fixed_decimals(0)
                        .max_decimals(0)
                        .suffix(" pt"),
                )
                .on_hover_text(match language {
                    Language::Japanese => "UI文字サイズ（10〜32 pt、1 pt刻み）",
                    Language::English => "UI text size (10–32 pt, 1 pt steps)",
                });

                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().button_padding = Vec2::ZERO;

                    if ui
                        .add_enabled(
                            *draft_font_size_points < UI_FONT_SIZE_MAX_POINTS,
                            egui::Button::new(RichText::new("▲").size(7.0))
                                .min_size(Vec2::new(16.0, 10.0)),
                        )
                        .on_hover_text(match language {
                            Language::Japanese => "文字サイズを1 pt大きくする",
                            Language::English => "Increase text size by 1 pt",
                        })
                        .clicked()
                    {
                        *draft_font_size_points = increment_ui_font_size(*draft_font_size_points);
                    }

                    if ui
                        .add_enabled(
                            *draft_font_size_points > UI_FONT_SIZE_MIN_POINTS,
                            egui::Button::new(RichText::new("▼").size(7.0))
                                .min_size(Vec2::new(16.0, 10.0)),
                        )
                        .on_hover_text(match language {
                            Language::Japanese => "文字サイズを1 pt小さくする",
                            Language::English => "Decrease text size by 1 pt",
                        })
                        .clicked()
                    {
                        *draft_font_size_points = decrement_ui_font_size(*draft_font_size_points);
                    }
                });

                let has_pending_change = preferences.font_size_points != *draft_font_size_points;
                let (button_text, button_color, tooltip) = match (has_pending_change, language) {
                    (true, Language::Japanese) => {
                        ("確定", palette.error_red, "未確定の文字サイズを適用する")
                    }
                    (true, Language::English) => {
                        ("Apply", palette.error_red, "Apply the pending text size")
                    }
                    (false, Language::Japanese) => {
                        ("確定済", palette.codex_accent, "文字サイズは適用済み")
                    }
                    (false, Language::English) => {
                        ("Applied", palette.codex_accent, "The text size is applied")
                    }
                };
                let confirm_button = egui::Button::new(
                    RichText::new(button_text)
                        .size(11.0)
                        .strong()
                        .color(Color32::WHITE),
                )
                .min_size(Vec2::new(60.0, 22.0))
                .fill(button_color)
                .stroke(Stroke::new(1.0, button_color))
                .sense(if has_pending_change {
                    Sense::click()
                } else {
                    Sense::hover()
                });
                if ui.add(confirm_button).on_hover_text(tooltip).clicked() {
                    commit_ui_font_size(&mut preferences.font_size_points, *draft_font_size_points);
                }
            });
        });
    });
    *preferences != previous
}

fn language_choice_label(choice: LanguageChoice, language: Language) -> &'static str {
    match (choice, language) {
        (LanguageChoice::Ja, Language::Japanese) => "日本語",
        (LanguageChoice::En, Language::Japanese) => "英語",
        (LanguageChoice::System, Language::Japanese) => "システム",
        (LanguageChoice::Ja, Language::English) => "Japanese",
        (LanguageChoice::En, Language::English) => "English",
        (LanguageChoice::System, Language::English) => "System",
    }
}

fn theme_choice_label(choice: ThemeChoice, language: Language) -> &'static str {
    match (choice, language) {
        (ThemeChoice::Light, Language::Japanese) => "ライト",
        (ThemeChoice::Dark, Language::Japanese) => "ダーク",
        (ThemeChoice::System, Language::Japanese) => "システム",
        (ThemeChoice::Light, Language::English) => "Light",
        (ThemeChoice::Dark, Language::English) => "Dark",
        (ThemeChoice::System, Language::English) => "System",
    }
}

fn apply_display_mode_size(ctx: &egui::Context, display_mode: DisplayMode) {
    ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(display_mode.min_size()));
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(display_mode.size()));
}

fn draw_surface_toggle_targets(ui: &mut egui::Ui, rect: Rect, content_rect: Rect) -> bool {
    [
        (
            "surface_toggle_top",
            Rect::from_min_max(rect.left_top(), Pos2::new(rect.right(), content_rect.top())),
        ),
        (
            "surface_toggle_bottom",
            Rect::from_min_max(
                Pos2::new(rect.left(), content_rect.bottom()),
                rect.right_bottom(),
            ),
        ),
        (
            "surface_toggle_left",
            Rect::from_min_max(
                Pos2::new(rect.left(), content_rect.top()),
                Pos2::new(content_rect.left(), content_rect.bottom()),
            ),
        ),
        (
            "surface_toggle_right",
            Rect::from_min_max(
                Pos2::new(content_rect.right(), content_rect.top()),
                Pos2::new(rect.right(), content_rect.bottom()),
            ),
        ),
    ]
    .into_iter()
    .any(|(id_source, target_rect)| {
        ui.interact(
            target_rect,
            ui.make_persistent_id(id_source),
            Sense::click(),
        )
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
    })
}

fn draw_toggle_space(ui: &mut egui::Ui, height: f32) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    ui.painter().rect_filled(rect, 0.0, Color32::TRANSPARENT);
    response.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

fn draw_remaining_toggle_space(ui: &mut egui::Ui) -> bool {
    let height = ui.available_height();
    if !height.is_finite() || height <= 0.0 {
        return false;
    }

    draw_toggle_space(ui, height)
}

fn register_japanese_font(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let font_names = JAPANESE_FONT_PATHS
        .iter()
        .filter_map(|path| {
            let font_bytes = fs::read(path).ok()?;
            let name = format!("{JAPANESE_FONT_NAME}_{:x}", fxhash(path));
            fonts
                .font_data
                .insert(name.clone(), FontData::from_owned(font_bytes).into());
            Some(name)
        })
        .collect::<Vec<_>>();

    if font_names.is_empty() {
        return;
    }

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        if let Some(fonts_for_family) = fonts.families.get_mut(&family) {
            for (index, font_name) in font_names.iter().enumerate() {
                fonts_for_family.insert(index, font_name.clone());
            }
        }
    }

    ctx.set_fonts(fonts);
}

fn fxhash(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::System);
    ctx.options_mut(|options| options.zoom_with_keyboard = false);

    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let palette = Palette::for_theme(theme);
        let mut style = theme.default_style();

        style.visuals.panel_fill = Color32::TRANSPARENT;
        style.visuals.window_fill = palette.panel_bg;
        style.visuals.extreme_bg_color = palette.extreme_bg;
        style.spacing.item_spacing = Vec2::new(8.0, 5.0);
        style.spacing.button_padding = Vec2::new(8.0, 4.0);
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(18.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Body,
            FontId::new(
                f32::from(UI_FONT_SIZE_DEFAULT_POINTS),
                FontFamily::Proportional,
            ),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        );

        ctx.set_style_of(theme, style);
    }
}

fn start_metrics_sampler() -> (Snapshot, Receiver<Snapshot>) {
    let mut sampler = MetricsSampler::new();
    let initial = sampler.sample();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(1));

            if tx.send(sampler.sample()).is_err() {
                break;
            }
        }
    });

    (initial, rx)
}

fn start_token_usage_sampler() -> Receiver<TokenUsageState> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut sampler = TokenUsageSampler::from_env();
        loop {
            if tx.send(sampler.sample(unix_now_seconds())).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
    rx
}

fn start_codex_usage_sampler(
    auto_reset_enabled: bool,
) -> (Sender<CodexControl>, Receiver<CodexUsageState>) {
    let (tx, rx) = mpsc::channel();
    let (control_tx, control_rx) = mpsc::channel();

    thread::spawn(move || {
        let mut poller = CodexUsagePoller::new(auto_reset_enabled);
        if tx.send(poller.refresh()).is_err() {
            return;
        }

        loop {
            let state = match control_rx.recv_timeout(poller.next_delay()) {
                Ok(CodexControl::Refresh) => poller.refresh(),
                Ok(CodexControl::SetAutoReset(enabled)) => poller.set_auto_reset_enabled(enabled),
                Ok(CodexControl::SetServiceTier(service_tier)) => {
                    poller.set_service_tier(service_tier)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => poller.refresh(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            if tx.send(state).is_err() {
                break;
            }
        }
    });

    (control_tx, rx)
}

fn draw_header(
    ui: &mut egui::Ui,
    last_update: Instant,
    display_mode: DisplayMode,
    language: Language,
    palette: Palette,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter_at(rect);
    let center_y = rect.center().y;
    let age = last_update.elapsed().as_secs_f32();
    let live_color = if age < 1.8 {
        palette.accent_green
    } else {
        palette.text_muted
    };

    painter.text(
        Pos2::new(rect.left(), center_y),
        Align2::LEFT_CENTER,
        match language {
            Language::Japanese => APP_TITLE,
            Language::English => "System Monitor",
        },
        FontId::proportional(18.0),
        palette.text_main,
    );
    painter.text(
        Pos2::new(rect.right(), center_y),
        Align2::RIGHT_CENTER,
        match language {
            Language::Japanese => "ライブ",
            Language::English => "LIVE",
        },
        FontId::proportional(10.0),
        palette.text_subtle,
    );
    painter.text(
        Pos2::new(rect.right() - 29.0, center_y - 0.5),
        Align2::RIGHT_CENTER,
        "●",
        FontId::proportional(11.0),
        live_color,
    );
    painter.text(
        Pos2::new(rect.right() - 46.0, center_y),
        Align2::RIGHT_CENTER,
        display_mode.label(language),
        FontId::proportional(10.0),
        palette.text_subtle,
    );

    response.clicked()
}

fn draw_system_card(ui: &mut egui::Ui, snapshot: &Snapshot, language: Language, palette: Palette) {
    let available_width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(available_width, 112.0), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12.0, palette.card_bg);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, palette.card_stroke),
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(15.0, 12.0));
    painter.text(
        inner.left_top(),
        Align2::LEFT_TOP,
        match language {
            Language::Japanese => "システム",
            Language::English => "System",
        },
        FontId::proportional(14.0),
        palette.text_main,
    );
    painter.text(
        inner.right_top(),
        Align2::RIGHT_TOP,
        match language {
            Language::Japanese => "CPU / メモリ",
            Language::English => "CPU / Memory",
        },
        FontId::proportional(10.0),
        palette.text_subtle,
    );

    let column_top = inner.top() + 28.0;
    let column_gap = 18.0;
    let column_width = (inner.width() - column_gap) / 2.0;
    let cpu_rect = Rect::from_min_size(
        Pos2::new(inner.left(), column_top),
        Vec2::new(column_width, inner.bottom() - column_top),
    );
    let memory_rect = Rect::from_min_size(
        Pos2::new(cpu_rect.right() + column_gap, column_top),
        Vec2::new(column_width, inner.bottom() - column_top),
    );

    let divider_x = cpu_rect.right() + column_gap / 2.0;
    painter.line_segment(
        [
            Pos2::new(divider_x, column_top + 3.0),
            Pos2::new(divider_x, inner.bottom() - 1.0),
        ],
        Stroke::new(1.0, palette.panel_stroke),
    );

    draw_system_metric(
        &painter,
        cpu_rect,
        SystemMetricDisplay {
            title: "CPU",
            value: format!("{:.0}%", snapshot.cpu_percent.clamp(0.0, 100.0)),
            detail: cpu_detail(snapshot, language),
            percent: snapshot.cpu_percent,
            accent: palette.accent_green,
        },
        palette,
    );
    draw_system_metric(
        &painter,
        memory_rect,
        SystemMetricDisplay {
            title: match language {
                Language::Japanese => "メモリ",
                Language::English => "Memory",
            },
            value: format!(
                "{:.1}/{:.1} GiB",
                snapshot.memory_used_gib, snapshot.memory_total_gib
            ),
            detail: match language {
                Language::Japanese => {
                    format!("使用率 {:.0}%", snapshot.memory_percent.clamp(0.0, 100.0))
                }
                Language::English => {
                    format!("Used {:.0}%", snapshot.memory_percent.clamp(0.0, 100.0))
                }
            },
            percent: snapshot.memory_percent,
            accent: palette.codex_accent,
        },
        palette,
    );
}

fn draw_compact_card(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    state: &CodexUsageState,
    details_open: bool,
    language: Language,
    palette: Palette,
) -> bool {
    let available_width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(available_width, 82.0), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12.0, palette.card_bg);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, palette.card_stroke),
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(15.0, 13.0));
    let column_gap = 14.0;
    let column_width = (inner.width() - column_gap * 2.0) / 3.0;
    let cpu_rect = Rect::from_min_size(inner.left_top(), Vec2::new(column_width, inner.height()));
    let memory_rect = Rect::from_min_size(
        Pos2::new(cpu_rect.right() + column_gap, inner.top()),
        Vec2::new(column_width, inner.height()),
    );
    let codex_rect = Rect::from_min_size(
        Pos2::new(memory_rect.right() + column_gap, inner.top()),
        Vec2::new(column_width, inner.height()),
    );

    draw_compact_metric(
        &painter,
        cpu_rect,
        CompactMetricDisplay {
            title: "CPU",
            value: format!("{:.0}%", snapshot.cpu_percent.clamp(0.0, 100.0)),
            detail: cpu_detail(snapshot, language),
            percent: snapshot.cpu_percent,
            accent: palette.accent_green,
        },
        palette,
    );
    draw_compact_metric(
        &painter,
        memory_rect,
        CompactMetricDisplay {
            title: match language {
                Language::Japanese => "メモリ",
                Language::English => "Mem",
            },
            value: format!("{:.0}%", snapshot.memory_percent.clamp(0.0, 100.0)),
            detail: format!(
                "{:.1}/{:.1} GiB",
                snapshot.memory_used_gib, snapshot.memory_total_gib
            ),
            percent: snapshot.memory_percent,
            accent: palette.codex_accent,
        },
        palette,
    );
    draw_compact_metric(
        &painter,
        codex_rect,
        CompactMetricDisplay {
            title: if details_open {
                "Codex ×"
            } else {
                "Codex ›"
            },
            value: codex_compact_value(state, language),
            detail: codex_compact_detail(state, language),
            percent: codex_compact_percent(state),
            accent: status_color(state.status, palette),
        },
        palette,
    );

    ui.interact(
        codex_rect,
        ui.make_persistent_id("compact_codex_details"),
        Sense::click(),
    )
    .on_hover_cursor(CursorIcon::PointingHand)
    .on_hover_text(codex_details_toggle_tooltip(details_open, language))
    .clicked()
}

struct CompactMetricDisplay {
    title: &'static str,
    value: String,
    detail: String,
    percent: f32,
    accent: Color32,
}

fn draw_compact_metric(
    painter: &egui::Painter,
    rect: Rect,
    metric: CompactMetricDisplay,
    palette: Palette,
) {
    painter.text(
        rect.left_top(),
        Align2::LEFT_TOP,
        metric.title,
        FontId::proportional(10.0),
        palette.text_muted,
    );
    painter.text(
        Pos2::new(rect.left(), rect.top() + 18.0),
        Align2::LEFT_TOP,
        compact_text(&metric.value, 9),
        FontId::proportional(18.0),
        palette.text_main,
    );
    painter.text(
        Pos2::new(rect.left(), rect.top() + 43.0),
        Align2::LEFT_TOP,
        compact_text(&metric.detail, 14),
        FontId::proportional(9.5),
        palette.text_subtle,
    );

    draw_metric_track(painter, rect, metric.percent, metric.accent, palette);
}

struct SystemMetricDisplay {
    title: &'static str,
    value: String,
    detail: String,
    percent: f32,
    accent: Color32,
}

fn draw_system_metric(
    painter: &egui::Painter,
    rect: Rect,
    metric: SystemMetricDisplay,
    palette: Palette,
) {
    painter.text(
        rect.left_top(),
        Align2::LEFT_TOP,
        metric.title,
        FontId::proportional(12.0),
        palette.text_muted,
    );
    painter.text(
        Pos2::new(rect.left(), rect.top() + 17.0),
        Align2::LEFT_TOP,
        metric.value,
        FontId::proportional(22.0),
        palette.text_main,
    );
    painter.text(
        Pos2::new(rect.left(), rect.top() + 44.0),
        Align2::LEFT_TOP,
        compact_text(&metric.detail, 22),
        FontId::proportional(10.0),
        palette.text_muted,
    );

    draw_metric_track(painter, rect, metric.percent, metric.accent, palette);
}

fn draw_metric_track(
    painter: &egui::Painter,
    rect: Rect,
    percent: f32,
    accent: Color32,
    palette: Palette,
) {
    let clamped = percent.clamp(0.0, 100.0) / 100.0;
    let track = Rect::from_min_size(
        Pos2::new(rect.left(), rect.bottom() - 5.0),
        Vec2::new(rect.width(), 4.0),
    );
    painter.rect_filled(track, 2.0, palette.track_bg);

    let fill = Rect::from_min_size(
        track.left_top(),
        Vec2::new(track.width() * clamped, track.height()),
    );
    painter.rect_filled(fill, 2.0, accent);
}

fn draw_codex_usage_card(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    token_usage: Option<&TokenUsageState>,
    auto_reset: bool,
    details_open: bool,
    language: Language,
    palette: Palette,
) -> bool {
    let available_width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(available_width, 194.0), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12.0, palette.card_bg);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, palette.card_stroke),
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(15.0, 12.0));

    painter.text(
        inner.left_top(),
        Align2::LEFT_TOP,
        match language {
            Language::Japanese => "Codex 使用量",
            Language::English => "Codex usage",
        },
        FontId::proportional(14.0),
        palette.text_main,
    );
    painter.text(
        Pos2::new(inner.right(), inner.top() + 1.0),
        Align2::RIGHT_TOP,
        localized_status(state.status, language),
        FontId::proportional(10.0),
        status_color(state.status, palette),
    );

    let footer_height = 26.0;
    let footer_rect = Rect::from_min_max(
        Pos2::new(inner.left(), inner.bottom() - footer_height),
        inner.right_bottom(),
    );
    let content_rect = Rect::from_min_max(
        Pos2::new(inner.left(), inner.top() + 31.0),
        Pos2::new(inner.right(), footer_rect.top() - 7.0),
    );

    ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
        ui.set_clip_rect(content_rect);
        ui.set_width(content_rect.width());
        draw_codex_usage_content(ui, state.content.as_ref(), token_usage, language, palette);
    });

    let mut toggle_details = false;
    ui.scope_builder(egui::UiBuilder::new().max_rect(footer_rect), |ui| {
        ui.set_clip_rect(footer_rect);
        ui.set_width(footer_rect.width());
        toggle_details =
            draw_codex_summary_footer(ui, state, auto_reset, details_open, language, palette);
    });
    toggle_details
}

fn draw_codex_usage_content(
    ui: &mut egui::Ui,
    content: Option<&CodexUsageContent>,
    token_usage: Option<&TokenUsageState>,
    language: Language,
    palette: Palette,
) {
    ui.columns(2, |columns| {
        draw_quota_section(
            &mut columns[0],
            content.map(|content| &content.codex),
            "Codex",
            Some(match language {
                Language::Japanese => "未取得",
                Language::English => "Unavailable",
            }),
            language,
            palette.codex_accent,
            palette,
        );
        draw_token_usage_section(&mut columns[1], token_usage, language, palette);
    });
}

fn abbreviated_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{:.2}K", tokens as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.2}M", tokens as f64 / 1_000_000.0),
        _ => format!("{:.2}B", tokens as f64 / 1_000_000_000.0),
    }
}

fn draw_token_usage_section(
    ui: &mut egui::Ui,
    state: Option<&TokenUsageState>,
    language: Language,
    palette: Palette,
) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 106.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let title = match language {
        Language::Japanese => "トークン · 直近1時間",
        Language::English => "Tokens · last hour",
    };
    painter.text(
        rect.left_top(),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(12.0),
        palette.text_main,
    );

    let totals = state.and_then(|state| state.totals.as_ref());
    let total = totals
        .map(|t| abbreviated_tokens(t.input.saturating_add(t.output)))
        .unwrap_or_else(|| "--".to_owned());
    painter.text(
        rect.left_top() + Vec2::new(0.0, 23.0),
        Align2::LEFT_TOP,
        total,
        FontId::proportional(26.0),
        palette.token_accent,
    );

    let (detail, cache, note) = if let Some(t) = totals {
        let hit = if t.input == 0 {
            "--".to_owned()
        } else {
            format!("{:.0}%", t.cached_input as f64 / t.input as f64 * 100.0)
        };
        let input = abbreviated_tokens(t.input);
        let output = abbreviated_tokens(t.output);
        let cached = abbreviated_tokens(t.cached_input);
        let partial = state.is_some_and(|s| s.partial);
        match language {
            Language::Japanese => (
                format!("入力 {input} / 出力 {output}"),
                format!("キャッシュ {cached} · {hit}"),
                if partial {
                    "このMac · 一部集計"
                } else {
                    "このMac"
                },
            ),
            Language::English => (
                format!("In {input} / Out {output}"),
                format!("Cached {cached} · {hit}"),
                if partial {
                    "This Mac · partial"
                } else {
                    "This Mac"
                },
            ),
        }
    } else {
        let label = match (state.is_some(), language) {
            (false, Language::Japanese) => "集計中…",
            (true, Language::Japanese) => "記録を取得できません",
            (false, Language::English) => "Loading…",
            (true, Language::English) => "Records unavailable",
        };
        (label.to_owned(), String::new(), "")
    };
    for (y, text, color) in [
        (55.0, detail, palette.text_muted),
        (73.0, cache, palette.text_muted),
        (
            91.0,
            note.to_owned(),
            if state.is_some_and(|s| s.partial) {
                palette.warning_amber
            } else {
                palette.text_muted
            },
        ),
    ] {
        painter.text(
            rect.left_top() + Vec2::new(0.0, y),
            Align2::LEFT_TOP,
            text,
            FontId::proportional(10.5),
            color,
        );
    }
    response.on_hover_text(match language {
        Language::Japanese => "このMacに保存されたCodex記録の直近60分。5秒ごとに集計します。\n合計 = 入力 + 出力。キャッシュは入力の内訳で、割合はキャッシュ入力 ÷ 全入力です（リクエスト単位のヒット率ではありません）。推論は出力に含まれます。\n記録時刻で集計するため、処理途中の使用量や他の端末・クラウドの使用量は含まれない場合があります。利用枠の消費率とは異なります。\n「一部集計」は読めない記録などがあり、集計が不完全な状態です。",
        Language::English => "Last 60 minutes of Codex records stored on this Mac, refreshed every 5 seconds.\nTotal = input + output. Cached tokens are part of input; the percentage is cached input / all input, not a request-level hit rate. Reasoning is part of output.\nUses record timestamps; in-flight, other-device and cloud usage may be absent. This is not quota consumption.\nPartial means some records could not be counted completely.",
    });
}

fn draw_codex_summary_footer(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    auto_reset: bool,
    details_open: bool,
    language: Language,
    palette: Palette,
) -> bool {
    let (reset_count, service_tier) = state
        .content
        .as_ref()
        .map(|content| {
            (
                content
                    .reset_credits
                    .available_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "--".to_owned()),
                localized_service_tier(content.service_tier, language),
            )
        })
        .unwrap_or_else(|| ("--".to_owned(), "--"));
    let mut toggle_details = false;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        ui.label(
            RichText::new(format!("RESET {reset_count}"))
                .size(10.5)
                .strong()
                .color(palette.text_main),
        );
        ui.label(
            RichText::new(service_tier)
                .size(10.5)
                .color(palette.codex_accent),
        );
        ui.label(
            RichText::new(match (auto_reset, language) {
                (true, Language::Japanese) => "自動 ON",
                (false, Language::Japanese) => "自動 OFF",
                (true, Language::English) => "Auto ON",
                (false, Language::English) => "Auto OFF",
            })
            .size(10.5)
            .color(if auto_reset {
                palette.accent_green
            } else {
                palette.text_muted
            }),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            toggle_details = ui
                .add(
                    egui::Button::new(
                        RichText::new(codex_details_toggle_label(details_open, language))
                            .size(11.0),
                    )
                    .min_size(Vec2::new(60.0, 22.0))
                    .selected(details_open),
                )
                .on_hover_text(codex_details_toggle_tooltip(details_open, language))
                .clicked();
        });
    });

    toggle_details
}

fn codex_details_toggle_label(details_open: bool, language: Language) -> &'static str {
    match (details_open, language) {
        (false, Language::Japanese) => "管理…",
        (true, Language::Japanese) => "閉じる",
        (false, Language::English) => "Manage…",
        (true, Language::English) => "Close",
    }
}

fn codex_details_toggle_tooltip(details_open: bool, language: Language) -> &'static str {
    match (details_open, language) {
        (false, Language::Japanese) => "Codex管理ウィンドウを開く",
        (true, Language::Japanese) => "Codex管理ウィンドウを閉じる",
        (false, Language::English) => "Open Codex controls",
        (true, Language::English) => "Close Codex controls",
    }
}

fn draw_codex_management(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    token_usage: Option<&TokenUsageState>,
    auto_reset: bool,
    language: Language,
    actions: &mut Vec<CodexUiAction>,
    close_requested: &mut bool,
) {
    let palette = Palette::for_theme(ui.ctx().theme());
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, palette.panel_bg);
    let content_rect = ui.max_rect().shrink2(Vec2::new(18.0, 16.0));

    ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
        ui.set_width(content_rect.width());
        let busy = state
            .activity
            .as_ref()
            .is_some_and(|activity| matches!(activity, CodexActivity::Working(_)));

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(match language {
                        Language::Japanese => "Codex 管理",
                        Language::English => "Codex controls",
                    })
                    .size(18.0)
                    .strong()
                    .color(palette.text_main),
                );
                ui.label(
                    RichText::new(last_updated_text(state, language))
                        .size(11.0)
                        .color(palette.text_muted),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(codex_details_toggle_label(true, language))
                            .min_size(Vec2::new(60.0, 24.0)),
                    )
                    .on_hover_text(codex_details_toggle_tooltip(true, language))
                    .clicked()
                {
                    *close_requested = true;
                }
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(match language {
                            Language::Japanese => "再読込",
                            Language::English => "Refresh",
                        }),
                    )
                    .clicked()
                {
                    actions.push(CodexUiAction::Refresh);
                }
                ui.label(
                    RichText::new(localized_status(state.status, language))
                        .size(10.5)
                        .color(status_color(state.status, palette)),
                );
            });
        });

        ui.add_space(10.0);
        draw_codex_activity(ui, state, language, palette);
        ui.add_space(8.0);
        draw_codex_links(ui, language, palette);
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            .id_salt("codex_management_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                draw_codex_limits_panel(ui, state, token_usage, language, palette);
                ui.add_space(10.0);
                draw_codex_controls_panel(ui, state, auto_reset, busy, language, palette, actions);
                ui.add_space(10.0);
                draw_reset_credits_panel(ui, state, language, palette);
                ui.add_space(4.0);
            });
    });
}

fn draw_codex_limits_panel(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    token_usage: Option<&TokenUsageState>,
    language: Language,
    palette: Palette,
) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new(match language {
                Language::Japanese => "利用枠とトークン",
                Language::English => "Limits and tokens",
            })
            .size(14.0)
            .strong()
            .color(palette.text_main),
        );
        ui.add_space(5.0);
        draw_codex_usage_content(ui, state.content.as_ref(), token_usage, language, palette);
    });
}

fn draw_codex_controls_panel(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    auto_reset: bool,
    busy: bool,
    language: Language,
    palette: Palette,
    actions: &mut Vec<CodexUiAction>,
) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new(match language {
                Language::Japanese => "動作設定",
                Language::English => "Behavior",
            })
            .size(14.0)
            .strong()
            .color(palette.text_main),
        );
        ui.add_space(7.0);

        let current_tier = state
            .content
            .as_ref()
            .map(|content| content.service_tier)
            .unwrap_or(CodexServiceTier::Unknown);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(match language {
                    Language::Japanese => "Codex既定速度",
                    Language::English => "Codex default speed",
                })
                .size(12.5)
                .color(palette.text_main),
            );
            ui.add_enabled_ui(!busy, |ui| {
                if ui
                    .selectable_label(
                        current_tier == CodexServiceTier::Standard,
                        "Standard",
                    )
                    .clicked()
                    && current_tier != CodexServiceTier::Standard
                {
                    actions.push(CodexUiAction::SetServiceTier(
                        CodexServiceTier::Standard,
                    ));
                }
                if ui
                    .selectable_label(current_tier == CodexServiceTier::Fast, "Fast")
                    .clicked()
                    && current_tier != CodexServiceTier::Fast
                {
                    actions.push(CodexUiAction::SetServiceTier(CodexServiceTier::Fast));
                }
            });
        });
        ui.label(
            RichText::new(match language {
                Language::Japanese => {
                    "新規タスク／設定再読込後の既定値です。別の実行中タスクは即時変更されません。"
                }
                Language::English => {
                    "Default for new tasks or after config reload; other running tasks are not switched immediately."
                }
            })
            .size(11.0)
            .color(palette.text_muted),
        );

        ui.add_space(8.0);
        let mut next_auto_reset = auto_reset;
        let response = ui.add_enabled(
            !busy,
            egui::Checkbox::new(
                &mut next_auto_reset,
                match language {
                    Language::Japanese => "週次残量が0%ならRESETを自動使用",
                    Language::English => "Auto-use RESET when weekly remaining reaches 0%",
                },
            ),
        );
        if response.changed() {
            actions.push(CodexUiAction::SetAutoReset(next_auto_reset));
        }
        ui.label(
            RichText::new(match language {
                Language::Japanese => {
                    "期限が最も近い利用可能な1枚だけを使います。初期設定はOFFです。"
                }
                Language::English => {
                    "Uses only the available credit with the nearest expiry. Default is OFF."
                }
            })
            .size(11.0)
            .color(palette.text_muted),
        );
    });
}

fn draw_reset_credits_panel(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    language: Language,
    palette: Palette,
) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        let inventory = state.content.as_ref().map(|content| &content.reset_credits);
        let count = inventory
            .and_then(|inventory| inventory.available_count)
            .map(|count| count.to_string())
            .unwrap_or_else(|| "--".to_owned());
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(match language {
                    Language::Japanese => "RESETクレジット",
                    Language::English => "RESET credits",
                })
                .size(14.0)
                .strong()
                .color(palette.text_main),
            );
            ui.label(
                RichText::new(match language {
                    Language::Japanese => format!("利用可能 {count}枚"),
                    Language::English => format!("{count} available"),
                })
                .size(12.0)
                .color(palette.codex_accent),
            );
        });
        if let Some((expires_at, fetched_at)) = inventory
            .and_then(|inventory| inventory.nearest_expiry())
            .zip(state.content.as_ref().map(|content| content.fetched_at))
        {
            ui.label(
                RichText::new(match language {
                    Language::Japanese => {
                        format!("最短期限: {}", expiry_text(Some(expires_at), fetched_at, language))
                    }
                    Language::English => {
                        format!(
                            "Nearest: {}",
                            expiry_text(Some(expires_at), fetched_at, language)
                        )
                    }
                })
                .size(11.0)
                .color(palette.warning_amber),
            );
        }
        ui.add_space(5.0);

        match inventory {
            None => {
                ui.label(
                    RichText::new(match language {
                        Language::Japanese => "クレジット情報を取得できていません。",
                        Language::English => "Credit information is unavailable.",
                    })
                    .color(palette.text_muted),
                );
            }
            Some(inventory) if inventory.available_count == Some(0) => {
                ui.label(
                    RichText::new(match language {
                        Language::Japanese => "利用可能なRESETクレジットはありません。",
                        Language::English => "No RESET credits are currently available.",
                    })
                    .color(palette.text_muted),
                );
            }
            Some(inventory) => {
                if !inventory.details_complete {
                    ui.label(
                        RichText::new(match language {
                            Language::Japanese => {
                                "在庫数のみ、または一覧が一部だけ取得されています。期限順を保証できない間は自動使用しません。"
                            }
                            Language::English => {
                                "Only a count or partial list is available. Auto-use waits until nearest-expiry ordering can be guaranteed."
                            }
                        })
                        .size(11.0)
                        .color(palette.warning_amber),
                    );
                    ui.add_space(4.0);
                }

                egui::ScrollArea::vertical()
                    .id_salt("reset_credit_list")
                    .max_height(RESET_CREDIT_LIST_MAX_HEIGHT)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, credit) in inventory.credits.iter().enumerate() {
                            draw_reset_credit_row(
                                ui,
                                credit,
                                index,
                                state
                                    .content
                                    .as_ref()
                                    .map(|content| content.fetched_at)
                                    .unwrap_or_default(),
                                language,
                                palette,
                            );
                            if index + 1 < inventory.credits.len() {
                                ui.add_space(5.0);
                            }
                        }
                    });
            }
        }
    });
}

fn draw_reset_credit_row(
    ui: &mut egui::Ui,
    credit: &ResetCredit,
    index: usize,
    now_secs: i64,
    language: Language,
    palette: Palette,
) {
    egui::Frame::new()
        .fill(palette.card_bg)
        .stroke(Stroke::new(1.0, palette.card_stroke))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(credit.title.as_deref().unwrap_or(match language {
                        Language::Japanese => "Codex RESETクレジット",
                        Language::English => "Codex RESET credit",
                    }))
                    .size(12.5)
                    .strong()
                    .color(palette.text_main),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("#{}", index + 1))
                            .size(10.0)
                            .color(palette.text_subtle),
                    );
                });
            });

            ui.label(
                RichText::new(expiry_text(credit.expires_at, now_secs, language))
                    .size(11.0)
                    .color(if index == 0 {
                        palette.warning_amber
                    } else {
                        palette.text_muted
                    }),
            );
            if let Some(description) = credit.description.as_deref() {
                ui.label(
                    RichText::new(compact_text(description, 110))
                        .size(11.0)
                        .color(palette.text_muted),
                );
            }
        });
}

fn draw_codex_activity(
    ui: &mut egui::Ui,
    state: &CodexUsageState,
    language: Language,
    palette: Palette,
) {
    let (text, color) = if let Some(activity) = state.activity.as_ref() {
        localized_activity(activity, language, palette)
    } else if let Some(error) = state.error.as_deref() {
        (
            format!(
                "{}: {}",
                match language {
                    Language::Japanese => "更新エラー",
                    Language::English => "Refresh error",
                },
                error
            ),
            palette.error_red,
        )
    } else {
        return;
    };

    egui::Frame::new()
        .fill(palette.track_bg)
        .corner_radius(7)
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(color));
        });
}

fn localized_activity(
    activity: &CodexActivity,
    language: Language,
    palette: Palette,
) -> (String, Color32) {
    match activity {
        CodexActivity::Working(action) => (
            match (action, language) {
                (CodexActionKind::Refresh, Language::Japanese) => "再読込しています…",
                (CodexActionKind::ServiceTier, Language::Japanese) => {
                    "Codex既定速度を保存しています…"
                }
                (CodexActionKind::AutoReset, Language::Japanese) => {
                    "自動RESET設定を反映しています…"
                }
                (CodexActionKind::Refresh, Language::English) => "Refreshing…",
                (CodexActionKind::ServiceTier, Language::English) => "Saving Codex default speed…",
                (CodexActionKind::AutoReset, Language::English) => "Applying auto-RESET setting…",
            }
            .to_owned(),
            palette.codex_accent,
        ),
        CodexActivity::ServiceTierSaved(tier) => (
            match language {
                Language::Japanese => {
                    format!(
                        "Codex既定速度を{}に保存しました。",
                        localized_service_tier(*tier, language)
                    )
                }
                Language::English => {
                    format!(
                        "Saved Codex default speed as {}.",
                        localized_service_tier(*tier, language)
                    )
                }
            },
            palette.accent_green,
        ),
        CodexActivity::ResetConsumed => (
            match language {
                Language::Japanese => "RESETクレジットを1枚使用しました。",
                Language::English => "Used one RESET credit.",
            }
            .to_owned(),
            palette.accent_green,
        ),
        CodexActivity::ResetAlreadyApplied => (
            match language {
                Language::Japanese => "同じRESET操作はすでに完了しています。",
                Language::English => "This RESET operation was already completed.",
            }
            .to_owned(),
            palette.accent_green,
        ),
        CodexActivity::ResetSkippedNoEligibleWindow => (
            match language {
                Language::Japanese => "現在RESETできる利用枠がありません。",
                Language::English => "No current limit window is eligible for RESET.",
            }
            .to_owned(),
            palette.warning_amber,
        ),
        CodexActivity::ResetSkippedNoCredit => (
            match language {
                Language::Japanese => "利用可能なRESETクレジットがありません。",
                Language::English => "No RESET credit is available.",
            }
            .to_owned(),
            palette.warning_amber,
        ),
        CodexActivity::Error { action, detail } => (
            format!(
                "{}: {}",
                match (action, language) {
                    (CodexActionKind::Refresh, Language::Japanese) => "再読込エラー",
                    (CodexActionKind::ServiceTier, Language::Japanese) => "速度設定エラー",
                    (CodexActionKind::AutoReset, Language::Japanese) => "自動RESETエラー",
                    (CodexActionKind::Refresh, Language::English) => "Refresh error",
                    (CodexActionKind::ServiceTier, Language::English) => "Speed setting error",
                    (CodexActionKind::AutoReset, Language::English) => "Auto-RESET error",
                },
                detail
            ),
            palette.error_red,
        ),
    }
}

fn draw_quota_section(
    ui: &mut egui::Ui,
    bucket: Option<&QuotaBucket>,
    title: &str,
    missing_label: Option<&str>,
    language: Language,
    accent: Color32,
    palette: Palette,
) {
    ui.set_width(ui.available_width());

    ui.horizontal(|ui| {
        let section_title = bucket.map(|bucket| bucket.title.as_str()).unwrap_or(title);
        ui.label(
            RichText::new(section_title)
                .size(13.0)
                .strong()
                .color(palette.text_main),
        );
        if bucket.is_none() {
            ui.label(
                RichText::new(missing_label.unwrap_or("--"))
                    .size(11.0)
                    .color(palette.text_subtle),
            );
        }
    });

    ui.add_space(7.0);

    if let Some(bucket) = bucket {
        draw_quota_window_row(ui, &bucket.five_hour, language, accent, palette);
        ui.add_space(5.0);
        draw_quota_window_row(ui, &bucket.weekly, language, accent, palette);
    } else {
        draw_empty_quota_window_row(ui, "5h", language, palette);
        ui.add_space(5.0);
        draw_empty_quota_window_row(
            ui,
            match language {
                Language::Japanese => "週",
                Language::English => "Week",
            },
            language,
            palette,
        );
    }
}

fn draw_quota_window_row(
    ui: &mut egui::Ui,
    window: &QuotaWindow,
    language: Language,
    accent: Color32,
    palette: Palette,
) {
    draw_quota_row(
        ui,
        localized_quota_label(window.label, language),
        &window.remaining_text(),
        &localized_reset_text(&window.reset_text, language),
        language,
        accent,
        palette,
    );
}

fn draw_empty_quota_window_row(
    ui: &mut egui::Ui,
    label: &'static str,
    language: Language,
    palette: Palette,
) {
    draw_quota_row(
        ui,
        label,
        "--",
        "--",
        language,
        palette.text_subtle,
        palette,
    );
}

fn draw_quota_row(
    ui: &mut egui::Ui,
    label: &'static str,
    remaining: &str,
    reset: &str,
    language: Language,
    accent: Color32,
    palette: Palette,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let left = rect.left() + 1.0;
    let center_y = rect.center().y;

    painter.text(
        Pos2::new(left, center_y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(11.0),
        palette.text_muted,
    );
    painter.text(
        Pos2::new(left + 28.0, center_y - 1.0),
        Align2::LEFT_CENTER,
        remaining,
        FontId::proportional(21.0),
        palette.text_main,
    );
    painter.text(
        Pos2::new(left + 78.0, center_y + 4.0),
        Align2::LEFT_CENTER,
        match language {
            Language::Japanese => "残",
            Language::English => "left",
        },
        FontId::proportional(10.0),
        accent,
    );
    painter.text(
        Pos2::new(rect.right() - 2.0, center_y),
        Align2::RIGHT_CENTER,
        compact_text(reset, 12),
        FontId::proportional(10.0),
        palette.text_muted,
    );
}

fn codex_compact_detail(state: &CodexUsageState, language: Language) -> String {
    state
        .content
        .as_ref()
        .map(|content| {
            let tier = match content.service_tier {
                CodexServiceTier::Standard => "S",
                CodexServiceTier::Fast => "F",
                CodexServiceTier::Unknown => "?",
            };
            match language {
                Language::Japanese => format!(
                    "週{} R{} {tier}",
                    content.codex.weekly.remaining_text(),
                    content
                        .reset_credits
                        .available_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "--".to_owned())
                ),
                Language::English => format!(
                    "W{} R{} {tier}",
                    content.codex.weekly.remaining_text(),
                    content
                        .reset_credits
                        .available_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "--".to_owned())
                ),
            }
        })
        .unwrap_or_else(|| match language {
            Language::Japanese => "使用量 --".to_owned(),
            Language::English => "Usage --".to_owned(),
        })
}

fn codex_compact_value(state: &CodexUsageState, language: Language) -> String {
    state
        .content
        .as_ref()
        .map(|content| match language {
            Language::Japanese => format!("5h {}残", content.codex.five_hour.remaining_text()),
            Language::English => format!("5h {} left", content.codex.five_hour.remaining_text()),
        })
        .unwrap_or_else(|| match state.status {
            CodexUsageStatus::Ready => "--".to_owned(),
            status => localized_status(status, language).to_owned(),
        })
}

fn localized_status(status: CodexUsageStatus, language: Language) -> &'static str {
    match (status, language) {
        (CodexUsageStatus::Loading, Language::Japanese) => "読込中",
        (CodexUsageStatus::Ready, Language::Japanese) => "取得済み",
        (CodexUsageStatus::Stale, Language::Japanese) => "更新待ち",
        (CodexUsageStatus::Unavailable, Language::Japanese) => "オフライン",
        (CodexUsageStatus::Loading, Language::English) => "LOADING",
        (CodexUsageStatus::Ready, Language::English) => "READY",
        (CodexUsageStatus::Stale, Language::English) => "STALE",
        (CodexUsageStatus::Unavailable, Language::English) => "OFFLINE",
    }
}

fn localized_service_tier(service_tier: CodexServiceTier, language: Language) -> &'static str {
    match (service_tier, language) {
        (CodexServiceTier::Standard, _) => "Standard",
        (CodexServiceTier::Fast, _) => "Fast",
        (CodexServiceTier::Unknown, Language::Japanese) => "不明",
        (CodexServiceTier::Unknown, Language::English) => "Unknown",
    }
}

fn last_updated_text(state: &CodexUsageState, language: Language) -> String {
    let Some(fetched_at) = state.content.as_ref().map(|content| content.fetched_at) else {
        return match language {
            Language::Japanese => "最終更新 --",
            Language::English => "Last updated --",
        }
        .to_owned();
    };
    let age = unix_now_seconds().saturating_sub(fetched_at).max(0);
    match language {
        Language::Japanese if age < 60 => format!("最終更新 {age}秒前"),
        Language::Japanese => format!("最終更新 {}分前", age / 60),
        Language::English if age < 60 => format!("Last updated {age}s ago"),
        Language::English => format!("Last updated {}m ago", age / 60),
    }
}

fn expiry_text(expires_at: Option<i64>, now_secs: i64, language: Language) -> String {
    let Some(expires_at) = expires_at else {
        return match language {
            Language::Japanese => "有効期限なし",
            Language::English => "No expiry",
        }
        .to_owned();
    };
    let (exact, time_zone) = format_unix_local(expires_at);
    let remaining = expires_at.saturating_sub(now_secs);
    let relative = format_relative_duration(remaining, language);
    match language {
        Language::Japanese => format!("期限 {exact} {time_zone}（{relative}）"),
        Language::English => format!("Expires {exact} {time_zone} ({relative})"),
    }
}

fn format_relative_duration(seconds: i64, language: Language) -> String {
    if seconds <= 0 {
        return match language {
            Language::Japanese => "期限切れ",
            Language::English => "expired",
        }
        .to_owned();
    }

    let minutes = (seconds + 59) / 60;
    let days = minutes / (24 * 60);
    let hours = (minutes % (24 * 60)) / 60;
    let mins = minutes % 60;
    match language {
        Language::Japanese if days > 0 && hours > 0 => format!("あと{days}日{hours}時間"),
        Language::Japanese if days > 0 => format!("あと{days}日"),
        Language::Japanese if hours > 0 && mins > 0 => format!("あと{hours}時間{mins}分"),
        Language::Japanese if hours > 0 => format!("あと{hours}時間"),
        Language::Japanese => format!("あと{mins}分"),
        Language::English if days > 0 && hours > 0 => format!("in {days}d {hours}h"),
        Language::English if days > 0 => format!("in {days}d"),
        Language::English if hours > 0 && mins > 0 => format!("in {hours}h {mins}m"),
        Language::English if hours > 0 => format!("in {hours}h"),
        Language::English => format!("in {mins}m"),
    }
}

fn format_unix_utc(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

fn format_unix_local(timestamp: i64) -> (String, String) {
    system_local_datetime(timestamp)
        .unwrap_or_else(|| (format_unix_utc(timestamp), "UTC".to_owned()))
}

#[cfg(target_os = "macos")]
fn system_local_datetime(timestamp: i64) -> Option<(String, String)> {
    let raw_time: libc::time_t = timestamp;
    let mut local_time = std::mem::MaybeUninit::<libc::tm>::uninit();
    let local_time = unsafe {
        if libc::localtime_r(&raw_time, local_time.as_mut_ptr()).is_null() {
            return None;
        }
        local_time.assume_init()
    };

    let exact = format_local_time_part(&local_time, b"%Y-%m-%d %H:%M\0")?;
    let time_zone = format_local_time_part(&local_time, b"%Z\0")?;
    if time_zone.is_empty() {
        return None;
    }
    Some((exact, time_zone))
}

#[cfg(target_os = "macos")]
fn format_local_time_part(local_time: &libc::tm, format: &[u8]) -> Option<String> {
    let mut output = [0_i8; 64];
    let written = unsafe {
        libc::strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr().cast(),
            local_time,
        )
    };
    if written == 0 {
        return None;
    }

    let value = unsafe { std::ffi::CStr::from_ptr(output.as_ptr()) };
    value.to_str().ok().map(str::to_owned)
}

#[cfg(not(target_os = "macos"))]
fn system_local_datetime(_timestamp: i64) -> Option<(String, String)> {
    None
}

fn civil_date_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn unix_now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn localized_quota_label(label: &'static str, language: Language) -> &'static str {
    match (label, language) {
        ("週", Language::English) => "Week",
        _ => label,
    }
}

fn localized_reset_text(reset: &str, language: Language) -> String {
    if language == Language::Japanese || reset == "--" {
        return reset.to_owned();
    }
    if reset == "まもなく" {
        return "Soon".to_owned();
    }

    let translated = reset
        .strip_prefix("あと")
        .unwrap_or(reset)
        .replace("1分未満", "<1m")
        .replace('日', "d ")
        .replace("時間", "h ")
        .replace('分', "m");
    format!("in {}", translated.trim())
}

fn codex_compact_percent(state: &CodexUsageState) -> f32 {
    state
        .content
        .as_ref()
        .and_then(|content| {
            content
                .codex
                .five_hour
                .remaining_percent
                .or(content.codex.weekly.remaining_percent)
        })
        .map(f32::from)
        .unwrap_or(0.0)
}

fn status_color(status: CodexUsageStatus, palette: Palette) -> Color32 {
    match status {
        CodexUsageStatus::Loading => palette.text_subtle,
        CodexUsageStatus::Ready => palette.accent_green,
        CodexUsageStatus::Stale => palette.warning_amber,
        CodexUsageStatus::Unavailable => palette.error_red,
    }
}

fn draw_codex_links(ui: &mut egui::Ui, language: Language, palette: Palette) {
    ui.horizontal(|ui| {
        ui.hyperlink_to(
            RichText::new(match language {
                Language::Japanese => "使用状況を開く",
                Language::English => "Open usage",
            })
            .size(12.0)
            .strong()
            .underline()
            .color(palette.codex_accent),
            "https://chatgpt.com/codex/settings/usage",
        );
        ui.label(RichText::new(" / ").size(10.0).color(palette.text_subtle));
        ui.hyperlink_to(
            RichText::new(match language {
                Language::Japanese => "ChatGPT 稼働状況",
                Language::English => "ChatGPT status",
            })
            .size(12.0)
            .strong()
            .underline()
            .color(palette.codex_accent),
            "https://status.openai.com",
        );
    });
}

fn cpu_detail(snapshot: &Snapshot, language: Language) -> String {
    match (snapshot.cpu_temp_c, snapshot.temp_source.as_deref()) {
        (Some(temp), Some(source)) if !source.is_empty() => match language {
            Language::Japanese => format!("温度 {temp:.0}℃ ・ {source}"),
            Language::English => format!("Temp {temp:.0}°C · {source}"),
        },
        (Some(temp), _) => match language {
            Language::Japanese => format!("温度 {temp:.0}℃"),
            Language::English => format!("Temp {temp:.0}°C"),
        },
        _ => match language {
            Language::Japanese => "温度 --".to_owned(),
            Language::English => "Temp --".to_owned(),
        },
    }
}

fn compact_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let mut compact = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    compact.push('…');
    compact
}

#[cfg(test)]
mod preference_tests {
    use super::*;

    fn assert_rect_fits(inner: Rect, outer: Rect) {
        let epsilon = 0.01;
        assert!(inner.left() >= outer.left() - epsilon);
        assert!(inner.right() <= outer.right() + epsilon);
        assert!(inner.top() >= outer.top() - epsilon);
        assert!(inner.bottom() <= outer.bottom() + epsilon);
    }

    #[test]
    fn places_codex_window_to_the_right_when_that_side_is_clear() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(2560.0, 1080.0));
        let parent = Rect::from_min_size(Pos2::new(300.0, 250.0), Vec2::new(420.0, 430.0));
        let window_size = Vec2::new(580.0, 600.0);

        let position = adjacent_window_position(parent, window_size, screen);
        let window = Rect::from_min_size(position, window_size);

        assert_eq!(position.x, parent.right() + CODEX_WINDOW_GAP);
        assert_eq!(rect_overlap_area(window, parent), 0.0);
        assert_rect_fits(window, screen);
    }

    #[test]
    fn places_codex_window_to_the_left_near_the_right_screen_edge() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(2560.0, 1080.0));
        let parent = Rect::from_min_size(Pos2::new(2100.0, 250.0), Vec2::new(420.0, 430.0));
        let window_size = Vec2::new(580.0, 600.0);

        let position = adjacent_window_position(parent, window_size, screen);
        let window = Rect::from_min_size(position, window_size);

        assert_eq!(
            window.right(),
            parent.left() - CODEX_WINDOW_GAP,
            "the clear left side should win over a clamped overlapping right candidate"
        );
        assert_eq!(rect_overlap_area(window, parent), 0.0);
        assert_rect_fits(window, screen);
    }

    #[test]
    fn keeps_the_codex_window_visible_on_a_cramped_screen() {
        let screen = Rect::from_min_size(Pos2::new(-800.0, 0.0), Vec2::new(800.0, 700.0));
        let parent = Rect::from_min_size(Pos2::new(-500.0, 140.0), Vec2::new(420.0, 430.0));
        let window_size = Vec2::new(580.0, 600.0);

        let position = adjacent_window_position(parent, window_size, screen);
        let window = Rect::from_min_size(position, window_size);

        assert_rect_fits(window, screen);
    }

    #[test]
    fn selects_the_visible_screen_containing_the_main_window() {
        let left_screen = Rect::from_min_size(Pos2::new(-1920.0, 0.0), Vec2::new(1920.0, 1080.0));
        let main_screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(2560.0, 1080.0));
        let parent = Rect::from_min_size(Pos2::new(-1200.0, 200.0), Vec2::new(420.0, 430.0));

        let selected = [main_screen, left_screen]
            .into_iter()
            .max_by(|left, right| compare_screens_for_parent(*left, *right, parent));

        assert_eq!(selected, Some(left_screen));
    }

    #[test]
    fn parses_supported_locale_forms_with_safe_unknown_result() {
        assert_eq!(
            language_from_locale("ja_JP.UTF-8"),
            Some(Language::Japanese)
        );
        assert_eq!(language_from_locale("en-US"), Some(Language::English));
        assert_eq!(
            language_from_locale("(\"ja-JP\", \"en-JP\")"),
            Some(Language::Japanese)
        );
        assert_eq!(language_from_locale("C"), None);
    }

    #[test]
    fn preference_json_uses_canonical_values_and_round_trips() {
        let preferences = UiPreferences {
            language: LanguageChoice::En,
            theme: ThemeChoice::Dark,
            font_size_points: 21,
            auto_reset: true,
        };
        let json = serde_json::to_string(&preferences).expect("serialize preferences");
        assert_eq!(
            json,
            r#"{"language":"en","theme":"dark","font_size_points":21,"auto_reset":true}"#
        );
        assert_eq!(UiPreferences::from_json(&json), Some(preferences));
        assert_eq!(
            UiPreferences::from_json(r#"{"language":"en","theme":"dark"}"#),
            Some(UiPreferences {
                language: LanguageChoice::En,
                theme: ThemeChoice::Dark,
                font_size_points: UI_FONT_SIZE_DEFAULT_POINTS,
                auto_reset: false,
            })
        );
        assert_eq!(
            UiPreferences::from_json(r#"{"language":"en","theme":"dark","font_size_points":9}"#)
                .map(|preferences| preferences.font_size_points),
            Some(UI_FONT_SIZE_MIN_POINTS)
        );
        assert_eq!(
            UiPreferences::from_json(r#"{"language":"en","theme":"dark","font_size_points":99}"#)
                .map(|preferences| preferences.font_size_points),
            Some(UI_FONT_SIZE_MAX_POINTS)
        );
        assert_eq!(UiPreferences::default().zoom_factor(), 1.0);
        assert_eq!(
            UiPreferences::from_json(r#"{"language":"xx","theme":"dark"}"#),
            None
        );
    }

    #[test]
    fn font_size_step_buttons_move_one_point_and_stop_at_bounds() {
        assert_eq!(increment_ui_font_size(16), 17);
        assert_eq!(decrement_ui_font_size(16), 15);
        assert_eq!(increment_ui_font_size(UI_FONT_SIZE_MAX_POINTS), 32);
        assert_eq!(decrement_ui_font_size(UI_FONT_SIZE_MIN_POINTS), 10);
    }

    #[test]
    fn font_size_is_only_committed_when_confirmation_runs() {
        let mut applied_points = 16;
        let draft_points = increment_ui_font_size(applied_points);

        assert_eq!(applied_points, 16);
        assert_eq!(draft_points, 17);
        assert!(commit_ui_font_size(&mut applied_points, draft_points));
        assert_eq!(applied_points, 17);
        assert!(!commit_ui_font_size(&mut applied_points, draft_points));
    }

    #[test]
    fn localized_labels_and_reset_text_cover_both_languages() {
        assert_eq!(
            language_choice_label(LanguageChoice::System, Language::English),
            "System"
        );
        assert_eq!(
            theme_choice_label(ThemeChoice::Light, Language::Japanese),
            "ライト"
        );
        assert_eq!(localized_quota_label("週", Language::English), "Week");
        assert_eq!(
            localized_reset_text("あと1時間1分", Language::English),
            "in 1h 1m"
        );
        assert_eq!(localized_reset_text("まもなく", Language::English), "Soon");
        assert_eq!(
            codex_details_toggle_label(false, Language::Japanese),
            "管理…"
        );
        assert_eq!(
            codex_details_toggle_label(true, Language::Japanese),
            "閉じる"
        );
        assert_eq!(
            codex_details_toggle_tooltip(false, Language::English),
            "Open Codex controls"
        );
        assert_eq!(
            codex_details_toggle_tooltip(true, Language::English),
            "Close Codex controls"
        );
    }

    #[test]
    fn formats_reset_credit_expiry_in_system_time_zone_and_relative_time() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00");
        assert_eq!(format_unix_utc(86_400), "1970-01-02 00:00");
        let (exact, time_zone) = format_unix_local(86_400);
        assert!(!exact.is_empty());
        assert!(!time_zone.is_empty());
        assert_eq!(
            expiry_text(Some(86_400), 0, Language::Japanese),
            format!("期限 {exact} {time_zone}（あと1日）")
        );
    }
}
