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
    path::PathBuf,
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

fn is_sub_ext(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("srt" | "vtt" | "ass" | "ssa" | "sub" | "idx")
    )
}

// ---------- Smooth GPU vertex-interpolated fade gradients (zero banding lines!) ----------
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

// ---------- Crisp custom vector icons (zero missing-glyph boxes on any OS!) ----------
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
    let pts = vec![
        Pos2::new(c.x - 4.5, c.y - 6.0),
        Pos2::new(c.x + 6.5, c.y),
        Pos2::new(c.x - 4.5, c.y + 6.0),
    ];
    p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
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
        // Outward 4 corners
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
        // Inward 4 corners
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

// ---------- Unified headless hairline seekbar (no circle knob!) ----------
fn hairline_bar(
    ui: &mut egui::Ui,
    _id: &str,
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
        // Crisp center zero tick
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptTab {
    Audio,
    Subs,
    Speed,
}

impl OptTab {
    fn idx(self) -> f32 {
        match self {
            OptTab::Audio => 0.0,
            OptTab::Subs => 1.0,
            OptTab::Speed => 2.0,
        }
    }
}

struct StreamdApp {
    engine: Arc<Engine>,
    mpv: Option<Arc<MpvPlayer>>,
    mpv_err: Option<String>,
    logo_tex: Option<TextureHandle>,

    // Home inputs
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

    // Active session
    session: Option<SessionMeta>,
    title: String,
    year: Option<String>,
    snap: PlayerSnapshot,
    status: SessionStatus,
    last_status_poll: Instant,

    // Online subtitle search
    sub_query: String,
    sub_lang: String,
    sub_msg: String,
    sub_results: Vec<OnlineSub>,
    sub_search_rx: Option<mpsc::Receiver<anyhow::Result<Vec<OnlineSub>>>>,
    sub_dl_rx: Option<mpsc::Receiver<anyhow::Result<PathBuf>>>,

    // Chrome & Morphing animations
    last_mouse_move: Instant,
    last_mouse_pos: Option<Pos2>,
    chrome_t: f32,
    pause_t: f32,
    pause_since: Option<Instant>,
    opts_open: bool,
    opts_t: f32,
    opt_tab: OptTab,
    tab_slide_x: f32,
    is_fullscreen: bool,

    // Text-only HUD (no box!)
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
    fn new(cc: &eframe::CreationContext<'_>, engine: Arc<Engine>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(Color32::WHITE);
        visuals.panel_fill = Color32::BLACK;
        visuals.window_fill = Color32::from_rgba_unmultiplied(10, 10, 12, 220);
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

        let logo_tex = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
            .ok()
            .map(|img| {
                let rgba = img.to_rgba8();
                let ci = ColorImage::from_rgba_unmultiplied(
                    [rgba.width() as usize, rgba.height() as usize],
                    rgba.as_raw(),
                );
                cc.egui_ctx
                    .load_texture("streamd_logo", ci, TextureOptions::LINEAR)
            });

        let (mpv, mpv_err) = match cc.get_proc_address {
            Some(gpa) => match MpvPlayer::new(cc.egui_ctx.clone(), gpa) {
                Ok(p) => (Some(p), None),
                Err(e) => (None, Some(e.to_string())),
            },
            None => (None, Some("OpenGL proc address loader unavailable".into())),
        };

        let (thumb_tx, thumb_rx) = mpsc::channel();

        Self {
            engine,
            mpv,
            mpv_err,
            logo_tex,
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
            session: None,
            title: String::new(),
            year: None,
            snap: PlayerSnapshot::default(),
            status: SessionStatus::default(),
            last_status_poll: Instant::now(),
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
        }
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
            self.open_local_path(maybe_path);
            return;
        }

        self.opening = true;
        self.err_msg.clear();
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

    fn open_local_path(&mut self, path: PathBuf) {
        self.err_msg.clear();
        match self.engine.open_local(path) {
            Ok(meta) => self.activate_session(meta),
            Err(e) => self.err_msg = e.to_string(),
        }
    }

    fn activate_session(&mut self, meta: SessionMeta) {
        let (t, y) = clean_title(&meta.name);
        self.title = t.clone();
        self.year = y;
        self.sub_query = t;
        self.sub_results.clear();
        self.sub_msg.clear();
        if let Some(mpv) = &self.mpv {
            mpv.load_url(&meta.media_url);
        }
        self.session = Some(meta);
        self.last_mouse_move = Instant::now();
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
        self.thumb_cache.clear();
        self.active_thumb = None;
        self.last_thumb_bucket = -1;
        self.opts_open = false;
        self.pause_t = 0.0;
        self.pause_since = None;
        self.snap = PlayerSnapshot::default();
        self.status = SessionStatus::default();
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(path) = f.path {
                if is_sub_ext(&path) {
                    if let Some(mpv) = &self.mpv {
                        mpv.add_sub_file(&path.to_string_lossy());
                        self.flash("Subtitle loaded");
                    }
                } else {
                    if self.session.is_some() {
                        self.close_session();
                    }
                    self.open_local_path(path);
                }
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
        // Clear stale preview if it's more than 30s away so it never looks stuck on an old frame
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

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.session.is_none() || ctx.wants_keyboard_input() {
            return;
        }
        let Some(mpv) = self.mpv.clone() else { return };

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
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.is_fullscreen));
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
                if self.opts_open {
                    self.opts_open = false;
                } else if self.is_fullscreen {
                    self.is_fullscreen = false;
                    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
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
    }

    fn draw_home(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.05);
        let target_h = if self.show_hdrs { 1.0 } else { 0.0 };
        self.hdrs_anim += (target_h - self.hdrs_anim) * (dt * 14.0).min(1.0);
        if (self.hdrs_anim - target_h).abs() > 0.002 {
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let w = rect.width().min(600.0);
                let block_h = 160.0 + expo_out(self.hdrs_anim) * 170.0;
                let top = (rect.height() - block_h) * 0.46;
                let area_rect = Rect::from_min_size(
                    Pos2::new(rect.center().x - w * 0.5, top.max(32.0)),
                    Vec2::new(w, block_h + 140.0),
                );

                ui.scope_builder(egui::UiBuilder::new().max_rect(area_rect), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 12.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(10.0, 0.0);
                        if let Some(tex) = &self.logo_tex {
                            ui.image(egui::load::SizedTexture::new(tex.id(), Vec2::splat(30.0)));
                        }
                        ui.label(
                            egui::RichText::new("streamd")
                                .size(28.0)
                                .color(Color32::WHITE),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        let btn_w = 88.0;
                        let te = egui::TextEdit::singleline(&mut self.url)
                            .hint_text("Paste direct video URL or drop a file")
                            .frame(false)
                            .margin(Margin::symmetric(14, 12))
                            .desired_width(w - btn_w);

                        let bg = Rect::from_min_size(ui.cursor().min, Vec2::new(w - btn_w, 42.0));
                        ui.painter().rect_filled(
                            bg,
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                        );
                        let resp = ui.add_sized(Vec2::new(w - btn_w, 42.0), te);
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            self.start_open_url();
                        }

                        let can_play =
                            !self.opening && !self.url.trim().is_empty() && self.mpv.is_some();
                        let btn_fill = if can_play {
                            Color32::WHITE
                        } else {
                            Color32::from_rgba_unmultiplied(255, 255, 255, 50)
                        };
                        let btn = egui::Button::new(
                            egui::RichText::new(if self.opening { "Opening" } else { "Play" })
                                .color(Color32::BLACK)
                                .size(14.0),
                        )
                        .fill(btn_fill)
                        .stroke(Stroke::NONE)
                        .corner_radius(CornerRadius::ZERO);
                        if ui.add_sized(Vec2::new(btn_w, 42.0), btn).clicked() && can_play {
                            self.start_open_url();
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(16.0, 0.0);
                        let hdr_btn = egui::Button::new(
                            egui::RichText::new(if self.show_hdrs {
                                "− Headers / Resume"
                            } else {
                                "+ Headers / Resume"
                            })
                            .size(12.5)
                            .color(Color32::from_rgba_unmultiplied(255, 255, 255, 130)),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::NONE);
                        if ui.add(hdr_btn).clicked() {
                            self.show_hdrs = !self.show_hdrs;
                        }

                        let open_btn = egui::Button::new(
                            egui::RichText::new("Open File / .part")
                                .size(12.5)
                                .color(Color32::from_rgba_unmultiplied(255, 255, 255, 130)),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::NONE);
                        if ui.add(open_btn).clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_file() {
                                self.open_local_path(p);
                            }
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
                                let r = Rect::from_min_size(ui.cursor().min, Vec2::new(w, 36.0));
                                ui.painter().rect_filled(
                                    r,
                                    CornerRadius::ZERO,
                                    Color32::from_rgba_unmultiplied(255, 255, 255, 14),
                                );
                                ui.add_sized(
                                    Vec2::new(w, 36.0),
                                    egui::TextEdit::singleline(val)
                                        .hint_text(hint)
                                        .frame(false)
                                        .margin(Margin::symmetric(12, 9)),
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                                let pick_seed = egui::Button::new(
                                    egui::RichText::new(
                                        "Attach partial file (.part / .crdownload)",
                                    )
                                    .size(12.0)
                                    .color(Color32::from_rgba_unmultiplied(255, 255, 255, 180)),
                                )
                                .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 14));
                                if ui.add(pick_seed).clicked() {
                                    self.seed_file = rfd::FileDialog::new().pick_file();
                                }
                                if let Some(sf) = &self.seed_file.clone() {
                                    let fname = sf
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("partial");
                                    ui.label(
                                        egui::RichText::new(fname)
                                            .size(12.0)
                                            .color(Color32::from_gray(160)),
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

                    if let Some(e) = &self.mpv_err {
                        ui.label(
                            egui::RichText::new(e)
                                .size(13.0)
                                .color(Color32::from_rgba_unmultiplied(255, 255, 255, 210)),
                        );
                    }
                    if !self.err_msg.is_empty() {
                        let r = Rect::from_min_size(ui.cursor().min, Vec2::new(w, 44.0));
                        ui.painter().rect_filled(
                            r,
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 18),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(&self.err_msg)
                                .size(13.0)
                                .color(Color32::from_rgba_unmultiplied(255, 255, 255, 225)),
                        );
                    }
                });
            });
    }

    fn draw_player(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.05);

        if let Some(mpv) = &self.mpv {
            self.snap = mpv.poll_state();
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
            || self.last_mouse_move.elapsed() < Duration::from_millis(2400);
        let chrome_target = if want_chrome { 1.0 } else { 0.0 };
        self.chrome_t += (chrome_target - self.chrome_t) * (dt * 11.0).min(1.0);

        if self.snap.paused && !self.opts_open {
            if self.pause_since.is_none() {
                self.pause_since = Some(Instant::now());
            }
        } else {
            self.pause_since = None;
        }
        let want_pause_overlay = self
            .pause_since
            .map_or(false, |t| t.elapsed() > Duration::from_millis(260));
        let pause_target = if want_pause_overlay { 1.0 } else { 0.0 };
        self.pause_t += (pause_target - self.pause_t) * (dt * 9.0).min(1.0);

        let opts_target = if self.opts_open { 1.0 } else { 0.0 };
        self.opts_t += (opts_target - self.opts_t) * (dt * 14.0).min(1.0);

        let tab_target = self.opt_tab.idx();
        self.tab_slide_x += (tab_target - self.tab_slide_x) * (dt * 16.0).min(1.0);

        if (self.chrome_t - chrome_target).abs() > 0.003
            || (self.pause_t - pause_target).abs() > 0.003
            || (self.opts_t - opts_target).abs() > 0.003
            || (self.tab_slide_x - tab_target).abs() > 0.003
        {
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();

                // 1. Hardware-decoded GPU video via libmpv OpenGL
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
                        rect: full_rect,
                        callback: Arc::new(cb),
                    });
                }

                let center_rect = Rect::from_min_max(
                    Pos2::new(full_rect.left(), full_rect.top() + 64.0),
                    Pos2::new(full_rect.right(), full_rect.bottom() - 96.0),
                );
                let bg_resp = ui.interact(center_rect, egui::Id::new("video_bg"), Sense::click());
                if bg_resp.double_clicked() {
                    self.is_fullscreen = !self.is_fullscreen;
                    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.is_fullscreen));
                } else if bg_resp.clicked() {
                    if self.opts_open {
                        self.opts_open = false;
                    } else if let Some(mpv) = &self.mpv {
                        mpv.toggle_pause();
                    }
                }

                let p = ui.painter();

                // 2. Smooth vertex-interpolated Pause Overlay (zero banding lines, zero cheesy text!)
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

                // 3. Text-only HUD (NO box!)
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

                // 4. Smooth Top & Bottom Fade Scrims + Controls
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
                        Vec2::new(full_rect.width() - 40.0, 32.0),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(top_bar), |ui| {
                        ui.set_opacity(ce);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                            if icon_btn(ui, Vec2::new(28.0, 28.0), draw_back).clicked() {
                                self.close_session();
                                return;
                            }
                            ui.label(egui::RichText::new(&self.title).size(14.0));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let is_local = self.session.as_ref().map_or(false, |s| s.is_local);
                                let stat_txt = if is_local {
                                    res_label(self.snap.width, self.snap.height)
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

                    // Video timeline scrub bar (headless hairline!)
                    let pad = 22.0;
                    let scrub_y = full_rect.bottom() - 56.0 + (1.0 - ce) * 10.0;
                    let scrub_rect = Rect::from_min_size(
                        Pos2::new(full_rect.left() + pad, scrub_y - 10.0),
                        Vec2::new(full_rect.width() - pad * 2.0, 20.0),
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

                    // Hover preview (immediately cancels stale ffmpeg spawns when moving!)
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

                    // Bottom controls row (vector icons + headless volume seekbar!)
                    let ctrl_rect = Rect::from_min_size(
                        Pos2::new(
                            full_rect.left() + pad,
                            full_rect.bottom() - 40.0 + (1.0 - ce) * 10.0,
                        ),
                        Vec2::new(full_rect.width() - pad * 2.0, 30.0),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(ctrl_rect), |ui| {
                        ui.set_opacity(ce);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                            let mpv = self.mpv.clone();

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

                            // Headless hairline volume seekbar (matches video scrub bar!)
                            let vol_cur = if self.snap.muted {
                                0.0
                            } else {
                                self.snap.volume
                            };
                            if let Some(new_vol) =
                                hairline_bar(ui, "vol_bar", 76.0, vol_cur, 0.0, 150.0, None)
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
                                    self.is_fullscreen = !self.is_fullscreen;
                                    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(
                                        self.is_fullscreen,
                                    ));
                                }

                                if icon_btn(ui, Vec2::new(30.0, 28.0), draw_menu).clicked() {
                                    self.opts_open = !self.opts_open;
                                }
                            });
                        });
                    });
                }

                // 5. Clean Borderless Morphing Options Sheet (all headless hairline seekbars!)
                let oe = expo_out(self.opts_t);
                if oe > 0.01 {
                    let ow = 316.0;
                    let oh = 310.0;
                    let ox = full_rect.right() - 22.0 - ow;
                    let oy = full_rect.bottom() - 68.0 - oh + (1.0 - oe) * 10.0;
                    let orect = Rect::from_min_size(Pos2::new(ox, oy), Vec2::new(ow, oh));

                    ui.painter().rect_filled(
                        orect,
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(10, 10, 12, (232.0 * oe) as u8),
                    );

                    ui.scope_builder(egui::UiBuilder::new().max_rect(orect), |ui| {
                        ui.set_opacity(oe);
                        let tab_h = 36.0;
                        let tab_w = ow / 3.0;
                        ui.painter().rect_filled(
                            Rect::from_min_size(orect.min, Vec2::new(ow, tab_h)),
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 8),
                        );

                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::ZERO;
                            for (tab, label) in [
                                (OptTab::Audio, "Audio"),
                                (OptTab::Subs, "Subtitles"),
                                (OptTab::Speed, "Speed"),
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
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                                    let row_w = ow - 32.0;
                                    let mpv = self.mpv.clone();

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
                                                let fill = if active {
                                                    Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 18,
                                                    )
                                                } else {
                                                    Color32::TRANSPARENT
                                                };
                                                let col = if active {
                                                    Color32::WHITE
                                                } else {
                                                    Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 175,
                                                    )
                                                };
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(row_w, 28.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(tr.display_label())
                                                                .size(12.5)
                                                                .color(col),
                                                        )
                                                        .fill(fill),
                                                    )
                                                    .clicked()
                                                {
                                                    if let Some(m) = &mpv {
                                                        m.set_audio_track(tr.id);
                                                    }
                                                    self.flash(tr.display_label());
                                                }
                                            }
                                            ui.add_space(10.0);
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new("Boost")
                                                        .size(12.0)
                                                        .color(Color32::from_gray(150)),
                                                );
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        ui.label(
                                                            egui::RichText::new(format!(
                                                                "{:.0}%",
                                                                self.snap.volume.max(100.0)
                                                            ))
                                                            .size(12.0),
                                                        );
                                                    },
                                                );
                                            });
                                            if let Some(v) = hairline_bar(
                                                ui,
                                                "boost_bar",
                                                row_w,
                                                self.snap.volume.max(100.0),
                                                100.0,
                                                300.0,
                                                None,
                                            ) {
                                                if let Some(m) = &mpv {
                                                    m.set_volume(v);
                                                }
                                            }
                                        }
                                        OptTab::Subs => {
                                            if ui
                                                .add_sized(
                                                    Vec2::new(row_w, 28.0),
                                                    egui::Button::new(
                                                        egui::RichText::new("+ Upload Subtitle")
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

                                            let off_active = self.snap.active_sid <= 0;
                                            if ui
                                                .add_sized(
                                                    Vec2::new(row_w, 26.0),
                                                    egui::Button::new(
                                                        egui::RichText::new("Off")
                                                            .size(12.0)
                                                            .color(if off_active {
                                                                Color32::WHITE
                                                            } else {
                                                                Color32::from_gray(165)
                                                            }),
                                                    )
                                                    .fill(if off_active {
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
                                                        m.set_sub_track(tr.id);
                                                    }
                                                }
                                            }

                                            ui.add_space(6.0);
                                            // Headless hairline seekbar for Subtitle Delay (-10s .. +10s)
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new("Delay")
                                                        .size(12.0)
                                                        .color(Color32::from_gray(150)),
                                                );
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        if ui
                                                            .add(
                                                                egui::Button::new(
                                                                    egui::RichText::new(format!(
                                                                        "{:+.1}s",
                                                                        self.snap.sub_delay
                                                                    ))
                                                                    .size(12.0),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
                                                            )
                                                            .clicked()
                                                        {
                                                            if let Some(m) = &mpv {
                                                                m.set_sub_delay(0.0);
                                                            }
                                                        }
                                                    },
                                                );
                                            });
                                            if let Some(d) = hairline_bar(
                                                ui,
                                                "sub_delay_bar",
                                                row_w,
                                                self.snap.sub_delay,
                                                -10.0,
                                                10.0,
                                                Some(0.0),
                                            ) {
                                                let snapped = (d * 10.0).round() / 10.0;
                                                if let Some(m) = &mpv {
                                                    m.set_sub_delay(snapped);
                                                }
                                            }

                                            // Headless hairline seekbar for Subtitle Speed (0.90x .. 1.10x)
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new("Speed")
                                                        .size(12.0)
                                                        .color(Color32::from_gray(150)),
                                                );
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        if ui
                                                            .add(
                                                                egui::Button::new(
                                                                    egui::RichText::new(format!(
                                                                        "{:.2}x",
                                                                        self.snap.sub_speed
                                                                    ))
                                                                    .size(12.0),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
                                                            )
                                                            .clicked()
                                                        {
                                                            if let Some(m) = &mpv {
                                                                m.set_sub_speed(1.0);
                                                            }
                                                        }
                                                    },
                                                );
                                            });
                                            if let Some(sp) = hairline_bar(
                                                ui,
                                                "sub_speed_bar",
                                                row_w,
                                                self.snap.sub_speed,
                                                0.90,
                                                1.10,
                                                Some(1.0),
                                            ) {
                                                let snapped = (sp * 100.0).round() / 100.0;
                                                if let Some(m) = &mpv {
                                                    m.set_sub_speed(snapped);
                                                }
                                            }

                                            // Headless hairline seekbar for Subtitle Size
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new("Size")
                                                        .size(12.0)
                                                        .color(Color32::from_gray(150)),
                                                );
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        ui.label(
                                                            egui::RichText::new(format!(
                                                                "{:.0}%",
                                                                self.snap.sub_scale * 100.0
                                                            ))
                                                            .size(12.0),
                                                        );
                                                    },
                                                );
                                            });
                                            if let Some(sc) = hairline_bar(
                                                ui,
                                                "sub_size_bar",
                                                row_w,
                                                self.snap.sub_scale,
                                                0.5,
                                                2.0,
                                                None,
                                            ) {
                                                if let Some(m) = &mpv {
                                                    m.set_sub_scale(sc);
                                                }
                                            }

                                            // Online subtitle search (free, zero API key)
                                            ui.add_space(8.0);
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                                                ui.add(
                                                    egui::TextEdit::singleline(&mut self.sub_query)
                                                        .hint_text("Search subtitles")
                                                        .frame(false)
                                                        .margin(Margin::symmetric(8, 5))
                                                        .desired_width(row_w - 94.0),
                                                );
                                                ui.add(
                                                    egui::TextEdit::singleline(&mut self.sub_lang)
                                                        .frame(false)
                                                        .margin(Margin::symmetric(6, 5))
                                                        .desired_width(36.0),
                                                );
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(48.0, 24.0),
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
                                                    item.name.chars().take(30).collect::<String>(),
                                                    item.lang
                                                );
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(row_w, 24.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(label).size(11.5),
                                                        )
                                                        .fill(Color32::from_rgba_unmultiplied(
                                                            255, 255, 255, 10,
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
                                        }
                                        OptTab::Speed => {
                                            for r in [0.5, 0.75, 1.0, 1.25, 1.5, 2.0] {
                                                let active = (self.snap.speed - r).abs() < 0.02;
                                                if ui
                                                    .add_sized(
                                                        Vec2::new(row_w, 28.0),
                                                        egui::Button::new(
                                                            egui::RichText::new(format!("{r}x"))
                                                                .size(12.5)
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
                                                        m.set_speed(r);
                                                    }
                                                    self.flash(format!("{r}x"));
                                                }
                                            }
                                        }
                                    }
                                });
                        });
                    });
                }
            });

        if !self.snap.paused {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }
}

impl eframe::App for StreamdApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        Box::new(move |cc| Ok(Box::new(StreamdApp::new(cc, eng_for_app)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    engine.close_all();
    Ok(())
}
