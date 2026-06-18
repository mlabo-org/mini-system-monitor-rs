mod codex_usage;
mod metrics;

use std::{
    fs,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use codex_usage::{
    CodexUsageContent, CodexUsagePoller, CodexUsageState, CodexUsageStatus, QuotaBucket,
    QuotaWindow,
};
use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, TextStyle, Vec2,
};
use metrics::{MetricsSampler, Snapshot};

const APP_TITLE: &str = "システムモニター";
const ACCENT_GREEN: Color32 = Color32::from_rgb(78, 210, 132);
const PANEL_BG: Color32 = Color32::from_rgba_premultiplied(17, 19, 23, 246);
const PANEL_STROKE: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 14);
const CARD_BG: Color32 = Color32::from_rgba_premultiplied(27, 30, 36, 238);
const CARD_STROKE: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 12);
const TRACK_BG: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 20);
const TEXT_MAIN: Color32 = Color32::from_rgb(238, 243, 240);
const TEXT_MUTED: Color32 = Color32::from_rgb(151, 161, 156);
const TEXT_SUBTLE: Color32 = Color32::from_rgb(104, 115, 110);
const CODEX_ACCENT: Color32 = Color32::from_rgb(91, 159, 255);
const SPARK_ACCENT: Color32 = Color32::from_rgb(246, 190, 82);
const WARNING_AMBER: Color32 = Color32::from_rgb(238, 170, 83);
const ERROR_RED: Color32 = Color32::from_rgb(236, 100, 95);
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

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 430.0])
            .with_min_inner_size([390.0, 410.0])
            .with_resizable(false)
            .with_transparent(true)
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
}

impl MonitorApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        register_japanese_font(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);

        let (snapshot, rx) = start_metrics_sampler();
        let codex_rx = start_codex_usage_sampler();

        Self {
            snapshot,
            rx,
            last_update: Instant::now(),
            codex_usage: CodexUsageState::loading(),
            codex_rx,
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_latest_snapshot();
        self.receive_latest_codex_usage();
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let rect = ui.max_rect().shrink2(Vec2::new(8.0, 7.0));
        let painter = ui.painter();
        painter.rect_filled(rect, 15.0, PANEL_BG);
        painter.rect_stroke(
            rect,
            15.0,
            Stroke::new(1.0, PANEL_STROKE),
            StrokeKind::Inside,
        );

        ui.scope_builder(
            egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(18.0, 15.0))),
            |ui| {
                draw_header(ui, self.last_update);

                ui.add_space(13.0);
                draw_system_card(ui, &self.snapshot);

                ui.add_space(10.0);
                draw_codex_usage_card(ui, &self.codex_usage);
            },
        );
    }
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
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = PANEL_BG;
    visuals.extreme_bg_color = Color32::from_rgb(12, 14, 17);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.global_style()).clone();
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
    ctx.set_global_style(style);
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

fn draw_header(ui: &mut egui::Ui, last_update: Instant) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(APP_TITLE)
                .size(18.0)
                .strong()
                .color(TEXT_MAIN),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let age = last_update.elapsed().as_secs_f32();
            let color = if age < 1.8 { ACCENT_GREEN } else { TEXT_MUTED };
            ui.label(RichText::new("●").size(11.0).color(color));
            ui.label(RichText::new("LIVE").size(10.0).color(TEXT_SUBTLE));
        });
    });
}

fn draw_system_card(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let available_width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(available_width, 112.0), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12.0, CARD_BG);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, CARD_STROKE),
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(15.0, 12.0));
    painter.text(
        inner.left_top(),
        Align2::LEFT_TOP,
        "System",
        FontId::proportional(14.0),
        TEXT_MAIN,
    );
    painter.text(
        inner.right_top(),
        Align2::RIGHT_TOP,
        "CPU / メモリ",
        FontId::proportional(10.0),
        TEXT_SUBTLE,
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
        Stroke::new(1.0, PANEL_STROKE),
    );

    draw_system_metric(
        &painter,
        cpu_rect,
        "CPU",
        format!("{:.0}%", snapshot.cpu_percent.clamp(0.0, 100.0)),
        cpu_detail(snapshot),
        snapshot.cpu_percent,
        ACCENT_GREEN,
    );
    draw_system_metric(
        &painter,
        memory_rect,
        "メモリ",
        format!(
            "{:.1}/{:.1} GiB",
            snapshot.memory_used_gib, snapshot.memory_total_gib
        ),
        format!("使用率 {:.0}%", snapshot.memory_percent.clamp(0.0, 100.0)),
        snapshot.memory_percent,
        CODEX_ACCENT,
    );
}

fn draw_system_metric(
    painter: &egui::Painter,
    rect: Rect,
    title: &str,
    value: String,
    detail: String,
    percent: f32,
    accent: Color32,
) {
    painter.text(
        rect.left_top(),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(12.0),
        TEXT_MUTED,
    );
    painter.text(
        Pos2::new(rect.left(), rect.top() + 17.0),
        Align2::LEFT_TOP,
        value,
        FontId::proportional(22.0),
        TEXT_MAIN,
    );
    painter.text(
        Pos2::new(rect.left(), rect.top() + 44.0),
        Align2::LEFT_TOP,
        compact_text(&detail, 22),
        FontId::proportional(10.0),
        TEXT_MUTED,
    );

    draw_metric_track(painter, rect, percent, accent);
}

fn draw_metric_track(painter: &egui::Painter, rect: Rect, percent: f32, accent: Color32) {
    let clamped = percent.clamp(0.0, 100.0) / 100.0;
    let track = Rect::from_min_size(
        Pos2::new(rect.left(), rect.bottom() - 5.0),
        Vec2::new(rect.width(), 4.0),
    );
    painter.rect_filled(track, 2.0, TRACK_BG);

    let fill = Rect::from_min_size(
        track.left_top(),
        Vec2::new(track.width() * clamped, track.height()),
    );
    painter.rect_filled(fill, 2.0, accent);
}

fn draw_codex_usage_card(ui: &mut egui::Ui, state: &CodexUsageState) {
    let available_width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(available_width, 194.0), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12.0, CARD_BG);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, CARD_STROKE),
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(15.0, 12.0));

    painter.text(
        inner.left_top(),
        Align2::LEFT_TOP,
        "Codex 使用量",
        FontId::proportional(14.0),
        TEXT_MAIN,
    );
    painter.text(
        Pos2::new(inner.right(), inner.top() + 1.0),
        Align2::RIGHT_TOP,
        state.status.label(),
        FontId::proportional(10.0),
        status_color(state.status),
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
        draw_codex_usage_content(ui, state.content.as_ref());
    });

    ui.scope_builder(egui::UiBuilder::new().max_rect(links_rect), |ui| {
        ui.set_clip_rect(links_rect);
        ui.set_width(links_rect.width());
        ui.add_space(3.0);
        draw_codex_links(ui);
    });
}

fn draw_codex_usage_content(ui: &mut egui::Ui, content: Option<&CodexUsageContent>) {
    ui.columns(2, |columns| match content {
        Some(content) => {
            draw_quota_section(
                &mut columns[0],
                Some(&content.codex),
                "Codex",
                None,
                CODEX_ACCENT,
            );
            draw_quota_section(
                &mut columns[1],
                content.spark.as_ref(),
                "Spark",
                Some("未検出"),
                SPARK_ACCENT,
            );
        }
        None => {
            draw_quota_section(&mut columns[0], None, "Codex", Some("未取得"), CODEX_ACCENT);
            draw_quota_section(&mut columns[1], None, "Spark", Some("未検出"), SPARK_ACCENT);
        }
    });
}

fn draw_quota_section(
    ui: &mut egui::Ui,
    bucket: Option<&QuotaBucket>,
    title: &str,
    missing_label: Option<&str>,
    accent: Color32,
) {
    ui.set_width(ui.available_width());

    ui.horizontal(|ui| {
        let section_title = bucket.map(|bucket| bucket.title.as_str()).unwrap_or(title);
        ui.label(
            RichText::new(section_title)
                .size(13.0)
                .strong()
                .color(TEXT_MAIN),
        );
        if bucket.is_none() {
            ui.label(
                RichText::new(missing_label.unwrap_or("--"))
                    .size(11.0)
                    .color(TEXT_SUBTLE),
            );
        }
    });

    ui.add_space(7.0);

    if let Some(bucket) = bucket {
        draw_quota_window_row(ui, &bucket.five_hour, accent);
        ui.add_space(5.0);
        draw_quota_window_row(ui, &bucket.weekly, accent);
    } else {
        draw_empty_quota_window_row(ui, "5h");
        ui.add_space(5.0);
        draw_empty_quota_window_row(ui, "週");
    }
}

fn draw_quota_window_row(ui: &mut egui::Ui, window: &QuotaWindow, accent: Color32) {
    draw_quota_row(
        ui,
        window.label,
        &window.remaining_text(),
        &window.reset_text,
        accent,
    );
}

fn draw_empty_quota_window_row(ui: &mut egui::Ui, label: &'static str) {
    draw_quota_row(ui, label, "--", "--", TEXT_SUBTLE);
}

fn draw_quota_row(
    ui: &mut egui::Ui,
    label: &'static str,
    remaining: &str,
    reset: &str,
    accent: Color32,
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
        TEXT_MUTED,
    );
    painter.text(
        Pos2::new(left + 28.0, center_y - 1.0),
        Align2::LEFT_CENTER,
        remaining,
        FontId::proportional(21.0),
        TEXT_MAIN,
    );
    painter.text(
        Pos2::new(left + 78.0, center_y + 4.0),
        Align2::LEFT_CENTER,
        "残",
        FontId::proportional(10.0),
        accent,
    );
    painter.text(
        Pos2::new(rect.right() - 2.0, center_y),
        Align2::RIGHT_CENTER,
        compact_text(reset, 12),
        FontId::proportional(10.0),
        TEXT_MUTED,
    );
}

fn status_color(status: CodexUsageStatus) -> Color32 {
    match status {
        CodexUsageStatus::Loading => TEXT_SUBTLE,
        CodexUsageStatus::Ready => ACCENT_GREEN,
        CodexUsageStatus::Stale => WARNING_AMBER,
        CodexUsageStatus::Unavailable => ERROR_RED,
    }
}

fn draw_codex_links(ui: &mut egui::Ui) {
    ui.horizontal_centered(|ui| {
        ui.hyperlink_to(
            RichText::new("使用状況を開く")
                .size(12.0)
                .strong()
                .underline()
                .color(CODEX_ACCENT),
            "https://chatgpt.com/codex/settings/usage",
        );
        ui.label(RichText::new(" / ").size(10.0).color(TEXT_SUBTLE));
        ui.hyperlink_to(
            RichText::new("Status page")
                .size(12.0)
                .strong()
                .underline()
                .color(CODEX_ACCENT),
            "https://status.openai.com",
        );
    });
}

fn cpu_detail(snapshot: &Snapshot) -> String {
    match (snapshot.cpu_temp_c, snapshot.temp_source.as_deref()) {
        (Some(temp), Some(source)) if !source.is_empty() => {
            format!("温度 {temp:.0}℃ ・ {source}")
        }
        (Some(temp), _) => format!("温度 {temp:.0}℃"),
        _ => "温度 --".to_owned(),
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
