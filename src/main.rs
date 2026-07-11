mod codex_usage;
mod metrics;

use std::{
    env, fs,
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use codex_usage::{
    CodexUsageContent, CodexUsagePoller, CodexUsageState, CodexUsageStatus, QuotaBucket,
    QuotaWindow,
};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect,
    RichText, Sense, Stroke, StrokeKind, TextStyle, Vec2,
};
use metrics::{MetricsSampler, Snapshot};
use serde::{Deserialize, Serialize};

const APP_TITLE: &str = "システムモニター";
const PREFERENCES_STORAGE_KEY: &str = "mini-system-monitor-rs.ui-preferences.v1";
const FULL_WINDOW_SIZE: [f32; 2] = [420.0, 430.0];
const FULL_MIN_WINDOW_SIZE: [f32; 2] = [390.0, 410.0];
const COMPACT_WINDOW_SIZE: [f32; 2] = [420.0, 170.0];
const COMPACT_MIN_WINDOW_SIZE: [f32; 2] = [360.0, 150.0];
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct UiPreferences {
    language: LanguageChoice,
    theme: ThemeChoice,
}

impl UiPreferences {
    fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
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
    spark_accent: Color32,
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
                spark_accent: Color32::from_rgb(246, 190, 82),
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
                spark_accent: Color32::from_rgb(169, 111, 14),
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
    display_mode: DisplayMode,
    preferences: UiPreferences,
    system_language: Language,
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
        apply_preferences(&cc.egui_ctx, preferences, system_language);

        let (snapshot, rx) = start_metrics_sampler();
        let codex_rx = start_codex_usage_sampler();

        Self {
            snapshot,
            rx,
            last_update: Instant::now(),
            codex_usage: CodexUsageState::loading(),
            codex_rx,
            display_mode: DisplayMode::Full,
            preferences,
            system_language,
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
    }
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
                    draw_codex_usage_card(ui, &self.codex_usage, language, palette);
                }
                DisplayMode::Compact => {
                    should_toggle_mode |= draw_toggle_space(ui, 11.0);
                    draw_compact_card(ui, &self.snapshot, &self.codex_usage, language, palette);
                }
            }

            if draw_preferences(ui, &mut self.preferences, language) {
                apply_preferences(ui.ctx(), self.preferences, self.system_language);
            }

            should_toggle_mode |= draw_remaining_toggle_space(ui);
        });

        if should_toggle_mode {
            self.display_mode = self.display_mode.toggled();
            apply_display_mode_size(ui.ctx(), self.display_mode);
        }
    }
}

fn apply_preferences(ctx: &egui::Context, preferences: UiPreferences, system_language: Language) {
    ctx.set_theme(preferences.theme.egui_preference());
    let title = match preferences.language.resolve(system_language) {
        Language::Japanese => APP_TITLE,
        Language::English => "System Monitor",
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.to_owned()));
}

fn draw_preferences(
    ui: &mut egui::Ui,
    preferences: &mut UiPreferences,
    language: Language,
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
                .width(70.0)
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
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(13.0, FontFamily::Proportional));
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

fn start_codex_usage_sampler() -> Receiver<CodexUsageState> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut poller = CodexUsagePoller::new();

        loop {
            let state = poller.refresh();
            let delay = poller.next_delay();

            if tx.send(state).is_err() {
                break;
            }

            thread::sleep(delay);
        }
    });

    rx
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
    language: Language,
    palette: Palette,
) {
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
            title: "Codex",
            value: codex_compact_value(state, language),
            detail: codex_compact_detail(state, language),
            percent: codex_compact_percent(state),
            accent: status_color(state.status, palette),
        },
        palette,
    );
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
    language: Language,
    palette: Palette,
) {
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

    let links_height = 24.0;
    let links_rect = Rect::from_min_max(
        Pos2::new(inner.left(), inner.bottom() - links_height),
        inner.right_bottom(),
    );
    let content_rect = Rect::from_min_max(
        Pos2::new(inner.left(), inner.top() + 31.0),
        Pos2::new(inner.right(), links_rect.top() - 7.0),
    );

    ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
        ui.set_clip_rect(content_rect);
        ui.set_width(content_rect.width());
        draw_codex_usage_content(ui, state.content.as_ref(), language, palette);
    });

    ui.scope_builder(egui::UiBuilder::new().max_rect(links_rect), |ui| {
        ui.set_clip_rect(links_rect);
        ui.set_width(links_rect.width());
        ui.add_space(3.0);
        draw_codex_links(ui, language, palette);
    });
}

fn draw_codex_usage_content(
    ui: &mut egui::Ui,
    content: Option<&CodexUsageContent>,
    language: Language,
    palette: Palette,
) {
    ui.columns(2, |columns| match content {
        Some(content) => {
            draw_quota_section(
                &mut columns[0],
                Some(&content.codex),
                "Codex",
                None,
                language,
                palette.codex_accent,
                palette,
            );
            draw_quota_section(
                &mut columns[1],
                content.spark.as_ref(),
                "Spark",
                Some(match language {
                    Language::Japanese => "未検出",
                    Language::English => "Not detected",
                }),
                language,
                palette.spark_accent,
                palette,
            );
        }
        None => {
            draw_quota_section(
                &mut columns[0],
                None,
                "Codex",
                Some(match language {
                    Language::Japanese => "未取得",
                    Language::English => "Unavailable",
                }),
                language,
                palette.codex_accent,
                palette,
            );
            draw_quota_section(
                &mut columns[1],
                None,
                "Spark",
                Some(match language {
                    Language::Japanese => "未検出",
                    Language::English => "Not detected",
                }),
                language,
                palette.spark_accent,
                palette,
            );
        }
    });
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
        .map(|content| match language {
            Language::Japanese => format!("週 {}残", content.codex.weekly.remaining_text()),
            Language::English => format!("Week {} left", content.codex.weekly.remaining_text()),
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
    ui.horizontal_centered(|ui| {
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
                Language::Japanese => "ステータスページ",
                Language::English => "Status page",
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
        };
        let json = serde_json::to_string(&preferences).expect("serialize preferences");
        assert_eq!(json, r#"{"language":"en","theme":"dark"}"#);
        assert_eq!(UiPreferences::from_json(&json), Some(preferences));
        assert_eq!(
            UiPreferences::from_json(r#"{"language":"xx","theme":"dark"}"#),
            None
        );
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
    }
}
