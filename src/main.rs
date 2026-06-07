mod metrics;

use std::{
    fs,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
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
            .with_inner_size([400.0, 300.0])
            .with_min_inner_size([360.0, 280.0])
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
}

impl MonitorApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        register_japanese_font(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);

        let (snapshot, rx) = start_metrics_sampler();

        Self {
            snapshot,
            rx,
            last_update: Instant::now(),
        }
    }

    fn receive_latest_snapshot(&mut self) {
        while let Ok(snapshot) = self.rx.try_recv() {
            self.snapshot = snapshot;
            self.last_update = Instant::now();
        }
    }
}

impl eframe::App for MonitorApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_latest_snapshot();
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
                draw_metric_card(
                    ui,
                    "CPU",
                    "使用率",
                    self.snapshot.cpu_percent,
                    format!("{:.0}", self.snapshot.cpu_percent.clamp(0.0, 100.0)),
                    "%",
                    cpu_detail(&self.snapshot),
                );

                ui.add_space(10.0);
                draw_metric_card(
                    ui,
                    "メモリ",
                    "使用量",
                    self.snapshot.memory_percent,
                    format!(
                        "{:.1}/{:.1}",
                        self.snapshot.memory_used_gib, self.snapshot.memory_total_gib
                    ),
                    "GiB",
                    format!(
                        "使用率 {:.0}%",
                        self.snapshot.memory_percent.clamp(0.0, 100.0)
                    ),
                );
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

fn draw_metric_card(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    percent: f32,
    value: String,
    unit: &str,
    detail: String,
) {
    let available_width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(available_width, 84.0), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12.0, CARD_BG);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, CARD_STROKE),
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(Vec2::new(15.0, 11.0));
    let progress_rect = Rect::from_min_max(
        Pos2::new(inner.right() - 86.0, inner.top() + 9.0),
        Pos2::new(inner.right(), inner.bottom() - 9.0),
    );
    let text_rect = Rect::from_min_max(
        inner.left_top(),
        Pos2::new(progress_rect.left() - 18.0, inner.bottom()),
    );

    ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
        ui.set_clip_rect(text_rect);
        ui.set_width(text_rect.width());

        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(13.0).strong().color(TEXT_MAIN));
            ui.label(RichText::new(subtitle).size(12.0).color(TEXT_MUTED));
        });

        ui.add_space(5.0);
        ui.horizontal(|ui| {
            let value_size = if value.chars().count() > 7 {
                24.0
            } else {
                30.0
            };
            ui.label(RichText::new(value).size(value_size).color(TEXT_MAIN));
            ui.add_space(2.0);
            ui.label(RichText::new(unit).size(12.0).strong().color(ACCENT_GREEN));
        });

        ui.add_space(2.0);
        ui.label(
            RichText::new(compact_text(&detail, 34))
                .size(11.0)
                .color(TEXT_MUTED),
        );
    });

    draw_progress_accent(&painter, progress_rect, percent);
}

fn draw_progress_accent(painter: &egui::Painter, rect: Rect, percent: f32) {
    let clamped = percent.clamp(0.0, 100.0) / 100.0;
    painter.text(
        Pos2::new(rect.center().x, rect.top()),
        Align2::CENTER_TOP,
        format!("{:.0}%", percent.clamp(0.0, 100.0)),
        FontId::proportional(18.0),
        TEXT_MAIN,
    );
    painter.text(
        Pos2::new(rect.center().x, rect.top() + 24.0),
        Align2::CENTER_TOP,
        "使用率",
        FontId::proportional(10.0),
        TEXT_SUBTLE,
    );

    let track = Rect::from_min_size(
        Pos2::new(rect.left(), rect.bottom() - 8.0),
        Vec2::new(rect.width(), 5.0),
    );
    painter.rect_filled(track, 3.0, TRACK_BG);

    let fill = Rect::from_min_size(
        track.left_top(),
        Vec2::new(track.width() * clamped, track.height()),
    );
    painter.rect_filled(fill, 3.0, ACCENT_GREEN);
    painter.circle_filled(
        Pos2::new(
            fill.right().clamp(track.left(), track.right()),
            track.center().y,
        ),
        2.5,
        ACCENT_GREEN,
    );
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
