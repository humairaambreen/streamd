mod engine;
mod mpv;

use eframe::{
    egui::{
        self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, IconData, Key, Layout,
        Margin, Mesh, PaintCallback, Pos2, Rect, Sense, Shape, Stroke, TextureHandle,
        TextureOptions, Vec2, ViewportCommand,
    },
    egui_glow,
};
use engine::{Engine, OnlineSub, OpenReq, SessionMeta, SessionStatus};
use mpv::{MpvPlayer, PlayerSnapshot};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn expo_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(4)
}

fn load_app_icon() -> Option<IconData> {
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?;
    let rgba = img.to_rgba8();
    Some(IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Loads a single clean modern geometric sans-serif font (SF Pro on macOS, Segoe UI on Windows).
fn setup_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = &[
        #[cfg(target_os = "macos")]
        "/System/Library/Fonts/SFNS.ttf",
        #[cfg(target_os = "macos")]
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        #[cfg(target_os = "windows")]
        "C:\\Windows\\Fonts\\segoeui.ttf",
        #[cfg(all(unix, not(target_os = "macos")))]
        "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
        #[cfg(all(unix, not(target_os = "macos")))]
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "clean_sans".to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "clean_sans".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "clean_sans".to_owned());
            ctx.set_fonts(fonts);
            break;
        }
    }
}

fn fmt_bytes(b: u64) -> String {
    if b == 0 {
        return "0 B".into();
    }
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i >= 2 {
        format!("{v:.1} {}", U[i])
    } else {
        format!("{v:.0} {}", U[i])
    }
}

fn fmt_time(sec: f64) -> String {
    if !sec.is_finite() || sec < 0.0 {
        return "0:00".into();
    }
    let s = sec.floor() as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let r = s % 60;
    if h > 0 {
        format!("{h}:{m:02}:{r:02}")
    } else {
        format!("{m}:{r:02}")
    }
}

fn fmt_remaining(sec: f64) -> String {
    if !sec.is_finite() || sec <= 0.0 {
        return String::new();
    }
    let mins = (sec / 60.0).ceil() as u64;
    let h = mins / 60;
    let m = mins % 60;
    if h > 0 {
        format!("{h}h {m}m left")
    } else {
        format!("{m}m left")
    }
}

fn res_label(w: i64, h: i64) -> String {
    if w >= 3800 || h >= 2100 {
        "4K".into()
    } else if w >= 2500 || h >= 1400 {
        "1440p".into()
    } else if w >= 1800 || h >= 1000 {
        "1080p".into()
    } else if w >= 1200 || h >= 700 {
        "720p".into()
    } else if h > 0 {
        format!("{h}p")
    } else {
        String::new()
    }
}

fn clean_title(raw: &str) -> (String, Option<String>) {
    let base = raw
        .rsplit_once('.')
        .map(|(a, _)| a)
        .unwrap_or(raw)
        .replace(['.', '_'], " ");
    let words: Vec<&str> = base.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if w.len() == 4
            && w.chars().all(|c| c.is_ascii_digit())
            && (w.starts_with("19") || w.starts_with("20"))
        {
            let t = words[..i].join(" ");
            return (
                if t.is_empty() { words.join(" ") } else { t },
                Some((*w).into()),
            );
        }
    }
    (words.join(" "), None)
}

fn is_sub_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("srt" | "vtt" | "ass" | "ssa" | "sub" | "idx")
    )
}

fn is_video_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some(
            "mkv"
                | "mp4"
                | "mov"
                | "m4v"
                | "webm"
                | "avi"
                | "ts"
                | "m2ts"
                | "flv"
                | "wmv"
                | "mpg"
                | "mpeg"
                | "part"
                | "crdownload"
        )
    )
}

fn sibling_videos(path: &Path) -> (Vec<PathBuf>, usize) {
    let Some(parent) = path.parent() else {
        return (vec![path.to_path_buf()], 0);
    };
    let mut list: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.is_file() && is_video_ext(p))
        .collect();
    list.sort_by(|a, b| {
        a.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .cmp(
                &b.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase(),
            )
    });
    let idx = list.iter().position(|p| p == path).unwrap_or(0);
    if list.is_empty() {
        (vec![path.to_path_buf()], 0)
    } else {
        (list, idx)
    }
}

// ---------- Smooth GPU vertex-interpolated fade gradients ----------
fn paint_fade_v(p: &egui::Painter, rect: Rect, top_col: Color32, bot_col: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top_col);
    mesh.colored_vertex(rect.right_top(), top_col);
    mesh.colored_vertex(rect.right_bottom(), bot_col);
    mesh.colored_vertex(rect.left_bottom(), bot_col);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));
}

fn paint_fade_h(p: &egui::Painter, rect: Rect, left_col: Color32, right_col: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), left_col);
    mesh.colored_vertex(rect.right_top(), right_col);
    mesh.colored_vertex(rect.right_bottom(), right_col);
    mesh.colored_vertex(rect.left_bottom(), left_col);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));
}

// ---------- Crisp custom vector icons ----------
fn icon_btn(
    ui: &mut egui::Ui,
    size: Vec2,
    draw: impl FnOnce(&egui::Painter, Rect, Color32),
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(255, 255, 255, 20),
        );
    }
    let col = if resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 220)
    };
    draw(ui.painter(), rect, col);
    resp
}

fn draw_play(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    p.add(Shape::convex_polygon(
        vec![
            Pos2::new(c.x - 4.5, c.y - 6.0),
            Pos2::new(c.x + 6.5, c.y),
            Pos2::new(c.x - 4.5, c.y + 6.0),
        ],
        col,
        Stroke::NONE,
    ));
}

fn draw_pause(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    p.rect_filled(
        Rect::from_min_size(Pos2::new(c.x - 5.0, c.y - 6.0), Vec2::new(3.2, 12.0)),
        CornerRadius::ZERO,
        col,
    );
    p.rect_filled(
        Rect::from_min_size(Pos2::new(c.x + 1.8, c.y - 6.0), Vec2::new(3.2, 12.0)),
        CornerRadius::ZERO,
        col,
    );
}

fn draw_next(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    p.add(Shape::convex_polygon(
        vec![
            Pos2::new(c.x - 5.5, c.y - 5.0),
            Pos2::new(c.x + 2.5, c.y),
            Pos2::new(c.x - 5.5, c.y + 5.0),
        ],
        col,
        Stroke::NONE,
    ));
    p.rect_filled(
        Rect::from_min_size(Pos2::new(c.x + 3.5, c.y - 5.0), Vec2::new(2.0, 10.0)),
        CornerRadius::ZERO,
        col,
    );
}

fn draw_prev(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    p.rect_filled(
        Rect::from_min_size(Pos2::new(c.x - 5.5, c.y - 5.0), Vec2::new(2.0, 10.0)),
        CornerRadius::ZERO,
        col,
    );
    p.add(Shape::convex_polygon(
        vec![
            Pos2::new(c.x + 5.5, c.y - 5.0),
            Pos2::new(c.x - 2.5, c.y),
            Pos2::new(c.x + 5.5, c.y + 5.0),
        ],
        col,
        Stroke::NONE,
    ));
}

fn draw_speaker(p: &egui::Painter, r: Rect, col: Color32, muted: bool) {
    let c = r.center();
    let cone = vec![
        Pos2::new(c.x - 6.5, c.y - 2.5),
        Pos2::new(c.x - 3.0, c.y - 2.5),
        Pos2::new(c.x + 1.0, c.y - 6.0),
        Pos2::new(c.x + 1.0, c.y + 6.0),
        Pos2::new(c.x - 3.0, c.y + 2.5),
        Pos2::new(c.x - 6.5, c.y + 2.5),
    ];
    p.add(Shape::convex_polygon(cone, col, Stroke::NONE));
    let st = Stroke::new(1.5_f32, col);
    if muted {
        p.line_segment(
            [
                Pos2::new(c.x + 3.0, c.y - 3.0),
                Pos2::new(c.x + 7.5, c.y + 3.0),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x + 7.5, c.y - 3.0),
                Pos2::new(c.x + 3.0, c.y + 3.0),
            ],
            st,
        );
    } else {
        p.line_segment(
            [
                Pos2::new(c.x + 3.8, c.y - 3.0),
                Pos2::new(c.x + 3.8, c.y + 3.0),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x + 6.8, c.y - 5.0),
                Pos2::new(c.x + 6.8, c.y + 5.0),
            ],
            st,
        );
    }
}

fn draw_fullscreen(p: &egui::Painter, r: Rect, col: Color32, exit: bool) {
    let c = r.center();
    let s = 5.5;
    let d = 3.5;
    let st = Stroke::new(1.5_f32, col);
    if !exit {
        p.line_segment(
            [Pos2::new(c.x - s, c.y - s + d), Pos2::new(c.x - s, c.y - s)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x - s, c.y - s), Pos2::new(c.x - s + d, c.y - s)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x + s - d, c.y - s), Pos2::new(c.x + s, c.y - s)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x + s, c.y - s), Pos2::new(c.x + s, c.y - s + d)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x - s, c.y + s - d), Pos2::new(c.x - s, c.y + s)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x - s, c.y + s), Pos2::new(c.x - s + d, c.y + s)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x + s - d, c.y + s), Pos2::new(c.x + s, c.y + s)],
            st,
        );
        p.line_segment(
            [Pos2::new(c.x + s, c.y + s), Pos2::new(c.x + s, c.y + s - d)],
            st,
        );
    } else {
        p.line_segment(
            [
                Pos2::new(c.x - s, c.y - 2.0),
                Pos2::new(c.x - 2.0, c.y - 2.0),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x - 2.0, c.y - 2.0),
                Pos2::new(c.x - 2.0, c.y - s),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x + s, c.y - 2.0),
                Pos2::new(c.x + 2.0, c.y - 2.0),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x + 2.0, c.y - 2.0),
                Pos2::new(c.x + 2.0, c.y - s),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x - s, c.y + 2.0),
                Pos2::new(c.x - 2.0, c.y + 2.0),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x - 2.0, c.y + 2.0),
                Pos2::new(c.x - 2.0, c.y + s),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x + s, c.y + 2.0),
                Pos2::new(c.x + 2.0, c.y + 2.0),
            ],
            st,
        );
        p.line_segment(
            [
                Pos2::new(c.x + 2.0, c.y + 2.0),
                Pos2::new(c.x + 2.0, c.y + s),
            ],
            st,
        );
    }
}

fn draw_back(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    let st = Stroke::new(1.6_f32, col);
    p.line_segment(
        [Pos2::new(c.x + 3.0, c.y - 5.5), Pos2::new(c.x - 3.0, c.y)],
        st,
    );
    p.line_segment(
        [Pos2::new(c.x - 3.0, c.y), Pos2::new(c.x + 3.0, c.y + 5.5)],
        st,
    );
}

fn draw_chevron_right(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    let st = Stroke::new(1.5_f32, col);
    p.line_segment(
        [Pos2::new(c.x - 2.5, c.y - 4.5), Pos2::new(c.x + 2.5, c.y)],
        st,
    );
    p.line_segment(
        [Pos2::new(c.x + 2.5, c.y), Pos2::new(c.x - 2.5, c.y + 4.5)],
        st,
    );
}

fn more_options_row(ui: &mut egui::Ui, width: f32, label: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
    let bg = if resp.hovered() {
        Color32::from_rgba_unmultiplied(255, 255, 255, 22)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 10)
    };
    ui.painter().rect_filled(rect, CornerRadius::ZERO, bg);
    ui.painter().text(
        Pos2::new(rect.left() + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        Color32::WHITE,
    );
    let chev_r = Rect::from_min_size(
        Pos2::new(rect.right() - 24.0, rect.top()),
        Vec2::new(20.0, rect.height()),
    );
    draw_chevron_right(
        ui.painter(),
        chev_r,
        Color32::from_rgba_unmultiplied(255, 255, 255, 190),
    );
    resp.clicked()
}

fn draw_menu(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    let st = Stroke::new(1.5_f32, col);
    for dy in [-4.0, 0.0, 4.0] {
        p.line_segment(
            [
                Pos2::new(c.x - 6.5, c.y + dy),
                Pos2::new(c.x + 6.5, c.y + dy),
            ],
            st,
        );
    }
}

// ---------- Unified headless hairline seekbar ----------
fn hairline_bar(
    ui: &mut egui::Ui,
    width: f32,
    val: f64,
    min: f64,
    max: f64,
    center_tick: Option<f64>,
) -> Option<f64> {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::click_and_drag());
    let hovered = resp.hovered() || resp.dragged();
    let rail_h = if hovered { 4.0 } else { 2.0 };
    let rail = Rect::from_min_size(
        Pos2::new(rect.left(), rect.center().y - rail_h * 0.5),
        Vec2::new(rect.width(), rail_h),
    );
    let p = ui.painter();
    p.rect_filled(
        rail,
        CornerRadius::ZERO,
        Color32::from_rgba_unmultiplied(255, 255, 255, 48),
    );

    let span = (max - min).max(1e-6);
    let norm = ((val - min) / span).clamp(0.0, 1.0) as f32;

    if let Some(mid) = center_tick {
        let mid_norm = ((mid - min) / span).clamp(0.0, 1.0) as f32;
        let x0 = rail.left() + rail.width() * mid_norm.min(norm);
        let x1 = rail.left() + rail.width() * mid_norm.max(norm);
        p.rect_filled(
            Rect::from_min_max(Pos2::new(x0, rail.top()), Pos2::new(x1, rail.bottom())),
            CornerRadius::ZERO,
            Color32::WHITE,
        );
        let cx = rail.left() + rail.width() * mid_norm;
        p.rect_filled(
            Rect::from_center_size(Pos2::new(cx, rail.center().y), Vec2::new(1.5, 8.0)),
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(255, 255, 255, 130),
        );
    } else {
        p.rect_filled(
            Rect::from_min_size(rail.min, Vec2::new(rail.width() * norm, rail_h)),
            CornerRadius::ZERO,
            Color32::WHITE,
        );
    }

    if (resp.clicked() || resp.dragged()) && rect.width() > 0.0 {
        if let Some(pos) = resp.interact_pointer_pos() {
            let p_norm = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
            return Some(min + span * p_norm);
        }
    }
    None
}

fn labeled_bar(
    ui: &mut egui::Ui,
    w: f32,
    label: &str,
    val_text: String,
    val: f64,
    min: f64,
    max: f64,
    center: Option<f64>,
) -> (Option<f64>, bool) {
    let mut reset = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .color(Color32::from_gray(155)),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(egui::RichText::new(val_text).size(12.0))
                        .fill(Color32::TRANSPARENT),
                )
                .clicked()
            {
                reset = true;
            }
        });
    });
    let changed = hairline_bar(ui, w, val, min, max, center);
    (changed, reset)
}

#[derive(Clone)]
struct LibraryItem {
    name: String,
    source: String, // URL or local path
    is_local: bool,
    position: f64,
    duration: f64,
    thumb: Option<TextureHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptTab {
    Audio,
    Subs,
    Video,
}

impl OptTab {
    fn idx(self) -> f32 {
        match self {
            OptTab::Audio => 0.0,
            OptTab::Subs => 1.0,
            OptTab::Video => 2.0,
        }
    }
}

struct StreamdApp {
    engine: Arc<Engine>,
    mpv: Option<Arc<MpvPlayer>>,
    mpv_err: Option<String>,

    // Home Cover-Flow stage & inputs
    url: String,
    show_hdrs: bool,
    hdrs_anim: f32,
    referer: String,
    cookie: String,
    user_agent: String,
    seed_file: Option<PathBuf>,
    opening: bool,
    open_rx: Option<mpsc::Receiver<anyhow::Result<SessionMeta>>>,
    err_msg: String,
    library: Vec<LibraryItem>,
    carousel_target: usize,
    carousel_pos: f32,
    scroll_accum: f32,
    last_scroll_step: Instant,
    expand_t: f32,
    closing_morph: bool,
    pending_resume_pos: Option<f64>,
    card_thumb_rx: mpsc::Receiver<(String, ColorImage)>,
    card_thumb_tx: mpsc::Sender<(String, ColorImage)>,

    // Active session & Play Next queue
    session: Option<SessionMeta>,
    queue: Vec<PathBuf>,
    queue_idx: usize,
    title: String,
    year: Option<String>,
    snap: PlayerSnapshot,
    status: SessionStatus,
    last_status_poll: Instant,

    // Studio state toggles
    quality_cap: i64, // 0 = Original, 1080, 720, 480
    aspect_mode: String,
    upscaler: String,
    night_mode: bool,
    stereo_downmix: bool,

    // Online subtitle search
    sub_query: String,
    sub_lang: String,
    sub_msg: String,
    sub_results: Vec<OnlineSub>,
    sub_search_rx: Option<mpsc::Receiver<anyhow::Result<Vec<OnlineSub>>>>,
    sub_dl_rx: Option<mpsc::Receiver<anyhow::Result<PathBuf>>>,

    // Chrome, Quick Options & Full Studio Panel morphing
    last_mouse_move: Instant,
    last_mouse_pos: Option<Pos2>,
    chrome_t: f32,
    pause_t: f32,
    pause_since: Option<Instant>,
    opts_open: bool,
    opts_t: f32,
    opt_tab: OptTab,
    tab_slide_x: f32,
    studio_tab: Option<OptTab>,
    studio_t: f32,
    is_fullscreen: bool,

    // Text-only HUD
    hud_text: Option<String>,
    hud_until: Instant,

    // Scrub preview GPU texture cache
    thumb_cache: HashMap<u64, TextureHandle>,
    thumb_gen: Arc<AtomicU64>,
    thumb_rx: mpsc::Receiver<(u64, ColorImage)>,
    thumb_tx: mpsc::Sender<(u64, ColorImage)>,
    last_thumb_bucket: i64,
    active_thumb: Option<(u64, TextureHandle)>,
    hover_x_smooth: f32,
    hover_t: f32,
}

impl StreamdApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        engine: Arc<Engine>,
        initial_arg: Option<String>,
    ) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(Color32::WHITE);
        visuals.panel_fill = Color32::BLACK;
        visuals.window_fill = Color32::from_rgba_unmultiplied(10, 10, 12, 224);
        visuals.window_stroke = Stroke::NONE;
        visuals.window_corner_radius = CornerRadius::ZERO;
        visuals.widgets.noninteractive.bg_stroke = Stroke::NONE;
        visuals.widgets.noninteractive.corner_radius = CornerRadius::ZERO;
        visuals.widgets.inactive.bg_stroke = Stroke::NONE;
        visuals.widgets.inactive.corner_radius = CornerRadius::ZERO;
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 16);
        visuals.widgets.hovered.bg_stroke = Stroke::NONE;
        visuals.widgets.hovered.corner_radius = CornerRadius::ZERO;
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 28);
        visuals.widgets.active.bg_stroke = Stroke::NONE;
        visuals.widgets.active.corner_radius = CornerRadius::ZERO;
        visuals.widgets.active.weak_bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 42);
        visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 55);
        visuals.selection.stroke = Stroke::NONE;
        cc.egui_ctx.set_visuals(visuals);
        setup_fonts(&cc.egui_ctx);

        let (mpv, mpv_err) = match cc.get_proc_address {
            Some(gpa) => match MpvPlayer::new(cc.egui_ctx.clone(), gpa) {
                Ok(p) => (Some(p), None),
                Err(e) => (None, Some(e.to_string())),
            },
            None => (None, Some("OpenGL proc address loader unavailable".into())),
        };

        let (thumb_tx, thumb_rx) = mpsc::channel();
        let (card_thumb_tx, card_thumb_rx) = mpsc::channel();

        let mut app = Self {
            engine,
            mpv,
            mpv_err,
            url: String::new(),
            show_hdrs: false,
            hdrs_anim: 0.0,
            referer: String::new(),
            cookie: String::new(),
            user_agent: String::new(),
            seed_file: None,
            opening: false,
            open_rx: None,
            err_msg: String::new(),
            library: Vec::new(),
            carousel_target: 0,
            carousel_pos: 0.0,
            scroll_accum: 0.0,
            last_scroll_step: Instant::now() - Duration::from_secs(1),
            expand_t: 0.0,
            closing_morph: false,
            pending_resume_pos: None,
            card_thumb_rx,
            card_thumb_tx,
            session: None,
            queue: Vec::new(),
            queue_idx: 0,
            title: String::new(),
            year: None,
            snap: PlayerSnapshot::default(),
            status: SessionStatus::default(),
            last_status_poll: Instant::now(),
            quality_cap: 0,
            aspect_mode: "auto".into(),
            upscaler: "spline36".into(),
            night_mode: false,
            stereo_downmix: false,
            sub_query: String::new(),
            sub_lang: "eng".into(),
            sub_msg: String::new(),
            sub_results: Vec::new(),
            sub_search_rx: None,
            sub_dl_rx: None,
            last_mouse_move: Instant::now(),
            last_mouse_pos: None,
            chrome_t: 1.0,
            pause_t: 0.0,
            pause_since: None,
            opts_open: false,
            opts_t: 0.0,
            opt_tab: OptTab::Audio,
            tab_slide_x: 0.0,
            studio_tab: None,
            studio_t: 0.0,
            is_fullscreen: false,
            hud_text: None,
            hud_until: Instant::now(),
            thumb_cache: HashMap::new(),
            thumb_gen: Arc::new(AtomicU64::new(0)),
            thumb_rx,
            thumb_tx,
            last_thumb_bucket: -1,
            active_thumb: None,
            hover_x_smooth: 0.0,
            hover_t: 0.0,
        };

        if let Some(arg) = initial_arg {
            let p = PathBuf::from(arg.strip_prefix("file://").unwrap_or(&arg));
            if p.is_file() {
                app.open_local_path(p, true);
            } else if arg.starts_with("http://") || arg.starts_with("https://") {
                app.url = arg;
                app.start_open_url();
            }
        }

        app
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.hud_text = Some(msg.into());
        self.hud_until = Instant::now() + Duration::from_millis(750);
    }

    fn start_open_url(&mut self) {
        if self.opening || self.url.trim().is_empty() {
            return;
        }
        let trimmed = self.url.trim();
        let maybe_path = PathBuf::from(trimmed.strip_prefix("file://").unwrap_or(trimmed));
        if maybe_path.is_file() {
            self.open_local_path(maybe_path, true);
            return;
        }

        self.opening = true;
        self.err_msg.clear();
        self.queue.clear();
        self.queue_idx = 0;
        let req = OpenReq {
            url: trimmed.to_string(),
            referer: self.referer.clone(),
            cookie: self.cookie.clone(),
            user_agent: self.user_agent.clone(),
            seed_file: self.seed_file.clone(),
        };
        let eng = self.engine.clone();
        let (tx, rx) = mpsc::channel();
        self.open_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(eng.open(req));
        });
    }

    fn open_local_path(&mut self, path: PathBuf, discover_siblings: bool) {
        self.err_msg.clear();
        if discover_siblings {
            let (q, idx) = sibling_videos(&path);
            self.queue = q;
            self.queue_idx = idx;
        }
        match self.engine.open_local(path) {
            Ok(meta) => self.activate_session(meta),
            Err(e) => self.err_msg = e.to_string(),
        }
    }

    fn play_queue_offset(&mut self, delta: isize) {
        if self.queue.len() <= 1 {
            return;
        }
        let next = self.queue_idx as isize + delta;
        if next < 0 || next as usize >= self.queue.len() {
            return;
        }
        self.queue_idx = next as usize;
        let path = self.queue[self.queue_idx].clone();
        self.thumb_gen.fetch_add(1, Relaxed);
        self.thumb_cache.clear();
        self.active_thumb = None;
        self.last_thumb_bucket = -1;
        self.open_local_path(path, false);
        self.flash(self.title.clone());
    }

    fn spawn_card_thumb(
        &self,
        source_key: String,
        sess_id: String,
        local_path: Option<String>,
        at_sec: u64,
        ctx: &egui::Context,
    ) {
        let eng = self.engine.clone();
        let tx = self.card_thumb_tx.clone();
        let egui_ctx = ctx.clone();
        let dummy_gen = Arc::new(AtomicU64::new(1));
        std::thread::spawn(move || {
            if let Some(jpeg) =
                eng.fetch_thumb(&sess_id, local_path.as_deref(), at_sec, &dummy_gen, 1)
            {
                if let Ok(img) =
                    image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                {
                    let rgba = img.to_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        rgba.as_raw(),
                    );
                    let _ = tx.send((source_key, ci));
                    egui_ctx.request_repaint();
                }
            }
        });
    }

    fn activate_session(&mut self, meta: SessionMeta) {
        let (t, y) = clean_title(&meta.name);
        self.title = t.clone();
        self.year = y;
        self.sub_query = t.clone();
        self.sub_results.clear();
        self.sub_msg.clear();
        self.closing_morph = false;
        self.expand_t = 0.0;

        let source = if meta.is_local {
            meta.media_url.clone()
        } else {
            self.url.trim().to_string()
        };
        if !source.is_empty() {
            if let Some(pos_idx) = self.library.iter().position(|x| x.source == source) {
                self.carousel_target = pos_idx + 1;
            } else {
                self.library.insert(
                    0,
                    LibraryItem {
                        name: t,
                        source,
                        is_local: meta.is_local,
                        position: 0.0,
                        duration: 0.0,
                        thumb: None,
                    },
                );
                self.carousel_target = 1;
            }
            self.carousel_pos = self.carousel_target as f32;
        }

        if let Some(mpv) = &self.mpv {
            mpv.load_url(&meta.media_url);
        }
        self.session = Some(meta);
        self.last_mouse_move = Instant::now();
    }

    fn begin_close_morph(&mut self, ctx: &egui::Context) {
        if self.closing_morph {
            return;
        }
        // Save real metadata title, exact paused position, and capture exact paused frame thumbnail!
        if let Some(s) = self.session.clone() {
            let src = if s.is_local {
                s.media_url.clone()
            } else {
                self.url.trim().to_string()
            };
            let cap_sec = self.snap.position.max(2.0).round() as u64;
            if let Some(item) = self.library.iter_mut().find(|x| x.source == src) {
                if !self.snap.media_title.is_empty() {
                    item.name = self.snap.media_title.clone();
                } else if !self.title.is_empty() {
                    item.name = self.title.clone();
                }
                if self.snap.duration > 0.0 {
                    item.position = self.snap.position;
                    item.duration = self.snap.duration;
                }
            }
            let lp = s.is_local.then(|| s.media_url.clone());
            self.spawn_card_thumb(src, s.id.clone(), lp, cap_sec, ctx);
        }
        self.opts_open = false;
        self.studio_tab = None;
        self.closing_morph = true;
    }

    fn close_session(&mut self) {
        self.thumb_gen.fetch_add(1, Relaxed);
        if let Some(mpv) = &self.mpv {
            mpv.stop();
        }
        if let Some(s) = self.session.take() {
            if !s.is_local {
                self.engine.close(&s.id);
            }
        }
        self.closing_morph = false;
        self.expand_t = 0.0;
        self.pending_resume_pos = None;
        self.queue.clear();
        self.queue_idx = 0;
        self.thumb_cache.clear();
        self.active_thumb = None;
        self.last_thumb_bucket = -1;
        self.opts_open = false;
        self.studio_tab = None;
        self.pause_t = 0.0;
        self.pause_since = None;
        self.snap = PlayerSnapshot::default();
        self.status = SessionStatus::default();
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        let mut videos = Vec::new();
        for f in dropped {
            if let Some(path) = f.path {
                if is_sub_ext(&path) {
                    if let Some(mpv) = &self.mpv {
                        mpv.add_sub_file(&path.to_string_lossy());
                        self.flash("Subtitle loaded");
                    }
                } else {
                    videos.push(path);
                }
            }
        }
        if !videos.is_empty() {
            if self.session.is_some() {
                self.close_session();
            }
            if videos.len() > 1 {
                self.queue = videos;
                self.queue_idx = 0;
                let first = self.queue[0].clone();
                self.open_local_path(first, false);
            } else {
                self.open_local_path(videos.remove(0), true);
            }
        }
    }

    fn request_thumb(&mut self, bucket: u64, ctx: &egui::Context) {
        if let Some(tex) = self.thumb_cache.get(&bucket) {
            self.active_thumb = Some((bucket, tex.clone()));
            self.last_thumb_bucket = bucket as i64;
            return;
        }
        if self.last_thumb_bucket == bucket as i64 {
            return;
        }
        if let Some((b, _)) = &self.active_thumb {
            if b.abs_diff(bucket) > 30 {
                self.active_thumb = None;
            }
        }
        self.last_thumb_bucket = bucket as i64;
        let Some(sess) = &self.session else { return };
        let id = sess.id.clone();
        let local_path = sess.is_local.then(|| sess.media_url.clone());
        let eng = self.engine.clone();
        let gen_arc = self.thumb_gen.clone();
        let my_gen = gen_arc.fetch_add(1, Relaxed) + 1;
        let tx = self.thumb_tx.clone();
        let egui_ctx = ctx.clone();

        std::thread::spawn(move || {
            if let Some(jpeg) =
                eng.fetch_thumb(&id, local_path.as_deref(), bucket, &gen_arc, my_gen)
            {
                if gen_arc.load(Relaxed) != my_gen {
                    return;
                }
                if let Ok(img) =
                    image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let ci = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let _ = tx.send((bucket, ci));
                    egui_ctx.request_repaint();
                }
            }
        });
    }

    /// Collects keyboard actions inside `ctx.input`, then dispatches viewport commands OUTSIDE
    /// to prevent egui RwLock deadlocks on macOS fullscreen toggles.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.session.is_none() || ctx.wants_keyboard_input() {
            return;
        }
        let Some(mpv) = self.mpv.clone() else { return };

        let mut want_fs_cmd: Option<bool> = None;
        let mut want_queue_delta: isize = 0;

        ctx.input(|i| {
            if i.key_pressed(Key::Space) || i.key_pressed(Key::K) {
                mpv.toggle_pause();
                self.last_mouse_move = Instant::now();
            }
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::J) {
                mpv.seek_relative(-10.0);
                self.flash("−10s");
                self.last_mouse_move = Instant::now();
            }
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::L) {
                mpv.seek_relative(10.0);
                self.flash("+10s");
                self.last_mouse_move = Instant::now();
            }
            if i.key_pressed(Key::ArrowUp) {
                let v = (self.snap.volume + 5.0).min(300.0);
                mpv.set_volume(v);
                self.flash(format!("{}%", v.round() as i64));
                self.last_mouse_move = Instant::now();
            }
            if i.key_pressed(Key::ArrowDown) {
                let v = (self.snap.volume - 5.0).max(0.0);
                mpv.set_volume(v);
                self.flash(format!("{}%", v.round() as i64));
                self.last_mouse_move = Instant::now();
            }
            if i.key_pressed(Key::M) {
                mpv.toggle_mute();
                self.flash(if self.snap.muted { "Unmuted" } else { "Muted" });
                self.last_mouse_move = Instant::now();
            }
            if i.key_pressed(Key::F) {
                self.is_fullscreen = !self.is_fullscreen;
                want_fs_cmd = Some(self.is_fullscreen);
            }
            if i.modifiers.shift && i.key_pressed(Key::N) {
                want_queue_delta = 1;
            }
            if i.modifiers.shift && i.key_pressed(Key::P) {
                want_queue_delta = -1;
            }
            if i.key_pressed(Key::C) {
                if self.snap.active_sid > 0 {
                    mpv.set_sub_track(0);
                    self.flash("Subtitles off");
                } else if let Some(first) = self.snap.sub_tracks.first() {
                    mpv.set_sub_track(first.id);
                    self.flash("Subtitles on");
                }
            }
            if i.key_pressed(Key::Escape) {
                if self.studio_tab.is_some() {
                    self.studio_tab = None;
                } else if self.opts_open {
                    self.opts_open = false;
                } else if self.is_fullscreen {
                    self.is_fullscreen = false;
                    want_fs_cmd = Some(false);
                }
            }
            let digits = [
                (Key::Num0, 0),
                (Key::Num1, 1),
                (Key::Num2, 2),
                (Key::Num3, 3),
                (Key::Num4, 4),
                (Key::Num5, 5),
                (Key::Num6, 6),
                (Key::Num7, 7),
                (Key::Num8, 8),
                (Key::Num9, 9),
            ];
            for (k, d) in digits {
                if i.key_pressed(k) && self.snap.duration > 0.0 {
                    mpv.seek_absolute(self.snap.duration * (d as f64 / 10.0));
                    self.flash(format!("{}%", d * 10));
                    self.last_mouse_move = Instant::now();
                }
            }
        });

        // Executed OUTSIDE ctx.input() so macOS fullscreen never deadlocks!
        if let Some(fs) = want_fs_cmd {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(fs));
        }
        if want_queue_delta != 0 {
            self.play_queue_offset(want_queue_delta);
        }
    }

    fn draw_home(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.05);
        let total_cards = 1 + self.library.len();
        if self.carousel_target >= total_cards {
            self.carousel_target = total_cards.saturating_sub(1);
        }

        // Debounced Cover-Flow navigation: 1 trackpad swipe = exactly 1 card (never skips middle!)
        if !ctx.wants_keyboard_input() && total_cards > 1 {
            ctx.input(|i| {
                if i.key_pressed(Key::ArrowRight) && self.carousel_target + 1 < total_cards {
                    self.carousel_target += 1;
                }
                if i.key_pressed(Key::ArrowLeft) && self.carousel_target > 0 {
                    self.carousel_target -= 1;
                }
                let scroll = if i.raw_scroll_delta.x.abs() > i.raw_scroll_delta.y.abs() {
                    i.raw_scroll_delta.x
                } else {
                    i.raw_scroll_delta.y
                };
                if scroll.abs() > 0.5 {
                    self.scroll_accum += scroll;
                    if self.last_scroll_step.elapsed() >= Duration::from_millis(260) {
                        if self.scroll_accum < -38.0 && self.carousel_target + 1 < total_cards {
                            self.carousel_target += 1;
                            self.scroll_accum = 0.0;
                            self.last_scroll_step = Instant::now();
                        } else if self.scroll_accum > 38.0 && self.carousel_target > 0 {
                            self.carousel_target -= 1;
                            self.scroll_accum = 0.0;
                            self.last_scroll_step = Instant::now();
                        }
                    }
                } else {
                    self.scroll_accum *= 0.8;
                }
            });
        }

        let target_f = self.carousel_target as f32;
        self.carousel_pos += (target_f - self.carousel_pos) * (dt * 12.0).min(1.0);
        let target_h = if self.show_hdrs && self.carousel_target == 0 {
            1.0
        } else {
            0.0
        };
        self.hdrs_anim += (target_h - self.hdrs_anim) * (dt * 14.0).min(1.0);

        if (self.carousel_pos - target_f).abs() > 0.002 || (self.hdrs_anim - target_h).abs() > 0.002
        {
            ctx.request_repaint();
        }

        const BG: Color32 = Color32::from_rgb(14, 14, 15); // #0E0E0F
        const CARD_DARK: Color32 = Color32::from_rgb(37, 37, 40); // #252528
        const CARD_EDGE: Color32 = Color32::from_rgb(58, 58, 61); // #3A3A3D
        const CARD_PAPER: Color32 = Color32::from_rgb(232, 232, 227); // #E8E8E3
        const ACCENT_BLUE: Color32 = Color32::from_rgb(77, 148, 255); // #4D94FF
        const TEXT_MAIN: Color32 = Color32::from_rgb(232, 232, 227);

        let mut open_card_idx: Option<(usize, bool)> = None; // (library_idx, start_over)

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(BG))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let cx = rect.center().x;
                let cy = rect.center().y - 34.0;
                let scale = (rect.width() / 960.0).clamp(0.85, 1.35);

                let card_geom = |d: f32| -> (Rect, f32) {
                    let ad = d.abs();
                    let w = if ad <= 1.0 {
                        400.0 + (288.0 - 400.0) * ad
                    } else {
                        (288.0 + (220.0 - 288.0) * (ad - 1.0).min(1.0)).max(180.0)
                    } * scale;
                    let h = w * 9.0 / 16.0;
                    let sign = if d >= 0.0 { 1.0 } else { -1.0 };
                    let x_off = if ad <= 1.0 {
                        sign * ad * 300.0 * scale
                    } else {
                        sign * (300.0 + (ad - 1.0) * 190.0) * scale
                    };
                    let dim = if ad <= 1.0 {
                        ad * 0.45
                    } else {
                        (0.45 + (ad - 1.0) * 0.25).min(0.78)
                    };
                    let r = Rect::from_center_size(Pos2::new(cx + x_off, cy), Vec2::new(w, h));
                    (r, dim)
                };

                let mut order: Vec<usize> = (0..total_cards).collect();
                order.sort_by(|&a, &b| {
                    let da = (a as f32 - self.carousel_pos).abs();
                    let db = (b as f32 - self.carousel_pos).abs();
                    db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
                });

                // 1. Floor reflections (smoothly fade out when headers drawer expands so zero overlap!)
                let refl_vis = 1.0 - expo_out(self.hdrs_anim);
                if refl_vis > 0.02 {
                    for &idx in &order {
                        let d = idx as f32 - self.carousel_pos;
                        if d.abs() > 2.6 {
                            continue;
                        }
                        let (cr, dim) = card_geom(d);
                        let ref_h = (cr.height() * 0.36).min(82.0 * scale);
                        let rr = Rect::from_min_size(
                            Pos2::new(cr.left(), cr.bottom() + 3.0),
                            Vec2::new(cr.width(), ref_h),
                        );
                        let base_col = if idx == 0 { CARD_PAPER } else { CARD_DARK };
                        let top_a = ((1.0 - dim) * 85.0 * refl_vis) as u8;
                        paint_fade_v(
                            ui.painter(),
                            rr,
                            Color32::from_rgba_unmultiplied(
                                base_col.r(),
                                base_col.g(),
                                base_col.b(),
                                top_a,
                            ),
                            Color32::TRANSPARENT,
                        );
                    }
                }

                // 2. Render Cover-Flow cards back-to-front
                for &idx in &order {
                    let d = idx as f32 - self.carousel_pos;
                    if d.abs() > 2.6 {
                        continue;
                    }
                    let (cr, dim) = card_geom(d);
                    let is_center = idx == self.carousel_target && d.abs() < 0.15;

                    if idx == 0 {
                        ui.painter().rect_filled(cr, CornerRadius::ZERO, CARD_PAPER);
                        ui.painter().rect_stroke(
                            cr,
                            CornerRadius::ZERO,
                            Stroke::new(1.0_f32, Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );

                        let pad_x = cr.width() * 0.07;
                        let bar_h = (46.0 * (cr.width() / (400.0 * scale))).clamp(28.0, 50.0);
                        let bar_r = Rect::from_min_size(
                            Pos2::new(cr.left() + pad_x, cr.center().y - bar_h * 0.5),
                            Vec2::new(cr.width() - pad_x * 2.0, bar_h),
                        );
                        ui.painter()
                            .rect_filled(bar_r, CornerRadius::ZERO, Color32::WHITE);

                        let act_r = Rect::from_min_size(
                            Pos2::new(bar_r.right() - bar_h, bar_r.top()),
                            Vec2::new(bar_h, bar_h),
                        );
                        ui.painter().rect_filled(act_r, CornerRadius::ZERO, BG);

                        // Morph button icon: '+' when empty (pick file) -> crisp Play triangle when URL pasted!
                        if !self.url.trim().is_empty() {
                            draw_play(ui.painter(), act_r, TEXT_MAIN);
                        } else {
                            let pc = act_r.center();
                            let arm = bar_h * 0.17;
                            let pst = Stroke::new(1.9_f32, TEXT_MAIN);
                            ui.painter().line_segment(
                                [Pos2::new(pc.x, pc.y - arm), Pos2::new(pc.x, pc.y + arm)],
                                pst,
                            );
                            ui.painter().line_segment(
                                [Pos2::new(pc.x - arm, pc.y), Pos2::new(pc.x + arm, pc.y)],
                                pst,
                            );
                        }

                        if is_center {
                            let input_r = Rect::from_min_max(
                                bar_r.min,
                                Pos2::new(act_r.left(), bar_r.bottom()),
                            );
                            ui.scope_builder(egui::UiBuilder::new().max_rect(input_r), |ui| {
                                ui.visuals_mut().override_text_color =
                                    Some(Color32::from_rgb(20, 20, 22));
                                ui.visuals_mut().selection.bg_fill =
                                    Color32::from_rgba_unmultiplied(77, 148, 255, 70);
                                let te = egui::TextEdit::singleline(&mut self.url)
                                    .hint_text("Paste a link or open a file")
                                    .font(FontId::proportional(13.0))
                                    .frame(false)
                                    .margin(Margin::symmetric(12, ((bar_h - 18.0) * 0.5) as i8))
                                    .desired_width(input_r.width());
                                let resp = ui.add_sized(input_r.size(), te);
                                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                    self.start_open_url();
                                }
                            });

                            let act_resp =
                                ui.interact(act_r, egui::Id::new("cf_plus_btn"), Sense::click());
                            if act_resp.clicked() {
                                if !self.url.trim().is_empty() {
                                    self.start_open_url();
                                } else if let Some(p) = rfd::FileDialog::new().pick_file() {
                                    self.open_local_path(p, true);
                                }
                            }
                        } else {
                            ui.painter().text(
                                Pos2::new(bar_r.left() + 12.0, bar_r.center().y),
                                Align2::LEFT_CENTER,
                                "Paste a link or open a file",
                                FontId::proportional(
                                    (12.0 * (cr.width() / (400.0 * scale))).max(9.0),
                                ),
                                Color32::from_rgb(85, 85, 79),
                            );
                            if ui
                                .interact(cr, egui::Id::new("cf_card_0"), Sense::click())
                                .clicked()
                            {
                                self.carousel_target = 0;
                            }
                        }
                    } else {
                        // Video Library Card with real paused/metadata thumbnail!
                        let item = &self.library[idx - 1];
                        ui.painter().rect_filled(cr, CornerRadius::ZERO, CARD_DARK);

                        if let Some(tex) = &item.thumb {
                            ui.painter().image(
                                tex.id(),
                                cr,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                            // Smooth bottom fade for legible title & time
                            paint_fade_v(
                                ui.painter(),
                                Rect::from_min_size(
                                    Pos2::new(cr.left(), cr.bottom() - cr.height() * 0.45),
                                    Vec2::new(cr.width(), cr.height() * 0.45),
                                ),
                                Color32::TRANSPARENT,
                                Color32::from_rgba_unmultiplied(0, 0, 0, 210),
                            );
                        }

                        ui.painter().rect_stroke(
                            cr,
                            CornerRadius::ZERO,
                            Stroke::new(1.0_f32, CARD_EDGE),
                            egui::StrokeKind::Inside,
                        );

                        if is_center {
                            draw_play(
                                ui.painter(),
                                cr,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 215),
                            );
                        }

                        let fs = (12.0 * (cr.width() / (400.0 * scale))).clamp(9.5, 13.0);
                        let status_str = if item.duration > 0.0 {
                            format!("{} / {}", fmt_time(item.position), fmt_time(item.duration))
                        } else {
                            "NEW".into()
                        };
                        let short_name: String = item.name.chars().take(28).collect();
                        ui.painter().text(
                            Pos2::new(cr.left() + 10.0, cr.bottom() - 14.0),
                            Align2::LEFT_CENTER,
                            short_name,
                            FontId::proportional(fs),
                            TEXT_MAIN,
                        );
                        ui.painter().text(
                            Pos2::new(cr.right() - 10.0, cr.bottom() - 14.0),
                            Align2::RIGHT_CENTER,
                            status_str,
                            FontId::proportional(fs),
                            TEXT_MAIN,
                        );

                        let pg_r = Rect::from_min_size(
                            Pos2::new(cr.left(), cr.bottom() - 3.0),
                            Vec2::new(cr.width(), 3.0),
                        );
                        ui.painter()
                            .rect_filled(pg_r, CornerRadius::ZERO, CARD_EDGE);
                        if item.duration > 0.0 {
                            let pct = (item.position / item.duration).clamp(0.0, 1.0) as f32;
                            ui.painter().rect_filled(
                                Rect::from_min_size(pg_r.min, Vec2::new(pg_r.width() * pct, 3.0)),
                                CornerRadius::ZERO,
                                ACCENT_BLUE,
                            );
                        }

                        let resp = ui.interact(cr, egui::Id::new(("cf_card", idx)), Sense::click());
                        if resp.clicked() {
                            if is_center {
                                open_card_idx = Some((idx - 1, false));
                            } else {
                                self.carousel_target = idx;
                            }
                        }
                    }

                    if dim > 0.01 {
                        ui.painter().rect_filled(
                            cr,
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(14, 14, 15, (dim * 255.0) as u8),
                        );
                    }
                }

                if !ctx.wants_keyboard_input()
                    && self.carousel_target > 0
                    && ctx.input(|i| i.key_pressed(Key::Enter))
                {
                    open_card_idx = Some((self.carousel_target - 1, false));
                }

                // Clean utility row below center card (Start Over for resumed video, or More options for Input card)
                let foot_w = 400.0 * scale;
                let foot_y = cy + (400.0 * scale * 9.0 / 16.0) * 0.5 + 16.0;
                let foot_r = Rect::from_min_size(
                    Pos2::new(cx - foot_w * 0.5, foot_y),
                    Vec2::new(foot_w, 220.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(foot_r), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 8.0);
                    if self.carousel_target > 0 {
                        if let Some(item) = self.library.get(self.carousel_target - 1) {
                            if item.position > 3.0 {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new("Start Over")
                                                    .size(12.0)
                                                    .color(Color32::from_gray(155)),
                                            )
                                            .fill(Color32::TRANSPARENT),
                                        )
                                        .clicked()
                                    {
                                        open_card_idx = Some((self.carousel_target - 1, true));
                                    }
                                });
                            }
                        }
                    } else {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(16.0, 0.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(if self.show_hdrs {
                                            "Hide options"
                                        } else {
                                            "More options"
                                        })
                                        .size(12.0)
                                        .color(Color32::from_gray(140)),
                                    )
                                    .fill(Color32::TRANSPARENT),
                                )
                                .clicked()
                            {
                                self.show_hdrs = !self.show_hdrs;
                            }
                            if self.opening {
                                ui.label(
                                    egui::RichText::new("Opening…")
                                        .size(12.0)
                                        .color(ACCENT_BLUE),
                                );
                            }
                        });

                        if self.hdrs_anim > 0.02 {
                            let a = expo_out(self.hdrs_anim);
                            ui.scope(|ui| {
                                ui.set_opacity(a);
                                ui.spacing_mut().item_spacing = Vec2::new(0.0, 6.0);
                                for (val, hint) in [
                                    (&mut self.referer, "Referer"),
                                    (&mut self.cookie, "Cookie"),
                                    (&mut self.user_agent, "User-Agent"),
                                ] {
                                    let r = Rect::from_min_size(
                                        ui.cursor().min,
                                        Vec2::new(foot_w, 32.0),
                                    );
                                    ui.painter().rect_filled(r, CornerRadius::ZERO, CARD_DARK);
                                    ui.add_sized(
                                        Vec2::new(foot_w, 32.0),
                                        egui::TextEdit::singleline(val)
                                            .hint_text(hint)
                                            .font(FontId::proportional(12.0))
                                            .frame(false)
                                            .margin(Margin::symmetric(10, 7)),
                                    );
                                }
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(
                                                    "Attach partial file (.part / .crdownload)",
                                                )
                                                .size(11.5)
                                                .color(TEXT_MAIN),
                                            )
                                            .fill(CARD_DARK),
                                        )
                                        .clicked()
                                    {
                                        self.seed_file = rfd::FileDialog::new().pick_file();
                                    }
                                    if let Some(sf) = &self.seed_file.clone() {
                                        let n = sf
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("part");
                                        ui.label(
                                            egui::RichText::new(n)
                                                .size(11.5)
                                                .color(Color32::from_gray(150)),
                                        );
                                        if ui
                                            .add(egui::Button::new("x").fill(Color32::TRANSPARENT))
                                            .clicked()
                                        {
                                            self.seed_file = None;
                                        }
                                    }
                                });
                            });
                        }
                    }

                    if let Some(e) = &self.mpv_err {
                        ui.label(
                            egui::RichText::new(e)
                                .size(12.0)
                                .color(Color32::from_gray(200)),
                        );
                    }
                    if !self.err_msg.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.err_msg)
                                .size(12.0)
                                .color(Color32::from_gray(220)),
                        );
                    }
                });
            });

        if let Some((li, start_over)) = open_card_idx {
            if let Some(item) = self.library.get(li).cloned() {
                self.pending_resume_pos = if !start_over && item.position > 3.0 {
                    Some(item.position)
                } else {
                    None
                };
                if item.is_local {
                    self.open_local_path(PathBuf::from(item.source), true);
                } else {
                    self.url = item.source;
                    self.start_open_url();
                }
            }
        }
    }

    fn draw_player(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.05);

        if let Some(mpv) = &self.mpv {
            self.snap = mpv.poll_state();
            // Adopt container metadata title when available
            if !self.snap.media_title.is_empty() && self.title != self.snap.media_title {
                self.title = self.snap.media_title.clone();
            }
            // Resume from last watched position once duration is loaded
            if let Some(pos) = self.pending_resume_pos {
                if self.snap.duration > 0.0 {
                    mpv.seek_absolute(pos);
                    self.pending_resume_pos = None;
                }
            }
        }

        // Smooth Card <-> Fullscreen morph animation
        let exp_target = if self.closing_morph { 0.0 } else { 1.0 };
        self.expand_t += (exp_target - self.expand_t) * (dt * 11.0).min(1.0);
        if (self.expand_t - exp_target).abs() > 0.003 {
            ctx.request_repaint();
        }
        if self.closing_morph && self.expand_t <= 0.04 {
            self.close_session();
            return;
        }
        if self.last_status_poll.elapsed() >= Duration::from_millis(500) {
            if let Some(s) = &self.session {
                if !s.is_local {
                    if let Some(st) = self.engine.status(&s.id) {
                        self.status = st;
                    }
                }
            }
            self.last_status_poll = Instant::now();
        }

        if let Some(rx) = &self.sub_search_rx {
            if let Ok(res) = rx.try_recv() {
                self.sub_search_rx = None;
                match res {
                    Ok(list) => {
                        self.sub_msg = if list.is_empty() {
                            "No results".into()
                        } else {
                            String::new()
                        };
                        self.sub_results = list;
                    }
                    Err(e) => self.sub_msg = format!("Search failed: {e}"),
                }
            }
        }
        if let Some(rx) = &self.sub_dl_rx {
            if let Ok(res) = rx.try_recv() {
                self.sub_dl_rx = None;
                match res {
                    Ok(path) => {
                        if let Some(mpv) = &self.mpv {
                            mpv.add_sub_file(&path.to_string_lossy());
                        }
                        self.sub_msg = "Loaded".into();
                        self.flash("Subtitle loaded");
                    }
                    Err(e) => self.sub_msg = format!("Download failed: {e}"),
                }
            }
        }

        while let Ok((bucket, ci)) = self.thumb_rx.try_recv() {
            let tex = ctx.load_texture(format!("thumb_{bucket}"), ci, TextureOptions::LINEAR);
            if self.thumb_cache.len() > 80 {
                if let Some(&k) = self.thumb_cache.keys().next() {
                    self.thumb_cache.remove(&k);
                }
            }
            self.thumb_cache.insert(bucket, tex.clone());
            if self.last_thumb_bucket == bucket as i64 {
                self.active_thumb = Some((bucket, tex));
            }
        }

        let pointer_pos = ctx.input(|i| i.pointer.hover_pos());
        if pointer_pos != self.last_mouse_pos {
            self.last_mouse_pos = pointer_pos;
            if pointer_pos.is_some() {
                self.last_mouse_move = Instant::now();
            }
        }

        let want_chrome = self.snap.paused
            || self.opts_open
            || self.studio_tab.is_some()
            || self.last_mouse_move.elapsed() < Duration::from_millis(2400);
        let chrome_target = if want_chrome { 1.0 } else { 0.0 };
        self.chrome_t += (chrome_target - self.chrome_t) * (dt * 11.0).min(1.0);

        if self.snap.paused && !self.opts_open && self.studio_tab.is_none() {
            if self.pause_since.is_none() {
                self.pause_since = Some(Instant::now());
            }
        } else {
            self.pause_since = None;
        }
        let want_pause_overlay = self
            .pause_since
            .map_or(false, |t| t.elapsed() > Duration::from_millis(240));
        let pause_target = if want_pause_overlay { 1.0 } else { 0.0 };
        self.pause_t += (pause_target - self.pause_t) * (dt * 9.0).min(1.0);

        let opts_target = if self.opts_open && self.studio_tab.is_none() {
            1.0
        } else {
            0.0
        };
        self.opts_t += (opts_target - self.opts_t) * (dt * 14.0).min(1.0);

        let studio_target = if self.studio_tab.is_some() { 1.0 } else { 0.0 };
        self.studio_t += (studio_target - self.studio_t) * (dt * 13.0).min(1.0);

        let tab_target = self.opt_tab.idx();
        self.tab_slide_x += (tab_target - self.tab_slide_x) * (dt * 16.0).min(1.0);

        if (self.chrome_t - chrome_target).abs() > 0.003
            || (self.pause_t - pause_target).abs() > 0.003
            || (self.opts_t - opts_target).abs() > 0.003
            || (self.studio_t - studio_target).abs() > 0.003
            || (self.tab_slide_x - tab_target).abs() > 0.003
        {
            ctx.request_repaint();
        }

        let mut want_fs_toggle = false;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(14, 14, 15)))
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();
                let exp = expo_out(self.expand_t);

                // Compute smooth morphed rect from centered 400x225 Cover-Flow card -> full window!
                let scale = (full_rect.width() / 960.0).clamp(0.85, 1.35);
                let card_w = 400.0 * scale;
                let card_h = card_w * 9.0 / 16.0;
                let start_rect = Rect::from_center_size(
                    Pos2::new(full_rect.center().x, full_rect.center().y - 34.0),
                    Vec2::new(card_w, card_h),
                );
                let video_rect = Rect::from_min_max(
                    Pos2::new(
                        start_rect.left() + (full_rect.left() - start_rect.left()) * exp,
                        start_rect.top() + (full_rect.top() - start_rect.top()) * exp,
                    ),
                    Pos2::new(
                        start_rect.right() + (full_rect.right() - start_rect.right()) * exp,
                        start_rect.bottom() + (full_rect.bottom() - start_rect.bottom()) * exp,
                    ),
                );

                // 1. Hardware-decoded GPU video via libmpv OpenGL (morphs cleanly with video_rect!)
                if let Some(mpv) = self.mpv.clone() {
                    let cb = egui_glow::CallbackFn::new(move |info, painter| {
                        let vp = info.viewport_in_pixels();
                        let fbo = painter
                            .intermediate_fbo()
                            .map(|f| f.0.get() as i32)
                            .unwrap_or(0);
                        mpv.render_gl(fbo, vp.width_px, vp.height_px);
                    });
                    ui.painter().add(PaintCallback {
                        rect: video_rect,
                        callback: Arc::new(cb),
                    });
                }

                let right_cut = if self.studio_tab.is_some() {
                    360.0
                } else {
                    0.0
                };
                let center_rect = Rect::from_min_max(
                    Pos2::new(full_rect.left(), full_rect.top() + 64.0),
                    Pos2::new(full_rect.right() - right_cut, full_rect.bottom() - 96.0),
                );
                let bg_resp = ui.interact(center_rect, egui::Id::new("video_bg"), Sense::click());
                if bg_resp.double_clicked() {
                    want_fs_toggle = true;
                } else if bg_resp.clicked() {
                    if self.studio_tab.is_some() {
                        self.studio_tab = None;
                    } else if self.opts_open {
                        self.opts_open = false;
                    } else if let Some(mpv) = &self.mpv {
                        mpv.toggle_pause();
                    }
                }

                let p = ui.painter();

                // 2. Smooth vertex-interpolated Pause Overlay
                let pe = expo_out(self.pause_t);
                if pe > 0.01 {
                    let scrim = Rect::from_min_size(
                        full_rect.min,
                        Vec2::new(full_rect.width() * 0.65, full_rect.height()),
                    );
                    paint_fade_h(
                        p,
                        scrim,
                        Color32::from_rgba_unmultiplied(0, 0, 0, (195.0 * pe) as u8),
                        Color32::TRANSPARENT,
                    );

                    let lx = full_rect.left() + (full_rect.width() * 0.07).max(40.0);
                    let cy = full_rect.center().y - 20.0 + (1.0 - pe) * 12.0;
                    let a_hi = (255.0 * pe) as u8;
                    let a_mid = (180.0 * pe) as u8;
                    let a_dim = (115.0 * pe) as u8;

                    let res = res_label(self.snap.width, self.snap.height);
                    let kicker = match (&self.year, res.is_empty()) {
                        (Some(y), false) => format!("{y} · {res}"),
                        (Some(y), true) => y.clone(),
                        (None, false) => res,
                        (None, true) => String::new(),
                    };
                    if !kicker.is_empty() {
                        p.text(
                            Pos2::new(lx, cy - 22.0),
                            Align2::LEFT_CENTER,
                            kicker,
                            FontId::proportional(12.0),
                            Color32::from_rgba_unmultiplied(255, 255, 255, a_dim),
                        );
                    }
                    p.text(
                        Pos2::new(lx, cy + 12.0),
                        Align2::LEFT_CENTER,
                        &self.title,
                        FontId::proportional(40.0),
                        Color32::from_rgba_unmultiplied(255, 255, 255, a_hi),
                    );
                    if self.snap.duration > 0.0 {
                        let rem = fmt_remaining((self.snap.duration - self.snap.position).max(0.0));
                        p.text(
                            Pos2::new(lx, cy + 48.0),
                            Align2::LEFT_CENTER,
                            rem,
                            FontId::proportional(14.0),
                            Color32::from_rgba_unmultiplied(255, 255, 255, a_mid),
                        );
                    }
                }

                // 3. Text-only HUD
                if let Some(txt) = &self.hud_text {
                    if Instant::now() < self.hud_until {
                        let c = full_rect.center();
                        p.text(
                            c + Vec2::new(0.0, 1.5),
                            Align2::CENTER_CENTER,
                            txt,
                            FontId::proportional(20.0),
                            Color32::from_rgba_unmultiplied(0, 0, 0, 160),
                        );
                        p.text(
                            c,
                            Align2::CENTER_CENTER,
                            txt,
                            FontId::proportional(20.0),
                            Color32::WHITE,
                        );
                        ctx.request_repaint();
                    } else {
                        self.hud_text = None;
                    }
                }

                // 4. Top & Bottom Fade Scrims + Controls
                let ce = expo_out(self.chrome_t);
                if ce > 0.01 {
                    paint_fade_v(
                        p,
                        Rect::from_min_size(full_rect.min, Vec2::new(full_rect.width(), 88.0)),
                        Color32::from_rgba_unmultiplied(0, 0, 0, (175.0 * ce) as u8),
                        Color32::TRANSPARENT,
                    );
                    paint_fade_v(
                        p,
                        Rect::from_min_size(
                            Pos2::new(full_rect.left(), full_rect.bottom() - 120.0),
                            Vec2::new(full_rect.width(), 120.0),
                        ),
                        Color32::TRANSPARENT,
                        Color32::from_rgba_unmultiplied(0, 0, 0, (205.0 * ce) as u8),
                    );

                    // Top bar
                    let top_bar = Rect::from_min_size(
                        Pos2::new(
                            full_rect.left() + 20.0,
                            full_rect.top() + 14.0 - (1.0 - ce) * 8.0,
                        ),
                        Vec2::new(full_rect.width() - 40.0 - right_cut, 32.0),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(top_bar), |ui| {
                        ui.set_opacity(ce);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                            if icon_btn(ui, Vec2::new(28.0, 28.0), draw_back).clicked() {
                                self.begin_close_morph(ctx);
                                return;
                            }
                            ui.label(egui::RichText::new(&self.title).size(14.0));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let is_local = self.session.as_ref().map_or(false, |s| s.is_local);
                                let stat_txt = if is_local {
                                    let r = res_label(self.snap.width, self.snap.height);
                                    if self.queue.len() > 1 {
                                        format!(
                                            "{} · {}/{}",
                                            r,
                                            self.queue_idx + 1,
                                            self.queue.len()
                                        )
                                    } else {
                                        r
                                    }
                                } else if self.status.percent >= 99.9 {
                                    fmt_bytes(self.status.total.max(self.status.downloaded))
                                } else {
                                    format!(
                                        "{:.0}% · {}/s",
                                        self.status.percent,
                                        fmt_bytes(self.status.speed_bps)
                                    )
                                };
                                ui.label(
                                    egui::RichText::new(stat_txt)
                                        .size(12.0)
                                        .color(Color32::from_rgba_unmultiplied(255, 255, 255, 130)),
                                );
                            });
                        });
                    });

                    // Video timeline scrub bar
                    let pad = 22.0;
                    let scrub_y = full_rect.bottom() - 56.0 + (1.0 - ce) * 10.0;
                    let scrub_w = (full_rect.width() - pad * 2.0 - right_cut).max(120.0);
                    let scrub_rect = Rect::from_min_size(
                        Pos2::new(full_rect.left() + pad, scrub_y - 10.0),
                        Vec2::new(scrub_w, 20.0),
                    );
                    let scrub_resp = ui.interact(
                        scrub_rect,
                        egui::Id::new("scrub_bar"),
                        Sense::click_and_drag(),
                    );
                    let hovering_scrub = scrub_resp.hovered() || scrub_resp.dragged();

                    let rail_h = if hovering_scrub { 4.0 } else { 2.0 };
                    let rail_rect = Rect::from_min_size(
                        Pos2::new(scrub_rect.left(), scrub_y - rail_h * 0.5),
                        Vec2::new(scrub_rect.width(), rail_h),
                    );
                    let sp = ui.painter();
                    sp.rect_filled(
                        rail_rect,
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(255, 255, 255, (52.0 * ce) as u8),
                    );

                    if self.snap.buffering {
                        let t = (ctx.input(|i| i.time) as f32 * 1.4).fract();
                        let bw = rail_rect.width() * 0.25;
                        let bx = rail_rect.left() + (rail_rect.width() + bw) * t - bw;
                        let pulse_r = Rect::from_min_max(
                            Pos2::new(bx.max(rail_rect.left()), rail_rect.top()),
                            Pos2::new((bx + bw).min(rail_rect.right()), rail_rect.bottom()),
                        );
                        sp.rect_filled(
                            pulse_r,
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 120),
                        );
                        ctx.request_repaint();
                    }

                    let pct = if self.snap.duration > 0.0 {
                        (self.snap.position / self.snap.duration).clamp(0.0, 1.0) as f32
                    } else {
                        0.0
                    };
                    sp.rect_filled(
                        Rect::from_min_size(
                            rail_rect.min,
                            Vec2::new(rail_rect.width() * pct, rail_h),
                        ),
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(255, 255, 255, (255.0 * ce) as u8),
                    );

                    if (scrub_resp.clicked() || scrub_resp.dragged()) && self.snap.duration > 0.0 {
                        if let Some(pos) = scrub_resp.interact_pointer_pos() {
                            let p_norm =
                                ((pos.x - scrub_rect.left()) / scrub_rect.width()).clamp(0.0, 1.0);
                            if let Some(mpv) = &self.mpv {
                                mpv.seek_absolute(self.snap.duration * p_norm as f64);
                            }
                        }
                    }

                    // Hover preview
                    let hover_target = if hovering_scrub && self.snap.duration > 0.0 {
                        1.0
                    } else {
                        0.0
                    };
                    self.hover_t += (hover_target - self.hover_t) * (dt * 16.0).min(1.0);
                    if hovering_scrub && self.snap.duration > 0.0 {
                        if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                            let p_norm =
                                ((pos.x - scrub_rect.left()) / scrub_rect.width()).clamp(0.0, 1.0);
                            let target_x = pos
                                .x
                                .clamp(scrub_rect.left() + 92.0, scrub_rect.right() - 92.0);
                            if self.hover_x_smooth <= 1.0 {
                                self.hover_x_smooth = target_x;
                            } else {
                                self.hover_x_smooth +=
                                    (target_x - self.hover_x_smooth) * (dt * 24.0).min(1.0);
                            }
                            let hover_sec = self.snap.duration * p_norm as f64;
                            let bucket = ((hover_sec / 5.0).round() as u64) * 5;
                            self.request_thumb(bucket, ctx);

                            if self.hover_t > 0.02 {
                                let ha = expo_out(self.hover_t);
                                let pw = 180.0;
                                let ph = 101.0;
                                let box_r = Rect::from_min_size(
                                    Pos2::new(self.hover_x_smooth - pw * 0.5, scrub_y - ph - 32.0),
                                    Vec2::new(pw, ph + 20.0),
                                );
                                sp.rect_filled(
                                    box_r,
                                    CornerRadius::ZERO,
                                    Color32::from_rgba_unmultiplied(0, 0, 0, (195.0 * ha) as u8),
                                );
                                let img_r = Rect::from_min_size(
                                    box_r.min + Vec2::new(3.0, 3.0),
                                    Vec2::new(pw - 6.0, ph - 6.0),
                                );
                                if let Some((_, tex)) = &self.active_thumb {
                                    sp.image(
                                        tex.id(),
                                        img_r,
                                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                        Color32::from_rgba_unmultiplied(
                                            255,
                                            255,
                                            255,
                                            (255.0 * ha) as u8,
                                        ),
                                    );
                                } else {
                                    sp.rect_filled(
                                        img_r,
                                        CornerRadius::ZERO,
                                        Color32::from_rgba_unmultiplied(
                                            255,
                                            255,
                                            255,
                                            (14.0 * ha) as u8,
                                        ),
                                    );
                                }
                                sp.text(
                                    Pos2::new(box_r.center().x, box_r.bottom() - 10.0),
                                    Align2::CENTER_CENTER,
                                    fmt_time(hover_sec),
                                    FontId::proportional(11.5),
                                    Color32::from_rgba_unmultiplied(
                                        255,
                                        255,
                                        255,
                                        (220.0 * ha) as u8,
                                    ),
                                );
                            }
                        }
                    } else if self.last_thumb_bucket >= 0 {
                        self.thumb_gen.fetch_add(1, Relaxed);
                        self.last_thumb_bucket = -1;
                    }

                    // Bottom controls row (Prev / Play / Next / -10 / +10 / Volume / Clock | Options / Fullscreen)
                    let ctrl_rect = Rect::from_min_size(
                        Pos2::new(
                            full_rect.left() + pad,
                            full_rect.bottom() - 40.0 + (1.0 - ce) * 10.0,
                        ),
                        Vec2::new(scrub_w, 30.0),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(ctrl_rect), |ui| {
                        ui.set_opacity(ce);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                            let mpv = self.mpv.clone();

                            if self.queue.len() > 1 && self.queue_idx > 0 {
                                if icon_btn(ui, Vec2::new(28.0, 28.0), draw_prev).clicked() {
                                    self.play_queue_offset(-1);
                                }
                            }

                            if icon_btn(ui, Vec2::new(30.0, 28.0), |p, r, c| {
                                if self.snap.paused {
                                    draw_play(p, r, c);
                                } else {
                                    draw_pause(p, r, c);
                                }
                            })
                            .clicked()
                            {
                                if let Some(m) = &mpv {
                                    m.toggle_pause();
                                }
                            }

                            if self.queue.len() > 1 && self.queue_idx + 1 < self.queue.len() {
                                if icon_btn(ui, Vec2::new(28.0, 28.0), draw_next).clicked() {
                                    self.play_queue_offset(1);
                                }
                            }

                            if ui
                                .add(egui::Button::new("−10").fill(Color32::TRANSPARENT))
                                .clicked()
                            {
                                if let Some(m) = &mpv {
                                    m.seek_relative(-10.0);
                                }
                                self.flash("−10s");
                            }
                            if ui
                                .add(egui::Button::new("+10").fill(Color32::TRANSPARENT))
                                .clicked()
                            {
                                if let Some(m) = &mpv {
                                    m.seek_relative(10.0);
                                }
                                self.flash("+10s");
                            }

                            let is_muted = self.snap.muted || self.snap.volume < 0.5;
                            if icon_btn(ui, Vec2::new(30.0, 28.0), |p, r, c| {
                                draw_speaker(p, r, c, is_muted);
                            })
                            .clicked()
                            {
                                if let Some(m) = &mpv {
                                    m.toggle_mute();
                                }
                            }

                            let vol_cur = if self.snap.muted {
                                0.0
                            } else {
                                self.snap.volume
                            };
                            if let Some(new_vol) = hairline_bar(ui, 76.0, vol_cur, 0.0, 150.0, None)
                            {
                                if let Some(m) = &mpv {
                                    m.set_volume(new_vol);
                                }
                            }

                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {}",
                                    fmt_time(self.snap.position),
                                    fmt_time(self.snap.duration)
                                ))
                                .size(12.5)
                                .color(Color32::from_rgba_unmultiplied(255, 255, 255, 195)),
                            );

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                                if icon_btn(ui, Vec2::new(30.0, 28.0), |p, r, c| {
                                    draw_fullscreen(p, r, c, self.is_fullscreen);
                                })
                                .clicked()
                                {
                                    want_fs_toggle = true;
                                }

                                if icon_btn(ui, Vec2::new(30.0, 28.0), draw_menu).clicked() {
                                    if self.studio_tab.is_some() {
                                        self.studio_tab = None;
                                    } else {
                                        self.opts_open = !self.opts_open;
                                    }
                                }
                            });
                        });
                    });
                }

                // 5. Tier 1: Compact Quick Options Sheet (Audio · Subtitles · Video + "More Options →")
                let oe = expo_out(self.opts_t);
                if oe > 0.01 {
                    let ow = 316.0;
                    let oh = 286.0;
                    let ox = full_rect.right() - 22.0 - ow;
                    let oy = full_rect.bottom() - 68.0 - oh + (1.0 - oe) * 10.0;
                    let orect = Rect::from_min_size(Pos2::new(ox, oy), Vec2::new(ow, oh));

                    ui.painter().rect_filled(
                        orect,
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(12, 12, 14, (238.0 * oe) as u8),
                    );

                    ui.scope_builder(egui::UiBuilder::new().max_rect(orect), |ui| {
                        ui.set_opacity(oe);
                        let tab_h = 36.0;
                        let tab_w = ow / 3.0;

                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::ZERO;
                            for (tab, label) in [
                                (OptTab::Audio, "Audio"),
                                (OptTab::Subs, "Subtitles"),
                                (OptTab::Video, "Video"),
                            ] {
                                let col = if self.opt_tab == tab {
                                    Color32::WHITE
                                } else {
                                    Color32::from_rgba_unmultiplied(255, 255, 255, 115)
                                };
                                let b = egui::Button::new(
                                    egui::RichText::new(label).size(12.0).color(col),
                                )
                                .fill(Color32::TRANSPARENT);
                                if ui.add_sized(Vec2::new(tab_w, tab_h), b).clicked() {
                                    self.opt_tab = tab;
                                }
                            }
                        });

                        let slide_r = Rect::from_min_size(
                            Pos2::new(
                                orect.left() + self.tab_slide_x * tab_w,
                                orect.top() + tab_h - 1.5,
                            ),
                            Vec2::new(tab_w, 1.5),
                        );
                        ui.painter()
                            .rect_filled(slide_r, CornerRadius::ZERO, Color32::WHITE);

                        let body_rect = Rect::from_min_size(
                            Pos2::new(orect.left() + 14.0, orect.top() + tab_h + 10.0),
                            Vec2::new(ow - 28.0, oh - tab_h - 20.0),
                        );
                        ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
                            let row_w = ow - 32.0;
                            let mpv = self.mpv.clone();

                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                                    match self.opt_tab {
                                        OptTab::Audio => {
                                            if self.snap.audio_tracks.is_empty() {
                                                ui.label(
                                                    egui::RichText::new("Default audio")
                                                        .size(12.0)
                                                        .color(Color32::from_gray(130)),
                                                );
                                            }
                                            for tr in self.snap.audio_tracks.clone() {
                                                let active = tr.id == self.snap.active_aid;
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(row_w, 26.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(tr.display_label())
                                                                .size(12.0)
                                                                .color(if active {
                                                                    Color32::WHITE
                                                                } else {
                                                                    Color32::from_gray(165)
                                                                }),
                                                        )
                                                        .fill(if active {
                                                            Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 18,
                                                            )
                                                        } else {
                                                            Color32::TRANSPARENT
                                                        }),
                                                    )
                                                    .clicked()
                                                {
                                                    if let Some(m) = &mpv {
                                                        m.set_audio_track(tr.id);
                                                    }
                                                }
                                            }
                                            ui.add_space(6.0);
                                            let (v, rst) = labeled_bar(
                                                ui,
                                                row_w,
                                                "Boost",
                                                format!("{:.0}%", self.snap.volume.max(100.0)),
                                                self.snap.volume.max(100.0),
                                                100.0,
                                                300.0,
                                                None,
                                            );
                                            if let Some(nv) = v {
                                                if let Some(m) = &mpv {
                                                    m.set_volume(nv);
                                                }
                                            }
                                            if rst {
                                                if let Some(m) = &mpv {
                                                    m.set_volume(100.0);
                                                }
                                            }
                                            ui.add_space(8.0);
                                            if more_options_row(ui, row_w, "More options") {
                                                self.studio_tab = Some(OptTab::Audio);
                                                self.opts_open = false;
                                            }
                                        }
                                        OptTab::Subs => {
                                            if ui
                                                .add_sized(
                                                    Vec2::new(row_w, 26.0),
                                                    egui::Button::new(
                                                        egui::RichText::new("+ Upload Subtitle")
                                                            .size(12.0),
                                                    )
                                                    .fill(Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 16,
                                                    )),
                                                )
                                                .clicked()
                                            {
                                                if let Some(p) = rfd::FileDialog::new()
                                                    .add_filter(
                                                        "Subtitles",
                                                        &["srt", "vtt", "ass", "ssa", "sub"],
                                                    )
                                                    .pick_file()
                                                {
                                                    if let Some(m) = &mpv {
                                                        m.add_sub_file(&p.to_string_lossy());
                                                    }
                                                    self.flash("Subtitle loaded");
                                                }
                                            }
                                            let off = self.snap.active_sid <= 0;
                                            if ui
                                                .add_sized(
                                                    Vec2::new(row_w, 24.0),
                                                    egui::Button::new(
                                                        egui::RichText::new("Off")
                                                            .size(12.0)
                                                            .color(if off {
                                                                Color32::WHITE
                                                            } else {
                                                                Color32::from_gray(165)
                                                            }),
                                                    )
                                                    .fill(if off {
                                                        Color32::from_rgba_unmultiplied(
                                                            255, 255, 255, 18,
                                                        )
                                                    } else {
                                                        Color32::TRANSPARENT
                                                    }),
                                                )
                                                .clicked()
                                            {
                                                if let Some(m) = &mpv {
                                                    m.set_sub_track(0);
                                                }
                                            }
                                            for tr in self.snap.sub_tracks.clone() {
                                                let active = tr.id == self.snap.active_sid;
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(row_w, 24.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(tr.display_label())
                                                                .size(12.0)
                                                                .color(if active {
                                                                    Color32::WHITE
                                                                } else {
                                                                    Color32::from_gray(165)
                                                                }),
                                                        )
                                                        .fill(if active {
                                                            Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 18,
                                                            )
                                                        } else {
                                                            Color32::TRANSPARENT
                                                        }),
                                                    )
                                                    .clicked()
                                                {
                                                    if let Some(m) = &mpv {
                                                        m.set_sub_track(tr.id);
                                                    }
                                                }
                                            }
                                            ui.add_space(4.0);
                                            let (d, rst) = labeled_bar(
                                                ui,
                                                row_w,
                                                "Delay",
                                                format!("{:+.1}s", self.snap.sub_delay),
                                                self.snap.sub_delay,
                                                -10.0,
                                                10.0,
                                                Some(0.0),
                                            );
                                            if let Some(nd) = d {
                                                if let Some(m) = &mpv {
                                                    m.set_sub_delay((nd * 10.0).round() / 10.0);
                                                }
                                            }
                                            if rst {
                                                if let Some(m) = &mpv {
                                                    m.set_sub_delay(0.0);
                                                }
                                            }
                                            ui.add_space(8.0);
                                            if more_options_row(ui, row_w, "More options") {
                                                self.studio_tab = Some(OptTab::Subs);
                                                self.opts_open = false;
                                            }
                                        }
                                        OptTab::Video => {
                                            ui.label(
                                                egui::RichText::new("Quality")
                                                    .size(11.5)
                                                    .color(Color32::from_gray(145)),
                                            );
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                                                let qs = [
                                                    (0i64, "Original"),
                                                    (1080, "1080p"),
                                                    (720, "720p"),
                                                    (480, "480p"),
                                                ];
                                                let bw = (row_w - 12.0) / 4.0;
                                                for (cap, lbl) in qs {
                                                    let active = self.quality_cap == cap;
                                                    if ui
                                                        .add_sized(
                                                            Vec2::new(bw, 26.0),
                                                            egui::Button::new(
                                                                egui::RichText::new(lbl)
                                                                    .size(11.0)
                                                                    .color(if active {
                                                                        Color32::BLACK
                                                                    } else {
                                                                        Color32::from_gray(190)
                                                                    }),
                                                            )
                                                            .fill(if active {
                                                                Color32::WHITE
                                                            } else {
                                                                Color32::from_rgba_unmultiplied(
                                                                    255, 255, 255, 14,
                                                                )
                                                            }),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.quality_cap = cap;
                                                        if let Some(m) = &mpv {
                                                            m.set_video_quality_cap(cap);
                                                        }
                                                        self.flash(lbl);
                                                    }
                                                }
                                            });

                                            ui.add_space(6.0);
                                            ui.label(
                                                egui::RichText::new("Playback Speed")
                                                    .size(11.5)
                                                    .color(Color32::from_gray(145)),
                                            );
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                                                let bw = (row_w - 20.0) / 6.0;
                                                for r in [0.5, 0.75, 1.0, 1.25, 1.5, 2.0] {
                                                    let active = (self.snap.speed - r).abs() < 0.02;
                                                    if ui
                                                        .add_sized(
                                                            Vec2::new(bw, 26.0),
                                                            egui::Button::new(
                                                                egui::RichText::new(format!(
                                                                    "{r}x"
                                                                ))
                                                                .size(11.5)
                                                                .color(if active {
                                                                    Color32::BLACK
                                                                } else {
                                                                    Color32::from_gray(190)
                                                                }),
                                                            )
                                                            .fill(if active {
                                                                Color32::WHITE
                                                            } else {
                                                                Color32::from_rgba_unmultiplied(
                                                                    255, 255, 255, 14,
                                                                )
                                                            }),
                                                        )
                                                        .clicked()
                                                    {
                                                        if let Some(m) = &mpv {
                                                            m.set_speed(r);
                                                        }
                                                        self.flash(format!("{r}x"));
                                                    }
                                                }
                                            });

                                            ui.add_space(6.0);
                                            ui.label(
                                                egui::RichText::new("Aspect / Fit")
                                                    .size(11.5)
                                                    .color(Color32::from_gray(145)),
                                            );
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                                                let modes = [
                                                    ("auto", "Fit"),
                                                    ("fill", "Fill"),
                                                    ("16:9", "16:9"),
                                                    ("21:9", "21:9"),
                                                    ("stretch", "Stretch"),
                                                ];
                                                let bw = (row_w - 16.0) / 5.0;
                                                for (id, lbl) in modes {
                                                    let active = self.aspect_mode == id;
                                                    if ui
                                                        .add_sized(
                                                            Vec2::new(bw, 26.0),
                                                            egui::Button::new(
                                                                egui::RichText::new(lbl)
                                                                    .size(11.0)
                                                                    .color(if active {
                                                                        Color32::BLACK
                                                                    } else {
                                                                        Color32::from_gray(190)
                                                                    }),
                                                            )
                                                            .fill(if active {
                                                                Color32::WHITE
                                                            } else {
                                                                Color32::from_rgba_unmultiplied(
                                                                    255, 255, 255, 14,
                                                                )
                                                            }),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.aspect_mode = id.into();
                                                        if let Some(m) = &mpv {
                                                            if id == "fill" {
                                                                m.set_aspect_mode("auto");
                                                                m.set_panscan(1.0);
                                                            } else {
                                                                m.set_panscan(0.0);
                                                                m.set_aspect_mode(id);
                                                            }
                                                        }
                                                    }
                                                }
                                            });

                                            ui.add_space(10.0);
                                            if more_options_row(ui, row_w, "More options") {
                                                self.studio_tab = Some(OptTab::Video);
                                                self.opts_open = false;
                                            }
                                        }
                                    }
                                });
                        });
                    });
                }

                // 6. Tier 2: Full Slide-In "More Options" Studio Panel (Apple TV / iOS motion)
                let se = expo_out(self.studio_t);
                if se > 0.01 {
                    let sw = 360.0;
                    let sx = full_rect.right() - sw * se;
                    let srect = Rect::from_min_size(
                        Pos2::new(sx, full_rect.top()),
                        Vec2::new(sw, full_rect.height()),
                    );
                    // Soft fade shadow on left edge of studio panel
                    paint_fade_h(
                        ui.painter(),
                        Rect::from_min_size(
                            Pos2::new(sx - 44.0, full_rect.top()),
                            Vec2::new(44.0, full_rect.height()),
                        ),
                        Color32::TRANSPARENT,
                        Color32::from_rgba_unmultiplied(0, 0, 0, (140.0 * se) as u8),
                    );
                    ui.painter().rect_filled(
                        srect,
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(8, 8, 10, (242.0 * se) as u8),
                    );

                    let active_studio = self.studio_tab.unwrap_or(self.opt_tab);
                    let inner = srect.shrink2(Vec2::new(20.0, 18.0));
                    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                        ui.set_opacity(se);
                        let row_w = inner.width() - 8.0;
                        let mpv = self.mpv.clone();

                        // Header with Back + Category switcher
                        ui.horizontal(|ui| {
                            if icon_btn(ui, Vec2::new(26.0, 26.0), draw_back).clicked() {
                                self.studio_tab = None;
                                return;
                            }
                            for (tab, lbl) in [
                                (OptTab::Video, "Video"),
                                (OptTab::Audio, "Audio"),
                                (OptTab::Subs, "Subtitles"),
                            ] {
                                let on = active_studio == tab;
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(lbl).size(13.0).color(if on {
                                                Color32::WHITE
                                            } else {
                                                Color32::from_gray(120)
                                            }),
                                        )
                                        .fill(Color32::TRANSPARENT),
                                    )
                                    .clicked()
                                {
                                    self.studio_tab = Some(tab);
                                }
                            }
                        });
                        ui.add_space(10.0);

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 8.0);
                                match active_studio {
                                    OptTab::Video => {
                                        let (z, rz) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Zoom",
                                            format!("{:+.2}", self.snap.video_zoom),
                                            self.snap.video_zoom,
                                            -0.5,
                                            1.5,
                                            Some(0.0),
                                        );
                                        if let Some(v) = z {
                                            if let Some(m) = &mpv {
                                                m.set_video_zoom(v);
                                            }
                                        }
                                        if rz {
                                            if let Some(m) = &mpv {
                                                m.set_video_zoom(0.0);
                                            }
                                        }

                                        let (ps, rps) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Crop / Pan-Scan",
                                            format!("{:.0}%", self.snap.panscan * 100.0),
                                            self.snap.panscan,
                                            0.0,
                                            1.0,
                                            None,
                                        );
                                        if let Some(v) = ps {
                                            if let Some(m) = &mpv {
                                                m.set_panscan(v);
                                            }
                                        }
                                        if rps {
                                            if let Some(m) = &mpv {
                                                m.set_panscan(0.0);
                                            }
                                        }

                                        let (vy, rvy) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Vertical Shift",
                                            format!("{:+.2}", self.snap.align_y),
                                            self.snap.align_y,
                                            -1.0,
                                            1.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = vy {
                                            if let Some(m) = &mpv {
                                                m.set_video_align_y(v);
                                            }
                                        }
                                        if rvy {
                                            if let Some(m) = &mpv {
                                                m.set_video_align_y(0.0);
                                            }
                                        }

                                        let (vx, rvx) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Horizontal Shift",
                                            format!("{:+.2}", self.snap.align_x),
                                            self.snap.align_x,
                                            -1.0,
                                            1.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = vx {
                                            if let Some(m) = &mpv {
                                                m.set_video_align_x(v);
                                            }
                                        }
                                        if rvx {
                                            if let Some(m) = &mpv {
                                                m.set_video_align_x(0.0);
                                            }
                                        }

                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new("Aspect Ratio")
                                                .size(12.0)
                                                .color(Color32::from_gray(150)),
                                        );
                                        ui.horizontal(|ui| {
                                            let bw = (row_w - 24.0) / 5.0;
                                            for (id, lbl) in [
                                                ("auto", "Auto"),
                                                ("16:9", "16:9"),
                                                ("21:9", "21:9"),
                                                ("4:3", "4:3"),
                                                ("stretch", "Stretch"),
                                            ] {
                                                let on = self.aspect_mode == id;
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(bw, 26.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(lbl)
                                                                .size(11.0)
                                                                .color(if on {
                                                                    Color32::BLACK
                                                                } else {
                                                                    Color32::WHITE
                                                                }),
                                                        )
                                                        .fill(if on {
                                                            Color32::WHITE
                                                        } else {
                                                            Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 16,
                                                            )
                                                        }),
                                                    )
                                                    .clicked()
                                                {
                                                    self.aspect_mode = id.into();
                                                    if let Some(m) = &mpv {
                                                        m.set_aspect_mode(id);
                                                    }
                                                }
                                            }
                                        });

                                        ui.label(
                                            egui::RichText::new("Rotate")
                                                .size(12.0)
                                                .color(Color32::from_gray(150)),
                                        );
                                        ui.horizontal(|ui| {
                                            let bw = (row_w - 18.0) / 4.0;
                                            for deg in [0, 90, 180, 270] {
                                                let on = self.snap.rotate == deg;
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(bw, 26.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(format!("{deg}°"))
                                                                .size(11.5)
                                                                .color(if on {
                                                                    Color32::BLACK
                                                                } else {
                                                                    Color32::WHITE
                                                                }),
                                                        )
                                                        .fill(if on {
                                                            Color32::WHITE
                                                        } else {
                                                            Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 16,
                                                            )
                                                        }),
                                                    )
                                                    .clicked()
                                                {
                                                    if let Some(m) = &mpv {
                                                        m.set_video_rotate(deg);
                                                    }
                                                }
                                            }
                                        });

                                        ui.label(
                                            egui::RichText::new("GPU Upscaling")
                                                .size(12.0)
                                                .color(Color32::from_gray(150)),
                                        );
                                        ui.horizontal(|ui| {
                                            let bw = (row_w - 12.0) / 3.0;
                                            for (id, lbl) in [
                                                ("bilinear", "Fast"),
                                                ("spline36", "Spline36"),
                                                ("ewa_lanczossharp", "Lanczos Sharp"),
                                            ] {
                                                let on = self.upscaler == id;
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(bw, 26.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(lbl)
                                                                .size(11.0)
                                                                .color(if on {
                                                                    Color32::BLACK
                                                                } else {
                                                                    Color32::WHITE
                                                                }),
                                                        )
                                                        .fill(if on {
                                                            Color32::WHITE
                                                        } else {
                                                            Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 16,
                                                            )
                                                        }),
                                                    )
                                                    .clicked()
                                                {
                                                    self.upscaler = id.into();
                                                    if let Some(m) = &mpv {
                                                        m.set_upscaler(id);
                                                    }
                                                }
                                            }
                                        });

                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 28.0),
                                                egui::Button::new(
                                                    egui::RichText::new(if self.snap.deband {
                                                        "Debanding: On"
                                                    } else {
                                                        "Debanding: Off"
                                                    })
                                                    .size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255,
                                                    255,
                                                    255,
                                                    if self.snap.deband { 26 } else { 12 },
                                                )),
                                            )
                                            .clicked()
                                        {
                                            if let Some(m) = &mpv {
                                                m.set_deband(!self.snap.deband);
                                            }
                                        }

                                        let (br, rbr) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Brightness",
                                            format!("{:+}", self.snap.brightness),
                                            self.snap.brightness as f64,
                                            -50.0,
                                            50.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = br {
                                            if let Some(m) = &mpv {
                                                m.set_brightness(v.round() as i64);
                                            }
                                        }
                                        if rbr {
                                            if let Some(m) = &mpv {
                                                m.set_brightness(0);
                                            }
                                        }

                                        let (ct, rct) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Contrast",
                                            format!("{:+}", self.snap.contrast),
                                            self.snap.contrast as f64,
                                            -50.0,
                                            50.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = ct {
                                            if let Some(m) = &mpv {
                                                m.set_contrast(v.round() as i64);
                                            }
                                        }
                                        if rct {
                                            if let Some(m) = &mpv {
                                                m.set_contrast(0);
                                            }
                                        }

                                        let (sat, rsat) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Saturation",
                                            format!("{:+}", self.snap.saturation),
                                            self.snap.saturation as f64,
                                            -50.0,
                                            50.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = sat {
                                            if let Some(m) = &mpv {
                                                m.set_saturation(v.round() as i64);
                                            }
                                        }
                                        if rsat {
                                            if let Some(m) = &mpv {
                                                m.set_saturation(0);
                                            }
                                        }

                                        let (gm, rgm) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Gamma",
                                            format!("{:+}", self.snap.gamma),
                                            self.snap.gamma as f64,
                                            -50.0,
                                            50.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = gm {
                                            if let Some(m) = &mpv {
                                                m.set_gamma(v.round() as i64);
                                            }
                                        }
                                        if rgm {
                                            if let Some(m) = &mpv {
                                                m.set_gamma(0);
                                            }
                                        }

                                        ui.add_space(6.0);
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new("Reset Video").size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255, 255, 255, 16,
                                                )),
                                            )
                                            .clicked()
                                        {
                                            self.aspect_mode = "auto".into();
                                            self.upscaler = "spline36".into();
                                            if let Some(m) = &mpv {
                                                m.reset_video();
                                            }
                                        }
                                    }
                                    OptTab::Audio => {
                                        let (v, rv) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Volume Boost",
                                            format!("{:.0}%", self.snap.volume),
                                            self.snap.volume,
                                            0.0,
                                            300.0,
                                            Some(100.0),
                                        );
                                        if let Some(nv) = v {
                                            if let Some(m) = &mpv {
                                                m.set_volume(nv);
                                            }
                                        }
                                        if rv {
                                            if let Some(m) = &mpv {
                                                m.set_volume(100.0);
                                            }
                                        }

                                        let (ad, rad) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Audio Sync Delay",
                                            format!("{:+.2}s", self.snap.audio_delay),
                                            self.snap.audio_delay,
                                            -5.0,
                                            5.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = ad {
                                            if let Some(m) = &mpv {
                                                m.set_audio_delay((v * 20.0).round() / 20.0);
                                            }
                                        }
                                        if rad {
                                            if let Some(m) = &mpv {
                                                m.set_audio_delay(0.0);
                                            }
                                        }

                                        ui.add_space(6.0);
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new(if self.night_mode {
                                                        "Night Mode (Loudness Norm): On"
                                                    } else {
                                                        "Night Mode (Loudness Norm): Off"
                                                    })
                                                    .size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255,
                                                    255,
                                                    255,
                                                    if self.night_mode { 26 } else { 12 },
                                                )),
                                            )
                                            .clicked()
                                        {
                                            self.night_mode = !self.night_mode;
                                            if let Some(m) = &mpv {
                                                m.set_night_mode(self.night_mode);
                                            }
                                        }
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new(if self.stereo_downmix {
                                                        "Stereo Downmix: On"
                                                    } else {
                                                        "Stereo Downmix: Auto"
                                                    })
                                                    .size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255,
                                                    255,
                                                    255,
                                                    if self.stereo_downmix { 26 } else { 12 },
                                                )),
                                            )
                                            .clicked()
                                        {
                                            self.stereo_downmix = !self.stereo_downmix;
                                            if let Some(m) = &mpv {
                                                m.set_stereo_downmix(self.stereo_downmix);
                                            }
                                        }
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new(
                                                        if self.snap.pitch_correction {
                                                            "Pitch Correction: On"
                                                        } else {
                                                            "Pitch Correction: Off"
                                                        },
                                                    )
                                                    .size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255,
                                                    255,
                                                    255,
                                                    if self.snap.pitch_correction {
                                                        26
                                                    } else {
                                                        12
                                                    },
                                                )),
                                            )
                                            .clicked()
                                        {
                                            if let Some(m) = &mpv {
                                                m.set_pitch_correction(!self.snap.pitch_correction);
                                            }
                                        }

                                        ui.add_space(8.0);
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new("Reset Audio").size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255, 255, 255, 16,
                                                )),
                                            )
                                            .clicked()
                                        {
                                            self.night_mode = false;
                                            self.stereo_downmix = false;
                                            if let Some(m) = &mpv {
                                                m.reset_audio();
                                            }
                                        }
                                    }
                                    OptTab::Subs => {
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new("+ Upload Local Subtitle")
                                                        .size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255, 255, 255, 18,
                                                )),
                                            )
                                            .clicked()
                                        {
                                            if let Some(p) = rfd::FileDialog::new()
                                                .add_filter(
                                                    "Subtitles",
                                                    &["srt", "vtt", "ass", "ssa", "sub"],
                                                )
                                                .pick_file()
                                            {
                                                if let Some(m) = &mpv {
                                                    m.add_sub_file(&p.to_string_lossy());
                                                }
                                                self.flash("Subtitle loaded");
                                            }
                                        }

                                        let (d, rd) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Delay",
                                            format!("{:+.1}s", self.snap.sub_delay),
                                            self.snap.sub_delay,
                                            -10.0,
                                            10.0,
                                            Some(0.0),
                                        );
                                        if let Some(v) = d {
                                            if let Some(m) = &mpv {
                                                m.set_sub_delay((v * 10.0).round() / 10.0);
                                            }
                                        }
                                        if rd {
                                            if let Some(m) = &mpv {
                                                m.set_sub_delay(0.0);
                                            }
                                        }

                                        let (sp, rsp) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Speed",
                                            format!("{:.2}x", self.snap.sub_speed),
                                            self.snap.sub_speed,
                                            0.90,
                                            1.10,
                                            Some(1.0),
                                        );
                                        if let Some(v) = sp {
                                            if let Some(m) = &mpv {
                                                m.set_sub_speed((v * 100.0).round() / 100.0);
                                            }
                                        }
                                        if rsp {
                                            if let Some(m) = &mpv {
                                                m.set_sub_speed(1.0);
                                            }
                                        }

                                        let (sc, rsc) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Size",
                                            format!("{:.0}%", self.snap.sub_scale * 100.0),
                                            self.snap.sub_scale,
                                            0.5,
                                            2.0,
                                            Some(1.0),
                                        );
                                        if let Some(v) = sc {
                                            if let Some(m) = &mpv {
                                                m.set_sub_scale(v);
                                            }
                                        }
                                        if rsc {
                                            if let Some(m) = &mpv {
                                                m.set_sub_scale(1.0);
                                            }
                                        }

                                        let (pos, rpos) = labeled_bar(
                                            ui,
                                            row_w,
                                            "Vertical Position",
                                            format!("{}%", self.snap.sub_pos),
                                            self.snap.sub_pos as f64,
                                            10.0,
                                            100.0,
                                            None,
                                        );
                                        if let Some(v) = pos {
                                            if let Some(m) = &mpv {
                                                m.set_sub_pos(v.round() as i64);
                                            }
                                        }
                                        if rpos {
                                            if let Some(m) = &mpv {
                                                m.set_sub_pos(100);
                                            }
                                        }

                                        ui.add_space(6.0);
                                        ui.label(
                                            egui::RichText::new("Online Subtitles (Free)")
                                                .size(12.0)
                                                .color(Color32::from_gray(150)),
                                        );
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.sub_query)
                                                    .hint_text("Movie / show title")
                                                    .frame(false)
                                                    .margin(Margin::symmetric(8, 6))
                                                    .desired_width(row_w - 94.0),
                                            );
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.sub_lang)
                                                    .frame(false)
                                                    .margin(Margin::symmetric(6, 6))
                                                    .desired_width(36.0),
                                            );
                                            if ui
                                                .add_sized(
                                                    Vec2::new(48.0, 26.0),
                                                    egui::Button::new(
                                                        egui::RichText::new("Find")
                                                            .color(Color32::BLACK)
                                                            .size(12.0),
                                                    )
                                                    .fill(Color32::WHITE),
                                                )
                                                .clicked()
                                                && !self.sub_query.trim().is_empty()
                                            {
                                                self.sub_msg = "Searching…".into();
                                                let eng = self.engine.clone();
                                                let q = self.sub_query.clone();
                                                let l = self.sub_lang.clone();
                                                let (tx, rx) = mpsc::channel();
                                                self.sub_search_rx = Some(rx);
                                                std::thread::spawn(move || {
                                                    let _ = tx.send(eng.search_subs(&q, &l));
                                                });
                                            }
                                        });
                                        if !self.sub_msg.is_empty() {
                                            ui.label(
                                                egui::RichText::new(&self.sub_msg)
                                                    .size(11.5)
                                                    .color(Color32::from_gray(145)),
                                            );
                                        }
                                        for item in self.sub_results.clone() {
                                            let label = format!(
                                                "{} ({})",
                                                item.name.chars().take(34).collect::<String>(),
                                                item.lang
                                            );
                                            if ui
                                                .add_sized(
                                                    Vec2::new(row_w, 26.0),
                                                    egui::Button::new(
                                                        egui::RichText::new(label).size(11.5),
                                                    )
                                                    .fill(Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 12,
                                                    )),
                                                )
                                                .clicked()
                                            {
                                                self.sub_msg = "Downloading…".into();
                                                let eng = self.engine.clone();
                                                let dl = item.download_url.clone();
                                                let (tx, rx) = mpsc::channel();
                                                self.sub_dl_rx = Some(rx);
                                                std::thread::spawn(move || {
                                                    let _ = tx.send(eng.download_sub(&dl));
                                                });
                                            }
                                        }

                                        ui.add_space(6.0);
                                        if ui
                                            .add_sized(
                                                Vec2::new(row_w, 30.0),
                                                egui::Button::new(
                                                    egui::RichText::new("Reset Subtitles")
                                                        .size(12.0),
                                                )
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    255, 255, 255, 16,
                                                )),
                                            )
                                            .clicked()
                                        {
                                            if let Some(m) = &mpv {
                                                m.reset_subs();
                                            }
                                        }
                                    }
                                }
                            });
                    });
                }
            });

        if want_fs_toggle {
            self.is_fullscreen = !self.is_fullscreen;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.is_fullscreen));
        }

        if !self.snap.paused {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }
}

impl eframe::App for StreamdApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Receive decoded Cover-Flow card thumbnails
        while let Ok((src_key, ci)) = self.card_thumb_rx.try_recv() {
            let tex = ctx.load_texture(format!("card_{src_key}"), ci, TextureOptions::LINEAR);
            if let Some(item) = self.library.iter_mut().find(|x| x.source == src_key) {
                item.thumb = Some(tex);
            }
        }

        self.handle_dropped_files(ctx);

        if let Some(rx) = &self.open_rx {
            if let Ok(res) = rx.try_recv() {
                self.opening = false;
                self.open_rx = None;
                match res {
                    Ok(meta) => self.activate_session(meta),
                    Err(e) => self.err_msg = e.to_string(),
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }

        self.handle_shortcuts(ctx);

        if self.session.is_some() {
            self.draw_player(ctx);
        } else {
            self.draw_home(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(mpv) = &self.mpv {
            mpv.destroy();
        }
        self.engine.close_all();
    }
}

fn main() -> anyhow::Result<()> {
    let initial_arg = std::env::args().nth(1);
    let engine = Engine::start()?;
    let mut vp = egui::ViewportBuilder::default()
        .with_title("streamd")
        .with_inner_size([1280.0, 760.0])
        .with_min_inner_size([640.0, 400.0])
        .with_drag_and_drop(true);
    if let Some(icon) = load_app_icon() {
        vp = vp.with_icon(Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport: vp,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    let eng_for_app = engine.clone();
    eframe::run_native(
        "streamd",
        options,
        Box::new(move |cc| Ok(Box::new(StreamdApp::new(cc, eng_for_app, initial_arg)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    engine.close_all();
    Ok(())
}
