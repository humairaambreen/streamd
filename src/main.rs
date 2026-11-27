mod blur;
mod engine;
mod mpv;

use blur::GpuBlur;

use eframe::{
    egui::{
        self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, IconData, Key, Layout,
        Margin, Mesh, PaintCallback, Pos2, Rect, Sense, Shape, Stroke, TextureHandle,
        TextureOptions, Vec2, ViewportCommand,
    },
    egui_glow,
};
use engine::{Engine, OnlineLyrics, OnlineSub, OpenReq, SessionMeta, SessionStatus};
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

fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

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

fn fmt_time_precise(sec: f64, duration: f64) -> String {
    if !sec.is_finite() || sec < 0.0 {
        return if duration > 0.0 && duration < 60.0 {
            "0:00.0".into()
        } else {
            "0:00".into()
        };
    }
    if duration > 0.0 && duration < 60.0 {
        let m = (sec / 60.0).floor() as u64;
        let rem = sec - (m as f64 * 60.0);
        format!("{m}:{rem:04.1}")
    } else {
        fmt_time(sec)
    }
}

fn fmt_remaining(sec: f64) -> String {
    if !sec.is_finite() || sec <= 0.05 {
        return String::new();
    }
    if sec < 60.0 {
        return format!("{sec:.1}s left");
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

fn is_lrc_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("lrc")
    )
}

fn is_audio_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some(
            "mp3"
                | "flac"
                | "wav"
                | "m4a"
                | "aac"
                | "ogg"
                | "opus"
                | "wma"
                | "alac"
                | "aiff"
                | "aif"
                | "oga"
                | "mka"
        )
    )
}

#[derive(Clone)]
struct LyricLine {
    time: Option<f64>,
    text: String,
}

fn parse_lrc(raw: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in raw.replace('\r', "").lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Parse [mm:ss.xx] timestamps
        let mut rest = trimmed;
        let mut times = Vec::new();
        while rest.starts_with('[') {
            if let Some(end) = rest.find(']') {
                let tag = &rest[1..end];
                if let Some((m, s)) = tag.split_once(':') {
                    if let (Ok(mm), Ok(ss)) = (m.trim().parse::<f64>(), s.trim().parse::<f64>()) {
                        times.push(mm * 60.0 + ss);
                    }
                }
                rest = rest[end + 1..].trim_start();
            } else {
                break;
            }
        }
        if !times.is_empty() {
            let txt = rest.trim().to_string();
            if !txt.is_empty() {
                for t in times {
                    out.push(LyricLine {
                        time: Some(t),
                        text: txt.clone(),
                    });
                }
            }
        } else if !trimmed.starts_with('[') {
            out.push(LyricLine {
                time: None,
                text: trimmed.to_string(),
            });
        }
    }
    if out.iter().any(|l| l.time.is_some()) {
        out.sort_by(|a, b| {
            a.time
                .unwrap_or(0.0)
                .partial_cmp(&b.time.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    out
}

fn is_photo_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some(
            "jpg"
                | "jpeg"
                | "png"
                | "webp"
                | "gif"
                | "bmp"
                | "tiff"
                | "tif"
                | "heic"
                | "heif"
                | "avif"
                | "jxl"
                | "dng"
        )
    )
}

/// Decodes any photo format safely (with content-sniffed format so misnamed PNG/JPEG never crashes,
/// plus macOS Apple ImageIO `sips` and cross-platform `ffmpeg` fallback for HEIC/HEIF/AVIF/DNG).
fn decode_any_photo(path: &Path, max_dim: u32) -> Option<ColorImage> {
    let native = std::panic::catch_unwind(|| {
        image::ImageReader::open(path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()
    })
    .ok()
    .flatten();

    let img = if let Some(i) = native {
        if i.width().max(i.height()) > max_dim {
            i.thumbnail(max_dim, max_dim)
        } else {
            i
        }
    } else {
        // Fallback 1 (macOS): native Apple ImageIO via `sips` (handles HEIC, HEIF, AVIF, DNG/RAW)
        #[cfg(target_os = "macos")]
        let decoded = {
            let tmp = std::env::temp_dir().join(format!(
                "streamd_img_{:x}.jpg",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let ok = std::process::Command::new("sips")
                .args([
                    "-s",
                    "format",
                    "jpeg",
                    "-Z",
                    &max_dim.to_string(),
                    &path.to_string_lossy(),
                    "--out",
                    &tmp.to_string_lossy(),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_or(false, |s| s.success());
            let loaded = if ok { image::open(&tmp).ok() } else { None };
            let _ = std::fs::remove_file(&tmp);
            loaded
        };
        #[cfg(not(target_os = "macos"))]
        let decoded: Option<image::DynamicImage> = None;

        // Fallback 2 (cross-platform): ffmpeg image decode
        if let Some(d) = decoded {
            d
        } else {
            let out = std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-i",
                    &path.to_string_lossy(),
                    "-frames:v",
                    "1",
                    "-vf",
                    &format!("scale='min(iw,{max_dim})':-2"),
                    "-f",
                    "mjpeg",
                    "pipe:1",
                ])
                .stdin(std::process::Stdio::null())
                .output()
                .ok()?;
            if !out.status.success() || out.stdout.is_empty() {
                return None;
            }
            image::load_from_memory(&out.stdout).ok()?
        }
    };

    let rgba = img.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
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

fn scan_folder_media(dir: &Path) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.is_file() && (is_video_ext(p) || is_photo_ext(p) || is_audio_ext(p)))
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
    list
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

fn fit_aspect_rect(container: Rect, aspect: f32) -> Rect {
    let a = if aspect.is_finite() && aspect > 0.1 {
        aspect
    } else {
        16.0 / 9.0
    };
    let cw = container.width().max(1.0);
    let ch = container.height().max(1.0);
    let (w, h) = if cw / ch > a {
        (ch * a, ch)
    } else {
        (cw, cw / a)
    };
    Rect::from_center_size(container.center(), Vec2::new(w, h))
}

fn paint_frosted(
    painter: &egui::Painter,
    blur: &Option<Arc<GpuBlur>>,
    rect: Rect,
    tint: [f32; 4],
    alpha: f32,
    fade_right: bool,
) {
    if let Some(b) = blur.clone() {
        let cb = egui_glow::CallbackFn::new(move |info, gl_painter| {
            let vp = info.viewport_in_pixels();
            b.draw_frosted_region(
                gl_painter.gl(),
                gl_painter.intermediate_fbo(),
                vp.left_px,
                vp.from_bottom_px,
                vp.width_px,
                vp.height_px,
                tint,
                alpha,
                fade_right,
            );
        });
        painter.add(PaintCallback {
            rect,
            callback: Arc::new(cb),
        });
    } else if fade_right {
        paint_fade_h(
            painter,
            rect,
            Color32::from_rgba_unmultiplied(0, 0, 0, (195.0 * alpha) as u8),
            Color32::TRANSPARENT,
        );
    } else {
        painter.rect_filled(
            rect,
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(12, 12, 14, (232.0 * alpha) as u8),
        );
    }
}

/// Paints a texture rotated by 0/90/180/270 degrees directly on GPU (0ms, zero CPU copy!).
fn paint_rotated_image(
    p: &egui::Painter,
    rect: Rect,
    tex_id: egui::TextureId,
    rotate_deg: i64,
    tint: Color32,
) {
    let uvs = match rotate_deg.rem_euclid(360) {
        90 => [
            Pos2::new(0.0, 1.0),
            Pos2::new(0.0, 0.0),
            Pos2::new(1.0, 0.0),
            Pos2::new(1.0, 1.0),
        ],
        180 => [
            Pos2::new(1.0, 1.0),
            Pos2::new(0.0, 1.0),
            Pos2::new(0.0, 0.0),
            Pos2::new(1.0, 0.0),
        ],
        270 => [
            Pos2::new(1.0, 0.0),
            Pos2::new(1.0, 1.0),
            Pos2::new(0.0, 1.0),
            Pos2::new(0.0, 0.0),
        ],
        _ => [
            Pos2::new(0.0, 0.0),
            Pos2::new(1.0, 0.0),
            Pos2::new(1.0, 1.0),
            Pos2::new(0.0, 1.0),
        ],
    };
    let mut mesh = Mesh::with_texture(tex_id);
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_top(),
        uv: uvs[0],
        color: tint,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_top(),
        uv: uvs[1],
        color: tint,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_bottom(),
        uv: uvs[2],
        color: tint,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_bottom(),
        uv: uvs[3],
        color: tint,
    });
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));
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
    // Pure vector brightness feedback on hover (NO gray box!)
    let col = if resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 185)
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

fn draw_sliders_icon(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    let st = Stroke::new(1.5_f32, col);
    // Minimal 2-line Apple-style control sliders icon
    p.line_segment(
        [
            Pos2::new(c.x - 6.5, c.y - 3.2),
            Pos2::new(c.x + 6.5, c.y - 3.2),
        ],
        st,
    );
    p.circle_filled(Pos2::new(c.x - 2.0, c.y - 3.2), 2.1, col);
    p.line_segment(
        [
            Pos2::new(c.x - 6.5, c.y + 3.2),
            Pos2::new(c.x + 6.5, c.y + 3.2),
        ],
        st,
    );
    p.circle_filled(Pos2::new(c.x + 2.5, c.y + 3.2), 2.1, col);
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

#[derive(Clone, PartialEq, Eq)]
enum MediaKind {
    Video,
    Audio,
    Photo,
    Folder,
}

#[derive(Clone)]
struct LibraryItem {
    name: String,
    source: String, // URL, local file path, or folder path
    kind: MediaKind,
    is_local: bool,
    position: f64,
    duration: f64,
    poster_thumb: Option<TextureHandle>, // shown when card is on side peek (or folder cover)
    resume_thumb: Option<TextureHandle>, // exact paused frame shown when card is focused in center
    children: Vec<LibraryItem>,          // isolated items when kind == MediaKind::Folder
}

struct PhotoViewer {
    source: String,
    title: String,
    tex: Option<TextureHandle>,
    zoom: f32,
    rotate_deg: i64,
    slideshow: bool,
    interval_secs: f32,
    slide_started: Instant,
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
    blur: Option<Arc<GpuBlur>>,

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
    active_folder: Option<usize>,
    carousel_target: usize,
    carousel_pos: f32,
    folder_carousel_target: usize,
    folder_carousel_pos: f32,
    scroll_accum: f32,
    dismiss_y: f32,
    last_scroll_step: Instant,
    expand_t: f32,
    morph_start: Instant,
    home_entered_at: Instant,
    player_open: bool,
    closing_morph: bool,
    pending_close_since: Option<Instant>,
    morph_tex: Option<TextureHandle>,
    poster_requested_for: Option<String>,
    pending_resume_pos: Option<f64>,
    card_thumb_rx: mpsc::Receiver<(String, bool, ColorImage)>, // (source, is_poster, image)
    card_thumb_tx: mpsc::Sender<(String, bool, ColorImage)>,
    photo_viewer: Option<PhotoViewer>,
    photo_rx: mpsc::Receiver<(String, ColorImage)>,
    photo_tx: mpsc::Sender<(String, ColorImage)>,

    // Active session (stays warm & paused when returning to Cover Flow; wiped on new video or app exit)
    session: Option<SessionMeta>,
    session_source: String,
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

    // Audio mode & Live Synced Lyrics (lrclib.net + local .lrc upload)
    is_audio_session: bool,
    lyrics: Vec<LyricLine>,
    lyrics_delay: f64,
    lyrics_query: String,
    lyrics_msg: String,
    lyrics_results: Vec<OnlineLyrics>,
    lyrics_rx: Option<mpsc::Receiver<anyhow::Result<Vec<OnlineLyrics>>>>,
    lyrics_auto_fetched: bool,

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

        let blur = cc.gl.as_ref().map(|gl| Arc::new(GpuBlur::new(gl)));
        let (thumb_tx, thumb_rx) = mpsc::channel();
        let (card_thumb_tx, card_thumb_rx) = mpsc::channel();
        let (photo_tx, photo_rx) = mpsc::channel();

        let mut app = Self {
            engine,
            mpv,
            mpv_err,
            blur,
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
            active_folder: None,
            carousel_target: 0,
            carousel_pos: 0.0,
            folder_carousel_target: 0,
            folder_carousel_pos: 0.0,
            scroll_accum: 0.0,
            dismiss_y: 0.0,
            last_scroll_step: Instant::now() - Duration::from_secs(1),
            expand_t: 0.0,
            morph_start: Instant::now() - Duration::from_secs(1),
            home_entered_at: Instant::now() - Duration::from_secs(2),
            player_open: false,
            closing_morph: false,
            pending_close_since: None,
            morph_tex: None,
            poster_requested_for: None,
            pending_resume_pos: None,
            card_thumb_rx,
            card_thumb_tx,
            photo_viewer: None,
            photo_rx,
            photo_tx,
            session: None,
            session_source: String::new(),
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
            is_audio_session: false,
            lyrics: Vec::new(),
            lyrics_delay: 0.0,
            lyrics_query: String::new(),
            lyrics_msg: String::new(),
            lyrics_results: Vec::new(),
            lyrics_rx: None,
            lyrics_auto_fetched: false,
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
            if p.is_dir() {
                app.add_folder(p, &cc.egui_ctx);
            } else if p.is_file() {
                if is_photo_ext(&p) {
                    app.open_photo_path(p, false, &cc.egui_ctx);
                } else {
                    app.open_local_path(p, true);
                }
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

    fn spawn_photo_thumb(
        &self,
        source_key: String,
        path: PathBuf,
        max_dim: u32,
        ctx: &egui::Context,
    ) {
        let tx = self.card_thumb_tx.clone();
        let egui_ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(ci) = decode_any_photo(&path, max_dim) {
                let _ = tx.send((source_key, true, ci));
                egui_ctx.request_repaint();
            }
        });
    }

    fn spawn_audio_art(&self, source_key: String, path: String, ctx: &egui::Context) {
        let eng = self.engine.clone();
        let tx = self.card_thumb_tx.clone();
        let egui_ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(jpeg) = eng.extract_audio_art(&path) {
                if let Ok(img) = image::load_from_memory(&jpeg) {
                    let rgba = img.to_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        rgba.as_raw(),
                    );
                    let _ = tx.send((source_key, true, ci));
                    egui_ctx.request_repaint();
                }
            }
        });
    }

    fn start_lyrics_search(&mut self, query: String) {
        if query.trim().is_empty() {
            return;
        }
        self.lyrics_msg = "Searching…".into();
        let eng = self.engine.clone();
        let (tx, rx) = mpsc::channel();
        self.lyrics_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(eng.search_lyrics(&query));
        });
    }

    fn load_lrc_path(&mut self, path: &Path) {
        if let Ok(txt) = std::fs::read_to_string(path) {
            let parsed = parse_lrc(&txt);
            if !parsed.is_empty() {
                self.lyrics = parsed;
                self.lyrics_msg = "Loaded".into();
                self.flash("Lyrics loaded");
            }
        }
    }

    fn add_folder(&mut self, dir: PathBuf, ctx: &egui::Context) {
        let files = scan_folder_media(&dir);
        if files.is_empty() {
            self.err_msg = "No videos or photos found in that folder.".into();
            return;
        }
        self.err_msg.clear();
        let folder_src = dir.to_string_lossy().into_owned();
        let folder_name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Folder")
            .to_string();

        let mut children = Vec::with_capacity(files.len());
        for p in &files {
            let p_str = p.to_string_lossy().into_owned();
            let raw_name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("item")
                .to_string();
            let (clean, _) = clean_title(&raw_name);
            let kind = if is_photo_ext(p) {
                MediaKind::Photo
            } else if is_audio_ext(p) {
                MediaKind::Audio
            } else {
                MediaKind::Video
            };
            children.push(LibraryItem {
                name: clean,
                source: p_str.clone(),
                kind: kind.clone(),
                is_local: true,
                position: 0.0,
                duration: 0.0,
                poster_thumb: None,
                resume_thumb: None,
                children: Vec::new(),
            });
            if children.len() <= 12 {
                if kind == MediaKind::Photo {
                    self.spawn_photo_thumb(p_str, p.clone(), 640, ctx);
                } else if kind == MediaKind::Audio {
                    self.spawn_audio_art(p_str.clone(), p_str, ctx);
                } else {
                    self.spawn_card_thumb(p_str.clone(), String::new(), Some(p_str), 3, true, ctx);
                }
            }
        }

        if let Some(first) = files.first() {
            if is_photo_ext(first) {
                self.spawn_photo_thumb(folder_src.clone(), first.clone(), 640, ctx);
            } else if is_audio_ext(first) {
                self.spawn_audio_art(
                    folder_src.clone(),
                    first.to_string_lossy().into_owned(),
                    ctx,
                );
            } else {
                self.spawn_card_thumb(
                    folder_src.clone(),
                    String::new(),
                    Some(first.to_string_lossy().into_owned()),
                    3,
                    true,
                    ctx,
                );
            }
        }

        if let Some(pos) = self.library.iter().position(|x| x.source == folder_src) {
            self.library[pos].children = children;
            self.carousel_target = pos + 1;
        } else {
            self.library.insert(
                0,
                LibraryItem {
                    name: folder_name,
                    source: folder_src,
                    kind: MediaKind::Folder,
                    is_local: true,
                    position: 0.0,
                    duration: 0.0,
                    poster_thumb: None,
                    resume_thumb: None,
                    children,
                },
            );
            self.carousel_target = 1;
        }
        self.carousel_pos = self.carousel_target as f32;
        // Open straight into the isolated folder Cover Flow!
        self.active_folder = Some(self.carousel_target - 1);
        self.folder_carousel_target = 0;
        self.folder_carousel_pos = 0.0;
    }

    fn open_photo_path(&mut self, path: PathBuf, start_slideshow: bool, ctx: &egui::Context) {
        self.err_msg.clear();
        self.destroy_active_session();
        let src = path.to_string_lossy().into_owned();
        let raw_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Photo")
            .to_string();
        let (title, _) = clean_title(&raw_name);

        // If not inside an isolated folder, populate queue from sibling media & add to root library
        if self.active_folder.is_none() {
            if let Some(parent) = path.parent() {
                let all = scan_folder_media(parent);
                self.queue_idx = all.iter().position(|p| p == &path).unwrap_or(0);
                self.queue = all;
            }
            if !self.library.iter().any(|x| x.source == src) {
                self.library.insert(
                    0,
                    LibraryItem {
                        name: title.clone(),
                        source: src.clone(),
                        kind: MediaKind::Photo,
                        is_local: true,
                        position: 0.0,
                        duration: 0.0,
                        poster_thumb: None,
                        resume_thumb: None,
                        children: Vec::new(),
                    },
                );
                self.carousel_target = 1;
                self.carousel_pos = 1.0;
                self.spawn_photo_thumb(src.clone(), path.clone(), 640, ctx);
            }
        }

        // Check if we already have a cached thumbnail to display immediately while full-res decodes
        let initial_tex = self.find_item_thumb(&src);
        self.photo_viewer = Some(PhotoViewer {
            source: src.clone(),
            title,
            tex: initial_tex,
            zoom: 1.0,
            rotate_deg: 0,
            slideshow: start_slideshow,
            interval_secs: 4.0,
            slide_started: Instant::now(),
        });
        self.last_mouse_move = Instant::now();

        // Decode full-resolution photo (including HEIC/HEIF/AVIF/DNG!) off the UI thread
        let tx = self.photo_tx.clone();
        let egui_ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(ci) = decode_any_photo(&path, 3840) {
                let _ = tx.send((src, ci));
                egui_ctx.request_repaint();
            }
        });
    }

    fn find_item_thumb(&self, source: &str) -> Option<TextureHandle> {
        for item in &self.library {
            if item.source == source {
                return item
                    .resume_thumb
                    .clone()
                    .or_else(|| item.poster_thumb.clone());
            }
            for ch in &item.children {
                if ch.source == source {
                    return ch.resume_thumb.clone().or_else(|| ch.poster_thumb.clone());
                }
            }
        }
        None
    }

    fn step_media_queue(&mut self, delta: isize, keep_slideshow: bool, ctx: &egui::Context) {
        if let Some(fi) = self.active_folder {
            if let Some(folder) = self.library.get(fi) {
                let len = folder.children.len();
                if len == 0 {
                    return;
                }
                let next = (self.folder_carousel_target as isize + delta).rem_euclid(len as isize)
                    as usize;
                self.folder_carousel_target = next;
                self.folder_carousel_pos = next as f32;
                let item = folder.children[next].clone();
                let path = PathBuf::from(&item.source);
                if item.kind == MediaKind::Photo {
                    self.open_photo_path(path, keep_slideshow, ctx);
                } else {
                    self.photo_viewer = None;
                    self.open_local_path(path, false);
                }
                return;
            }
        }
        if self.queue.len() <= 1 {
            return;
        }
        let next = (self.queue_idx as isize + delta).rem_euclid(self.queue.len() as isize) as usize;
        self.queue_idx = next;
        let path = self.queue[next].clone();
        if is_photo_ext(&path) {
            self.open_photo_path(path, keep_slideshow, ctx);
        } else {
            self.photo_viewer = None;
            self.open_local_path(path, false);
        }
    }

    fn start_open_url(&mut self) {
        if self.opening || self.url.trim().is_empty() {
            return;
        }
        let trimmed = self.url.trim().to_string();
        let maybe_path = PathBuf::from(trimmed.strip_prefix("file://").unwrap_or(&trimmed));
        if maybe_path.is_file() {
            self.open_local_path(maybe_path, true);
            return;
        }

        self.opening = true;
        self.err_msg.clear();
        self.destroy_active_session();
        self.queue.clear();
        self.queue_idx = 0;
        let req = OpenReq {
            url: trimmed,
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
        self.destroy_active_session();
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
        is_poster: bool,
        ctx: &egui::Context,
    ) {
        let eng = self.engine.clone();
        let tx = self.card_thumb_tx.clone();
        let egui_ctx = ctx.clone();
        let dummy_gen = Arc::new(AtomicU64::new(1));
        std::thread::spawn(move || {
            if let Some(jpeg) =
                eng.fetch_thumb(&sess_id, local_path.as_deref(), at_sec, 960, &dummy_gen, 1)
            {
                if let Ok(img) =
                    image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                {
                    let rgba = img.to_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        rgba.as_raw(),
                    );
                    let _ = tx.send((source_key, is_poster, ci));
                    egui_ctx.request_repaint();
                }
            }
        });
    }

    fn capture_mpv_frame_async(&self, src_key: String, ctx: &egui::Context) {
        let Some(mpv) = self.mpv.clone() else { return };
        let tx = self.card_thumb_tx.clone();
        let egui_ctx = ctx.clone();
        std::thread::spawn(move || {
            let tmp = std::env::temp_dir().join(format!(
                "streamd_cap_{:x}.jpg",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            mpv.screenshot_to_file(&tmp.to_string_lossy());
            let rot = mpv.effective_rotate();
            let bytes = std::fs::read(&tmp).ok();
            let _ = std::fs::remove_file(&tmp);
            if let Some(bytes) = bytes {
                if let Ok(mut img) = image::load_from_memory(&bytes) {
                    img = match rot {
                        90 => img.rotate90(),
                        180 => img.rotate180(),
                        270 => img.rotate270(),
                        _ => img,
                    };
                    let rgba = img.to_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        rgba.as_raw(),
                    );
                    let _ = tx.send((src_key, false, ci));
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
        self.player_open = true;
        self.closing_morph = false;
        self.expand_t = 0.0;
        self.morph_start = Instant::now();
        self.poster_requested_for = None;

        let is_audio =
            is_audio_ext(Path::new(&meta.name)) || is_audio_ext(Path::new(&meta.media_url));
        self.is_audio_session = is_audio;
        self.lyrics.clear();
        self.lyrics_delay = 0.0;
        self.lyrics_query = t.clone();
        self.lyrics_msg.clear();
        self.lyrics_results.clear();
        self.lyrics_auto_fetched = false;

        let source = if meta.is_local {
            meta.media_url.clone()
        } else {
            self.url.trim().to_string()
        };
        self.session_source = source.clone();
        if !source.is_empty() && self.active_folder.is_none() {
            if let Some(pos_idx) = self.library.iter().position(|x| x.source == source) {
                self.carousel_target = pos_idx + 1;
                let it = &self.library[pos_idx];
                self.morph_tex = it.resume_thumb.clone().or_else(|| it.poster_thumb.clone());
            } else {
                self.library.insert(
                    0,
                    LibraryItem {
                        name: t,
                        source: source.clone(),
                        kind: if is_audio {
                            MediaKind::Audio
                        } else {
                            MediaKind::Video
                        },
                        is_local: meta.is_local,
                        position: 0.0,
                        duration: 0.0,
                        poster_thumb: None,
                        resume_thumb: None,
                        children: Vec::new(),
                    },
                );
                self.carousel_target = 1;
                self.morph_tex = None;
            }
            self.carousel_pos = self.carousel_target as f32;
        } else if !source.is_empty() {
            self.morph_tex = self.find_item_thumb(&source);
        }
        // Check for sibling .lrc file automatically when playing local audio
        if is_audio && meta.is_local {
            let lrc_path = Path::new(&meta.media_url).with_extension("lrc");
            if lrc_path.is_file() {
                self.load_lrc_path(&lrc_path);
            }
        }

        if let Some(mpv) = &self.mpv {
            mpv.load_url_at(&meta.media_url, self.pending_resume_pos);
        }
        self.session = Some(meta);
        self.last_mouse_move = Instant::now();
    }

    /// Re-enters the already-warm paused session instantaneously (0ms reload, 0ms black flash!).
    fn resume_warm_session(&mut self, start_over: bool) {
        if let Some(mpv) = &self.mpv {
            if start_over {
                mpv.seek_absolute(0.0);
                self.snap.position = 0.0;
                if let Some(item) = self
                    .library
                    .iter_mut()
                    .find(|x| x.source == self.session_source)
                {
                    item.position = 0.0;
                }
            }
            mpv.set_paused(false);
        }
        if let Some(item) = self
            .library
            .iter()
            .find(|x| x.source == self.session_source)
        {
            self.morph_tex = item
                .resume_thumb
                .clone()
                .or_else(|| item.poster_thumb.clone());
        }
        self.player_open = true;
        self.closing_morph = false;
        self.expand_t = 0.0;
        self.morph_start = Instant::now();
        self.last_mouse_move = Instant::now();
    }

    fn begin_close_morph(&mut self, ctx: &egui::Context) {
        if self.closing_morph || self.pending_close_since.is_some() {
            return;
        }
        // Pause immediately and read exact millisecond position; keep the live paused frame on screen
        // for ~20-40ms while the exact frame texture finishes capturing so the poster thumbnail NEVER flashes!
        let exact_pos = if let Some(mpv) = &self.mpv {
            mpv.set_paused(true);
            mpv.exact_position()
        } else {
            self.snap.position
        };
        self.snap.position = exact_pos;
        if let Some(s) = self.session.clone() {
            let src = self.session_source.clone();
            if let Some(item) = self.library.iter_mut().find(|x| x.source == src) {
                if !self.snap.media_title.is_empty() {
                    item.name = self.snap.media_title.clone();
                } else if !self.title.is_empty() {
                    item.name = self.title.clone();
                }
                if self.snap.duration > 0.0 {
                    item.position = exact_pos.min(self.snap.duration);
                    item.duration = self.snap.duration;
                }
                self.morph_tex = item
                    .resume_thumb
                    .clone()
                    .or_else(|| item.poster_thumb.clone());
            }
            let _ = s;
            self.capture_mpv_frame_async(src, ctx);
        }
        self.opts_open = false;
        self.studio_tab = None;
        self.pending_close_since = Some(Instant::now());
        ctx.request_repaint();
    }

    fn finish_close_to_home(&mut self) {
        // Keep session & .part alive in background while browsing Cover Flow (wiped on new video or exit)
        self.player_open = false;
        self.closing_morph = false;
        self.pending_close_since = None;
        self.expand_t = 0.0;
        self.home_entered_at = Instant::now();
        self.opts_open = false;
        self.studio_tab = None;
        self.pause_t = 0.0;
        self.pause_since = None;
    }

    fn destroy_active_session(&mut self) {
        self.thumb_gen.fetch_add(1, Relaxed);
        if let Some(mpv) = &self.mpv {
            mpv.stop();
        }
        if let Some(s) = self.session.take() {
            if !s.is_local {
                self.engine.close(&s.id);
            }
        }
        self.session_source.clear();
        self.player_open = false;
        self.closing_morph = false;
        self.expand_t = 0.0;
        self.pending_resume_pos = None;
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
        let mut photos = Vec::new();
        for f in dropped {
            if let Some(path) = f.path {
                if path.is_dir() {
                    self.photo_viewer = None;
                    self.player_open = false;
                    self.add_folder(path, ctx);
                    return;
                } else if is_lrc_ext(&path) {
                    self.load_lrc_path(&path);
                } else if is_sub_ext(&path) {
                    if let Some(mpv) = &self.mpv {
                        mpv.add_sub_file(&path.to_string_lossy());
                        self.flash("Subtitle loaded");
                    }
                } else if is_photo_ext(&path) {
                    photos.push(path);
                } else {
                    videos.push(path);
                }
            }
        }
        if !photos.is_empty() {
            let first = photos.remove(0);
            self.open_photo_path(first, false, ctx);
        } else if !videos.is_empty() {
            self.photo_viewer = None;
            if self.session.is_some() {
                self.destroy_active_session();
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
                eng.fetch_thumb(&id, local_path.as_deref(), bucket, 260, &gen_arc, my_gen)
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
        if !self.player_open || self.session.is_none() || ctx.wants_keyboard_input() {
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

    fn draw_photo_viewer(&mut self, ctx: &egui::Context) {
        let Some(pv) = self.photo_viewer.as_mut() else {
            return;
        };

        // Keyboard shortcuts for photo viewer & slideshow
        let mut step_delta: isize = 0;
        let mut close_viewer = false;
        let mut want_fs_cmd: Option<bool> = None;

        ctx.input(|i| {
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::L) {
                step_delta = 1;
            }
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::J) {
                step_delta = -1;
            }
            if i.key_pressed(Key::Space) {
                pv.slideshow = !pv.slideshow;
                pv.slide_started = Instant::now();
            }
            if i.key_pressed(Key::Escape) {
                if self.is_fullscreen {
                    self.is_fullscreen = false;
                    want_fs_cmd = Some(false);
                } else {
                    close_viewer = true;
                }
            }
            if i.key_pressed(Key::F) {
                self.is_fullscreen = !self.is_fullscreen;
                want_fs_cmd = Some(self.is_fullscreen);
            }
            if i.key_pressed(Key::R) {
                pv.rotate_deg = (pv.rotate_deg + 90) % 360;
            }
            if i.raw_scroll_delta.y.abs() > 1.0 {
                pv.zoom = (pv.zoom + i.raw_scroll_delta.y * 0.004).clamp(1.0, 3.5);
            }
        });

        if let Some(fs) = want_fs_cmd {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(fs));
        }

        // Auto-advance slideshow timer
        let slide_progress = if pv.slideshow {
            let el = pv.slide_started.elapsed().as_secs_f32();
            if el >= pv.interval_secs {
                step_delta = 1;
                1.0
            } else {
                ctx.request_repaint();
                (el / pv.interval_secs).clamp(0.0, 1.0)
            }
        } else {
            0.0
        };

        let title = pv.title.clone();
        let tex_opt = pv.tex.clone();
        let zoom = pv.zoom;
        let rot = pv.rotate_deg;
        let slideshow = pv.slideshow;
        let interval = pv.interval_secs;
        let mut toggle_slide = false;
        let mut next_interval: Option<f32> = None;
        let mut do_rotate = false;
        let mut toggle_zoom = false;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();

                if let Some(tex) = &tex_opt {
                    let raw_aspect = tex.aspect_ratio();
                    let disp_aspect = if rot.rem_euclid(180) != 0 {
                        1.0 / raw_aspect.max(0.05)
                    } else {
                        raw_aspect
                    };
                    let base = fit_aspect_rect(full_rect, disp_aspect);
                    let img_r = Rect::from_center_size(base.center(), base.size() * zoom);
                    paint_rotated_image(ui.painter(), img_r, tex.id(), rot, Color32::WHITE);
                }

                // Double-click toggles 1x <-> 2x zoom
                let bg = ui.interact(
                    Rect::from_min_max(
                        Pos2::new(full_rect.left(), full_rect.top() + 56.0),
                        Pos2::new(full_rect.right(), full_rect.bottom() - 68.0),
                    ),
                    egui::Id::new("photo_bg"),
                    Sense::click(),
                );
                if bg.double_clicked() {
                    toggle_zoom = true;
                }

                // Top fade + Back + Title
                paint_fade_v(
                    ui.painter(),
                    Rect::from_min_size(full_rect.min, Vec2::new(full_rect.width(), 72.0)),
                    Color32::from_rgba_unmultiplied(0, 0, 0, 165),
                    Color32::TRANSPARENT,
                );
                let top_bar = Rect::from_min_size(
                    Pos2::new(full_rect.left() + 20.0, full_rect.top() + 14.0),
                    Vec2::new(full_rect.width() - 40.0, 32.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(top_bar), |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                        if icon_btn(ui, Vec2::new(28.0, 28.0), draw_back).clicked() {
                            close_viewer = true;
                        }
                        ui.label(egui::RichText::new(&title).size(14.5));
                    });
                });

                // Bottom fade + Slideshow / Prev / Next / Interval / Zoom / Fullscreen
                paint_fade_v(
                    ui.painter(),
                    Rect::from_min_size(
                        Pos2::new(full_rect.left(), full_rect.bottom() - 88.0),
                        Vec2::new(full_rect.width(), 88.0),
                    ),
                    Color32::TRANSPARENT,
                    Color32::from_rgba_unmultiplied(0, 0, 0, 195),
                );

                // Hairline slideshow countdown bar
                if slideshow {
                    let rail = Rect::from_min_size(
                        Pos2::new(full_rect.left() + 22.0, full_rect.bottom() - 50.0),
                        Vec2::new(full_rect.width() - 44.0, 2.0),
                    );
                    ui.painter().rect_filled(
                        rail,
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 48),
                    );
                    ui.painter().rect_filled(
                        Rect::from_min_size(
                            rail.min,
                            Vec2::new(rail.width() * slide_progress, 2.0),
                        ),
                        CornerRadius::ZERO,
                        Color32::WHITE,
                    );
                }

                let ctrl = Rect::from_min_size(
                    Pos2::new(full_rect.left() + 22.0, full_rect.bottom() - 38.0),
                    Vec2::new(full_rect.width() - 44.0, 28.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(ctrl), |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(10.0, 0.0);
                        if icon_btn(ui, Vec2::new(26.0, 28.0), draw_prev).clicked() {
                            step_delta = -1;
                        }
                        if icon_btn(ui, Vec2::new(28.0, 28.0), |p, r, c| {
                            if slideshow {
                                draw_pause(p, r, c);
                            } else {
                                draw_play(p, r, c);
                            }
                        })
                        .clicked()
                        {
                            toggle_slide = true;
                        }
                        if icon_btn(ui, Vec2::new(26.0, 28.0), draw_next).clicked() {
                            step_delta = 1;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(if slideshow {
                                        "Slideshow On"
                                    } else {
                                        "Slideshow"
                                    })
                                    .size(12.5)
                                    .color(if slideshow {
                                        Color32::WHITE
                                    } else {
                                        Color32::from_gray(165)
                                    }),
                                )
                                .fill(Color32::TRANSPARENT),
                            )
                            .clicked()
                        {
                            toggle_slide = true;
                        }
                        for secs in [3.0_f32, 5.0, 8.0] {
                            let on = (interval - secs).abs() < 0.2;
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(format!("{}s", secs as u32))
                                            .size(12.0)
                                            .color(if on {
                                                Color32::WHITE
                                            } else {
                                                Color32::from_gray(125)
                                            }),
                                    )
                                    .fill(Color32::TRANSPARENT),
                                )
                                .clicked()
                            {
                                next_interval = Some(secs);
                            }
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_btn(ui, Vec2::new(28.0, 28.0), |p, r, c| {
                                draw_fullscreen(p, r, c, self.is_fullscreen);
                            })
                            .clicked()
                            {
                                self.is_fullscreen = !self.is_fullscreen;
                                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(
                                    self.is_fullscreen,
                                ));
                            }
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Rotate")
                                            .size(12.0)
                                            .color(Color32::from_gray(175)),
                                    )
                                    .fill(Color32::TRANSPARENT),
                                )
                                .clicked()
                            {
                                do_rotate = true;
                            }
                        });
                    });
                });
            });

        if close_viewer {
            self.photo_viewer = None;
            self.home_entered_at = Instant::now();
            return;
        }
        if let Some(pv) = self.photo_viewer.as_mut() {
            if toggle_slide {
                pv.slideshow = !pv.slideshow;
                pv.slide_started = Instant::now();
            }
            if let Some(s) = next_interval {
                pv.interval_secs = s;
                pv.slide_started = Instant::now();
            }
            if toggle_zoom {
                pv.zoom = if pv.zoom > 1.2 { 1.0 } else { 2.0 };
            }
            if do_rotate {
                pv.rotate_deg = (pv.rotate_deg + 90) % 360;
            }
        }
        if step_delta != 0 {
            let keep_slide = self.photo_viewer.as_ref().map_or(false, |p| p.slideshow);
            self.step_media_queue(step_delta, keep_slide, ctx);
        }
    }

    fn draw_home(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.05);
        let in_folder = self.active_folder;
        let total_cards = if let Some(fi) = in_folder {
            self.library.get(fi).map_or(0, |f| f.children.len())
        } else {
            1 + self.library.len()
        };

        let (c_target, c_pos) = if in_folder.is_some() {
            (
                &mut self.folder_carousel_target,
                &mut self.folder_carousel_pos,
            )
        } else {
            (&mut self.carousel_target, &mut self.carousel_pos)
        };
        if total_cards > 0 && *c_target >= total_cards {
            *c_target = total_cards - 1;
        }

        // Debounced Cover-Flow navigation (horizontal swipe = left/right, vertical swipe up = iOS dismiss!)
        let mut exit_folder = false;
        let mut dismiss_current = false;
        let can_dismiss = in_folder.is_some() || *c_target > 0;
        if !ctx.wants_keyboard_input() && total_cards > 0 {
            ctx.input(|i| {
                if in_folder.is_some() && i.key_pressed(Key::Escape) {
                    exit_folder = true;
                }
                if can_dismiss && (i.key_pressed(Key::Backspace) || i.key_pressed(Key::Delete)) {
                    dismiss_current = true;
                }
                if total_cards > 1 {
                    if i.key_pressed(Key::ArrowRight) && *c_target + 1 < total_cards {
                        *c_target += 1;
                    }
                    if i.key_pressed(Key::ArrowLeft) && *c_target > 0 {
                        *c_target -= 1;
                    }
                }
                let dx = i.raw_scroll_delta.x;
                let dy = i.raw_scroll_delta.y;
                if dx.abs() > dy.abs() && dx.abs() > 0.5 && total_cards > 1 {
                    self.scroll_accum += dx;
                    if self.last_scroll_step.elapsed() >= Duration::from_millis(260) {
                        if self.scroll_accum < -38.0 && *c_target + 1 < total_cards {
                            *c_target += 1;
                            self.scroll_accum = 0.0;
                            self.last_scroll_step = Instant::now();
                        } else if self.scroll_accum > 38.0 && *c_target > 0 {
                            *c_target -= 1;
                            self.scroll_accum = 0.0;
                            self.last_scroll_step = Instant::now();
                        }
                    }
                } else if can_dismiss && dy.abs() > dx.abs() && dy > 1.5 {
                    // Two-finger swipe UP lifts the centered card out of Cover Flow!
                    self.dismiss_y = (self.dismiss_y - dy * 1.15).max(-180.0);
                    if self.dismiss_y <= -95.0 {
                        dismiss_current = true;
                        self.dismiss_y = 0.0;
                    }
                } else {
                    self.scroll_accum *= 0.8;
                    self.dismiss_y *= 1.0 - (dt * 12.0).min(1.0);
                    if self.dismiss_y.abs() < 0.5 {
                        self.dismiss_y = 0.0;
                    }
                }
            });
        }
        if exit_folder {
            self.active_folder = None;
            self.dismiss_y = 0.0;
            return;
        }
        if dismiss_current {
            let mut removed_src: Option<String> = None;
            if let Some(fi) = in_folder {
                let mut now_empty = false;
                let mut new_len = 0;
                if let Some(folder) = self.library.get_mut(fi) {
                    if self.folder_carousel_target < folder.children.len() {
                        removed_src =
                            Some(folder.children.remove(self.folder_carousel_target).source);
                        now_empty = folder.children.is_empty();
                        new_len = folder.children.len();
                    }
                }
                if now_empty {
                    self.active_folder = None;
                } else if new_len > 0 && self.folder_carousel_target >= new_len {
                    self.folder_carousel_target = new_len - 1;
                }
            } else if self.carousel_target > 0 && self.carousel_target - 1 < self.library.len() {
                removed_src = Some(self.library.remove(self.carousel_target - 1).source);
                if self.carousel_target > self.library.len() {
                    self.carousel_target = self.library.len();
                }
            }
            if let Some(src) = removed_src {
                if self.session_source == src {
                    self.destroy_active_session();
                }
            }
            self.dismiss_y = 0.0;
            ctx.request_repaint();
            return;
        }

        let target_f = *c_target as f32;
        *c_pos += (target_f - *c_pos) * (dt * 12.0).min(1.0);
        let cur_target = *c_target;
        let cur_pos = *c_pos;
        let target_h = if self.show_hdrs && in_folder.is_none() && cur_target == 0 {
            1.0
        } else {
            0.0
        };
        self.hdrs_anim += (target_h - self.hdrs_anim) * (dt * 14.0).min(1.0);

        let home_el = self.home_entered_at.elapsed().as_secs_f32();
        let center_refl_fade = smoothstep((home_el / 0.65).clamp(0.0, 1.0));
        let start_over_fade = smoothstep(((home_el - 0.22) / 0.45).clamp(0.0, 1.0));
        if (cur_pos - target_f).abs() > 0.002
            || (self.hdrs_anim - target_h).abs() > 0.002
            || home_el < 0.72
        {
            ctx.request_repaint();
        }

        const BG: Color32 = Color32::from_rgb(14, 14, 15);
        const CARD_DARK: Color32 = Color32::from_rgb(37, 37, 40);
        const CARD_PAPER: Color32 = Color32::from_rgb(232, 232, 227);
        const ACCENT_BLUE: Color32 = Color32::from_rgb(77, 148, 255);
        const TEXT_MAIN: Color32 = Color32::from_rgb(232, 232, 227);

        let mut open_item: Option<(LibraryItem, bool, bool)> = None; // (item, start_over, start_slideshow)
        let mut enter_folder_idx: Option<usize> = None;
        let mut pick_folder_clicked = false;
        let mut new_target: Option<usize> = None;

        // Snapshot visible items for this isolated view (either folder children or root library)
        let items_slice: Vec<LibraryItem> = if let Some(fi) = in_folder {
            self.library
                .get(fi)
                .map(|f| f.children.clone())
                .unwrap_or_default()
        } else {
            self.library.clone()
        };
        let folder_header = in_folder
            .and_then(|fi| self.library.get(fi))
            .map(|f| (f.name.clone(), f.children.len()));

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(BG))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let cx = rect.center().x;
                let cy = rect.center().y - 34.0;
                let scale = (rect.width() / 960.0).clamp(0.85, 1.35);

                // Folder breadcrumb bar when inside an isolated folder!
                if let Some((fname, count)) = &folder_header {
                    let top_r = Rect::from_min_size(
                        Pos2::new(rect.left() + 22.0, rect.top() + 16.0),
                        Vec2::new(rect.width() - 44.0, 32.0),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(top_r), |ui| {
                        ui.horizontal(|ui| {
                            if icon_btn(ui, Vec2::new(28.0, 28.0), draw_back).clicked() {
                                self.active_folder = None;
                                return;
                            }
                            ui.label(
                                egui::RichText::new(format!("{fname}  ·  {count} items"))
                                    .size(14.5)
                                    .color(Color32::WHITE),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("Slideshow")
                                                .size(13.0)
                                                .color(Color32::WHITE),
                                        )
                                        .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 18)),
                                    )
                                    .clicked()
                                {
                                    if let Some(it) = items_slice.get(cur_target).cloned() {
                                        open_item = Some((it, false, true));
                                    }
                                }
                            });
                        });
                    });
                }

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
                    let da = (a as f32 - cur_pos).abs();
                    let db = (b as f32 - cur_pos).abs();
                    db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
                });

                let is_input_card = |idx: usize| in_folder.is_none() && idx == 0;
                let item_for = |idx: usize| -> Option<&LibraryItem> {
                    if in_folder.is_some() {
                        items_slice.get(idx)
                    } else if idx > 0 {
                        items_slice.get(idx - 1)
                    } else {
                        None
                    }
                };

                // 1. Real blurred mirrored floor reflections
                for &idx in &order {
                    let d = idx as f32 - cur_pos;
                    if d.abs() > 2.6 {
                        continue;
                    }
                    let (cr, dim) = card_geom(d);
                    let is_center = idx == cur_target && d.abs() < 0.15;
                    let ref_h = (cr.height() * 0.44).min(104.0 * scale);
                    let rr = Rect::from_min_size(
                        Pos2::new(cr.left(), cr.bottom() + 2.0),
                        Vec2::new(cr.width(), ref_h),
                    );

                    if is_input_card(idx) {
                        let mid_y = rr.top() + ref_h * 0.45;
                        let top_a = ((1.0 - dim) * 58.0) as u8;
                        let mid_a = ((1.0 - dim) * 14.0) as u8;
                        paint_fade_v(
                            ui.painter(),
                            Rect::from_min_max(rr.min, Pos2::new(rr.right(), mid_y)),
                            Color32::from_rgba_unmultiplied(
                                CARD_PAPER.r(),
                                CARD_PAPER.g(),
                                CARD_PAPER.b(),
                                top_a,
                            ),
                            Color32::from_rgba_unmultiplied(
                                CARD_PAPER.r(),
                                CARD_PAPER.g(),
                                CARD_PAPER.b(),
                                mid_a,
                            ),
                        );
                        paint_fade_v(
                            ui.painter(),
                            Rect::from_min_max(Pos2::new(rr.left(), mid_y), rr.max),
                            Color32::from_rgba_unmultiplied(
                                CARD_PAPER.r(),
                                CARD_PAPER.g(),
                                CARD_PAPER.b(),
                                mid_a,
                            ),
                            Color32::TRANSPARENT,
                        );
                    } else if let Some(item) = item_for(idx) {
                        let tex_opt = if is_center {
                            item.resume_thumb.as_ref().or(item.poster_thumb.as_ref())
                        } else {
                            item.poster_thumb.as_ref().or(item.resume_thumb.as_ref())
                        };
                        if let Some(tex) = tex_opt {
                            let fit_cr = fit_aspect_rect(cr, tex.aspect_ratio());
                            let fit_rr = Rect::from_min_size(
                                Pos2::new(fit_cr.left(), cr.bottom() + 2.0),
                                Vec2::new(fit_cr.width(), ref_h),
                            );
                            let tint_a = ((1.0 - dim)
                                * 120.0
                                * if is_center { center_refl_fade } else { 1.0 })
                                as u8;
                            if tint_a > 2 {
                                ui.painter().image(
                                    tex.id(),
                                    fit_rr,
                                    Rect::from_min_max(Pos2::new(0.0, 1.0), Pos2::new(1.0, 0.58)),
                                    Color32::from_rgba_unmultiplied(255, 255, 255, tint_a),
                                );
                                paint_frosted(
                                    ui.painter(),
                                    &self.blur,
                                    fit_rr,
                                    [0.055, 0.055, 0.059, 0.35],
                                    if is_center { center_refl_fade } else { 1.0 },
                                    false,
                                );
                                let mid_y = rr.top() + ref_h * 0.42;
                                paint_fade_v(
                                    ui.painter(),
                                    Rect::from_min_max(rr.min, Pos2::new(rr.right(), mid_y)),
                                    Color32::from_rgba_unmultiplied(14, 14, 15, 115),
                                    Color32::from_rgba_unmultiplied(14, 14, 15, 228),
                                );
                                paint_fade_v(
                                    ui.painter(),
                                    Rect::from_min_max(Pos2::new(rr.left(), mid_y), rr.max),
                                    Color32::from_rgba_unmultiplied(14, 14, 15, 228),
                                    BG,
                                );
                            }
                        } else {
                            let top_a = ((1.0 - dim) * 42.0) as u8;
                            paint_fade_v(
                                ui.painter(),
                                rr,
                                Color32::from_rgba_unmultiplied(
                                    CARD_DARK.r(),
                                    CARD_DARK.g(),
                                    CARD_DARK.b(),
                                    top_a,
                                ),
                                Color32::TRANSPARENT,
                            );
                        }
                    }
                }

                // 2. Render Cover-Flow cards back-to-front
                for &idx in &order {
                    let d = idx as f32 - cur_pos;
                    if d.abs() > 2.6 {
                        continue;
                    }
                    let (mut cr, dim) = card_geom(d);
                    let is_center = idx == cur_target && d.abs() < 0.15;
                    if is_center && !is_input_card(idx) && self.dismiss_y < -0.5 {
                        cr = cr.translate(Vec2::new(0.0, self.dismiss_y));
                    }

                    if is_input_card(idx) {
                        ui.painter().rect_filled(cr, CornerRadius::ZERO, CARD_PAPER);
                        let pad_x = cr.width() * 0.07;
                        let bar_h = (46.0 * (cr.width() / (400.0 * scale))).clamp(28.0, 50.0);
                        let shift_up = if is_center {
                            expo_out(self.hdrs_anim) * (cr.height() * 0.28)
                        } else {
                            0.0
                        };
                        let bar_r = Rect::from_min_size(
                            Pos2::new(cr.left() + pad_x, cr.center().y - bar_h * 0.5 - shift_up),
                            Vec2::new(cr.width() - pad_x * 2.0, bar_h),
                        );
                        ui.painter()
                            .rect_filled(bar_r, CornerRadius::ZERO, Color32::WHITE);

                        let act_r = Rect::from_min_size(
                            Pos2::new(bar_r.right() - bar_h, bar_r.top()),
                            Vec2::new(bar_h, bar_h),
                        );
                        ui.painter().rect_filled(act_r, CornerRadius::ZERO, BG);

                        if !self.url.trim().is_empty() {
                            draw_play(ui.painter(), act_r, TEXT_MAIN);
                        } else {
                            let pc = act_r.center();
                            let arm = bar_h * 0.17;
                            let pst = Stroke::new(2.0_f32, TEXT_MAIN);
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
                                    .font(FontId::proportional(14.0))
                                    .frame(false)
                                    .margin(Margin::symmetric(12, ((bar_h - 19.0) * 0.5) as i8))
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
                                    if is_photo_ext(&p) {
                                        self.open_photo_path(p, false, ctx);
                                    } else {
                                        self.open_local_path(p, true);
                                    }
                                }
                            }

                            let opt_area = Rect::from_min_max(
                                Pos2::new(bar_r.left(), bar_r.bottom() + 8.0),
                                Pos2::new(bar_r.right(), cr.bottom() - 8.0),
                            );
                            ui.scope_builder(egui::UiBuilder::new().max_rect(opt_area), |ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(0.0, 4.0);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(14.0, 0.0);
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(if self.show_hdrs {
                                                    "Hide options"
                                                } else {
                                                    "More options"
                                                })
                                                .size(12.5)
                                                .strong()
                                                .color(Color32::from_rgb(55, 55, 52)),
                                            )
                                            .fill(Color32::TRANSPARENT),
                                        )
                                        .clicked()
                                    {
                                        self.show_hdrs = !self.show_hdrs;
                                    }
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new("Open Folder")
                                                    .size(12.5)
                                                    .strong()
                                                    .color(Color32::from_rgb(55, 55, 52)),
                                            )
                                            .fill(Color32::TRANSPARENT),
                                        )
                                        .clicked()
                                    {
                                        pick_folder_clicked = true;
                                    }
                                    if self.opening {
                                        ui.label(
                                            egui::RichText::new("Opening…")
                                                .size(12.5)
                                                .strong()
                                                .color(ACCENT_BLUE),
                                        );
                                    }
                                });

                                if self.hdrs_anim > 0.05 {
                                    let a = expo_out(self.hdrs_anim);
                                    ui.scope(|ui| {
                                        ui.set_opacity(a);
                                        ui.visuals_mut().override_text_color =
                                            Some(Color32::from_rgb(20, 20, 22));
                                        let fw = opt_area.width();
                                        for (val, hint) in [
                                            (&mut self.referer, "Referer"),
                                            (&mut self.cookie, "Cookie"),
                                            (&mut self.user_agent, "User-Agent"),
                                        ] {
                                            let r = Rect::from_min_size(
                                                ui.cursor().min,
                                                Vec2::new(fw, 26.0),
                                            );
                                            ui.painter().rect_filled(
                                                r,
                                                CornerRadius::ZERO,
                                                Color32::from_rgba_unmultiplied(255, 255, 255, 215),
                                            );
                                            ui.add_sized(
                                                Vec2::new(fw, 26.0),
                                                egui::TextEdit::singleline(val)
                                                    .hint_text(hint)
                                                    .font(FontId::proportional(12.0))
                                                    .frame(false)
                                                    .margin(Margin::symmetric(8, 4)),
                                            );
                                        }
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        egui::RichText::new(
                                                            "Attach .part / .crdownload",
                                                        )
                                                        .size(11.5)
                                                        .strong()
                                                        .color(Color32::from_rgb(35, 35, 38)),
                                                    )
                                                    .fill(Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 190,
                                                    )),
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
                                                        .color(Color32::from_rgb(60, 60, 58)),
                                                );
                                                if ui
                                                    .add(
                                                        egui::Button::new("x")
                                                            .fill(Color32::TRANSPARENT),
                                                    )
                                                    .clicked()
                                                {
                                                    self.seed_file = None;
                                                }
                                            }
                                        });
                                    });
                                }
                            });
                        } else {
                            ui.painter().text(
                                Pos2::new(bar_r.left() + 12.0, bar_r.center().y),
                                Align2::LEFT_CENTER,
                                "Paste a link or open a file",
                                FontId::proportional(
                                    (13.0 * (cr.width() / (400.0 * scale))).max(10.0),
                                ),
                                Color32::from_rgb(85, 85, 79),
                            );
                            if ui
                                .interact(cr, egui::Id::new("cf_card_0"), Sense::click())
                                .clicked()
                            {
                                new_target = Some(0);
                            }
                        }
                    } else if let Some(item) = item_for(idx) {
                        // Subtle stacked edge behind Folder cards so folders are immediately distinct!
                        if item.kind == MediaKind::Folder {
                            let stack_r = Rect::from_min_size(
                                Pos2::new(cr.left() + 8.0, cr.top() - 6.0),
                                Vec2::new(cr.width() - 16.0, 6.0),
                            );
                            ui.painter().rect_filled(
                                stack_r,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                            );
                        }

                        ui.painter()
                            .rect_filled(cr, CornerRadius::ZERO, Color32::BLACK);
                        let chosen_thumb = if is_center {
                            item.resume_thumb.as_ref().or(item.poster_thumb.as_ref())
                        } else {
                            item.poster_thumb.as_ref().or(item.resume_thumb.as_ref())
                        };

                        if let Some(tex) = chosen_thumb {
                            let img_rect = fit_aspect_rect(cr, tex.aspect_ratio());
                            ui.painter().image(
                                tex.id(),
                                img_rect,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                            paint_fade_v(
                                ui.painter(),
                                Rect::from_min_size(
                                    Pos2::new(cr.left(), cr.bottom() - cr.height() * 0.42),
                                    Vec2::new(cr.width(), cr.height() * 0.42),
                                ),
                                Color32::TRANSPARENT,
                                Color32::from_rgba_unmultiplied(0, 0, 0, 215),
                            );
                        } else {
                            ui.painter().rect_filled(cr, CornerRadius::ZERO, CARD_DARK);
                        }

                        if is_center && item.kind == MediaKind::Video && start_over_fade > 0.02 {
                            draw_play(
                                ui.painter(),
                                cr,
                                Color32::from_rgba_unmultiplied(
                                    255,
                                    255,
                                    255,
                                    (230.0 * start_over_fade) as u8,
                                ),
                            );
                        }

                        let fs = (13.5 * (cr.width() / (400.0 * scale))).clamp(10.5, 14.5);
                        let status_str = match item.kind {
                            MediaKind::Folder => format!("{} items", item.children.len()),
                            MediaKind::Photo => "PHOTO".into(),
                            MediaKind::Audio => {
                                if item.duration > 0.0 {
                                    format!(
                                        "{} / {}",
                                        fmt_time_precise(item.position, item.duration),
                                        fmt_time_precise(item.duration, item.duration)
                                    )
                                } else {
                                    "AUDIO".into()
                                }
                            }
                            MediaKind::Video => {
                                if item.duration > 0.0 {
                                    format!(
                                        "{} / {}",
                                        fmt_time_precise(item.position, item.duration),
                                        fmt_time_precise(item.duration, item.duration)
                                    )
                                } else {
                                    "NEW".into()
                                }
                            }
                        };
                        let short_name: String = item.name.chars().take(28).collect();
                        ui.painter().text(
                            Pos2::new(cr.left() + 12.0, cr.bottom() - 16.0),
                            Align2::LEFT_CENTER,
                            short_name,
                            FontId::proportional(fs),
                            Color32::WHITE,
                        );
                        ui.painter().text(
                            Pos2::new(cr.right() - 12.0, cr.bottom() - 16.0),
                            Align2::RIGHT_CENTER,
                            status_str,
                            FontId::proportional(fs - 1.0),
                            Color32::from_rgba_unmultiplied(255, 255, 255, 215),
                        );

                        if item.kind == MediaKind::Video {
                            let pg_r = Rect::from_min_size(
                                Pos2::new(cr.left(), cr.bottom() - 2.5),
                                Vec2::new(cr.width(), 2.5),
                            );
                            ui.painter().rect_filled(
                                pg_r,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 48),
                            );
                            if item.duration > 0.0 {
                                let pct = (item.position / item.duration).clamp(0.0, 1.0) as f32;
                                ui.painter().rect_filled(
                                    Rect::from_min_size(
                                        pg_r.min,
                                        Vec2::new(pg_r.width() * pct, 2.5),
                                    ),
                                    CornerRadius::ZERO,
                                    Color32::WHITE,
                                );
                            }
                        }

                        let card_resp = ui.interact(
                            cr,
                            egui::Id::new(("cf_card", in_folder, idx)),
                            Sense::click(),
                        );
                        let mut clicked_start_over = false;
                        if is_center
                            && item.kind == MediaKind::Video
                            && item.position > 0.5
                            && start_over_fade > 0.02
                        {
                            let so_r = Rect::from_min_size(
                                Pos2::new(cr.right() - 88.0, cr.top() + 10.0),
                                Vec2::new(76.0, 26.0),
                            );
                            let so_resp = ui
                                .interact(
                                    so_r,
                                    egui::Id::new(("cf_start_over", idx)),
                                    Sense::click(),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            let txt_a = ((if so_resp.hovered() { 255.0 } else { 150.0 })
                                * start_over_fade) as u8;
                            ui.painter().text(
                                so_r.center() + Vec2::new(0.0, 1.0),
                                Align2::CENTER_CENTER,
                                "Start Over",
                                FontId::proportional(13.0),
                                Color32::from_rgba_unmultiplied(
                                    0,
                                    0,
                                    0,
                                    (160.0 * start_over_fade) as u8,
                                ),
                            );
                            ui.painter().text(
                                so_r.center(),
                                Align2::CENTER_CENTER,
                                "Start Over",
                                FontId::proportional(13.0),
                                Color32::from_rgba_unmultiplied(255, 255, 255, txt_a),
                            );
                            if so_resp.clicked() {
                                clicked_start_over = true;
                                open_item = Some((item.clone(), true, false));
                            }
                        }

                        if !clicked_start_over && card_resp.clicked() {
                            if is_center {
                                if item.kind == MediaKind::Folder {
                                    enter_folder_idx = Some(idx - 1);
                                } else {
                                    open_item = Some((item.clone(), false, false));
                                }
                            } else {
                                new_target = Some(idx);
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

                if !ctx.wants_keyboard_input() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                    if let Some(item) = item_for(cur_target) {
                        if item.kind == MediaKind::Folder {
                            enter_folder_idx = Some(cur_target - 1);
                        } else {
                            open_item = Some((item.clone(), false, false));
                        }
                    }
                }

                if self.mpv_err.is_some() || !self.err_msg.is_empty() {
                    let foot_w = 400.0 * scale;
                    let foot_y = cy + (400.0 * scale * 9.0 / 16.0) * 0.5 + 18.0;
                    let foot_r = Rect::from_min_size(
                        Pos2::new(cx - foot_w * 0.5, foot_y),
                        Vec2::new(foot_w, 80.0),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(foot_r), |ui| {
                        if let Some(e) = &self.mpv_err {
                            ui.label(
                                egui::RichText::new(e)
                                    .size(12.5)
                                    .color(Color32::from_gray(200)),
                            );
                        }
                        if !self.err_msg.is_empty() {
                            ui.label(
                                egui::RichText::new(&self.err_msg)
                                    .size(12.5)
                                    .color(Color32::from_gray(225)),
                            );
                        }
                    });
                }
            });

        if let Some(nt) = new_target {
            if in_folder.is_some() {
                self.folder_carousel_target = nt;
            } else {
                self.carousel_target = nt;
            }
        }
        if pick_folder_clicked {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                self.add_folder(dir, ctx);
            }
        }
        if let Some(fi) = enter_folder_idx {
            self.active_folder = Some(fi);
            self.folder_carousel_target = 0;
            self.folder_carousel_pos = 0.0;
        }
        if let Some((item, start_over, start_slideshow)) = open_item {
            match item.kind {
                MediaKind::Photo => {
                    self.open_photo_path(PathBuf::from(item.source), start_slideshow, ctx);
                }
                MediaKind::Video | MediaKind::Audio => {
                    if self.session.is_some() && self.session_source == item.source {
                        self.resume_warm_session(start_over);
                    } else {
                        self.pending_resume_pos = if !start_over && item.position > 0.5 {
                            Some(item.position)
                        } else {
                            None
                        };
                        if item.is_local {
                            self.open_local_path(PathBuf::from(item.source), in_folder.is_none());
                        } else {
                            self.url = item.source;
                            self.start_open_url();
                        }
                    }
                }
                MediaKind::Folder => {}
            }
        }
    }

    fn draw_player(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.05);

        if let Some(mpv) = &self.mpv {
            self.snap = mpv.poll_state();
            if !self.snap.media_title.is_empty() && self.title != self.snap.media_title {
                self.title = self.snap.media_title.clone();
            }
            if let Some(pos) = self.pending_resume_pos {
                if self.snap.duration > 0.0 {
                    if (self.snap.position - pos).abs() > 0.25 {
                        mpv.seek_absolute(pos);
                    }
                    self.pending_resume_pos = None;
                }
            }
            // Auto-extract embedded album art & auto-search synced lyrics for audio sessions!
            if self.is_audio_session && self.snap.duration > 0.0 && !self.lyrics_auto_fetched {
                self.lyrics_auto_fetched = true;
                if let Some(s) = self.session.clone() {
                    if self.find_item_thumb(&self.session_source).is_none() {
                        let art_path = if s.is_local {
                            s.media_url.clone()
                        } else {
                            s.media_url.clone()
                        };
                        self.spawn_audio_art(self.session_source.clone(), art_path, ctx);
                    }
                }
                if self.lyrics.is_empty() {
                    let q = if !self.snap.media_artist.is_empty() {
                        format!("{} {}", self.title, self.snap.media_artist)
                    } else {
                        self.title.clone()
                    };
                    self.lyrics_query = q.clone();
                    self.start_lyrics_search(q);
                }
            }
            // Automatically capture the 960px main poster frame (at ~15% into video) for side-peek cards!
            if !self.is_audio_session && self.snap.duration > 5.0 {
                if let Some(s) = self.session.clone() {
                    let src = if s.is_local {
                        s.media_url.clone()
                    } else {
                        self.url.trim().to_string()
                    };
                    if self.poster_requested_for.as_deref() != Some(src.as_str()) {
                        self.poster_requested_for = Some(src.clone());
                        let needs_poster = self
                            .library
                            .iter()
                            .find(|x| x.source == src)
                            .map_or(false, |x| x.poster_thumb.is_none());
                        if needs_poster {
                            let poster_sec =
                                ((self.snap.duration * 0.15).clamp(3.0, 120.0)).round() as u64;
                            let lp = s.is_local.then(|| s.media_url.clone());
                            self.spawn_card_thumb(src, s.id.clone(), lp, poster_sec, true, ctx);
                        }
                    }
                }
            }
        }

        // Safety fallback: if screenshot takes > 250ms, start closing morph anyway so UI never hangs
        if let Some(since) = self.pending_close_since {
            if since.elapsed() >= Duration::from_millis(250) {
                self.pending_close_since = None;
                self.closing_morph = true;
                self.morph_start = Instant::now();
            } else {
                ctx.request_repaint();
            }
        }

        // Lush 520ms cubic ease-in-out Card <-> Fullscreen morph (clearly readable Apple TV feel!)
        let morph_p = (self.morph_start.elapsed().as_secs_f32() / 0.52).clamp(0.0, 1.0);
        self.expand_t = if self.closing_morph {
            1.0 - ease_in_out_cubic(morph_p)
        } else {
            ease_in_out_cubic(morph_p)
        };
        if morph_p < 1.0 {
            ctx.request_repaint();
        }
        if self.closing_morph && morph_p >= 1.0 {
            self.finish_close_to_home();
            self.draw_home(ctx);
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

        if let Some(rx) = &self.lyrics_rx {
            if let Ok(res) = rx.try_recv() {
                self.lyrics_rx = None;
                match res {
                    Ok(list) => {
                        if let Some(first) = list.first() {
                            if self.lyrics.is_empty() {
                                if let Some(txt) =
                                    first.synced.as_deref().or(first.plain.as_deref())
                                {
                                    self.lyrics = parse_lrc(txt);
                                }
                            }
                        }
                        self.lyrics_msg = if list.is_empty() {
                            "No lyrics found".into()
                        } else {
                            String::new()
                        };
                        self.lyrics_results = list;
                    }
                    Err(e) => self.lyrics_msg = format!("Lyrics search failed: {e}"),
                }
            }
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
            .map_or(false, |t| t.elapsed() > Duration::from_millis(220));
        let pause_target = if want_pause_overlay { 1.0 } else { 0.0 };
        // Silky ~650ms gradual ease so pause blur melts in smoothly instead of snapping
        let pause_rate = if pause_target > self.pause_t {
            1.7
        } else {
            3.2
        };
        if self.pause_t < pause_target {
            self.pause_t = (self.pause_t + dt * pause_rate).min(1.0);
        } else if self.pause_t > pause_target {
            self.pause_t = (self.pause_t - dt * pause_rate).max(0.0);
        }

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

        let exp = self.expand_t;
        let bg_v = (14.0 * (1.0 - exp)) as u8;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(bg_v, bg_v, bg_v)))
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();

                // Aspect-locked Card <-> Fullscreen morph + receding/returning Cover Flow side cards!
                let scale = (full_rect.width() / 960.0).clamp(0.85, 1.35);
                let card_w = 400.0 * scale;
                let card_h = card_w * 9.0 / 16.0;
                let cx = full_rect.center().x;
                let cy = full_rect.center().y - 34.0;
                let card_box = Rect::from_center_size(Pos2::new(cx, cy), Vec2::new(card_w, card_h));

                // Draw the EXACT Cover Flow side cards (with full Input bar, titles, progress bars & floor reflections!)
                // fading smoothly behind the morphing center card so there is ZERO pop when switching to draw_home!
                let side_vis = (1.0 - exp).clamp(0.0, 1.0);
                if side_vis > 0.01 {
                    const BG_COL: Color32 = Color32::from_rgb(14, 14, 15);
                    const PAPER: Color32 = Color32::from_rgb(232, 232, 227);
                    const DARK: Color32 = Color32::from_rgb(37, 37, 40);
                    let total_cards = 1 + self.library.len();
                    let mut s_order: Vec<usize> = (0..total_cards)
                        .filter(|&i| i != self.carousel_target)
                        .collect();
                    s_order.sort_by(|&a, &b| {
                        let da = (a as f32 - self.carousel_pos).abs();
                        let db = (b as f32 - self.carousel_pos).abs();
                        db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
                    });

                    for idx in s_order {
                        let d = idx as f32 - self.carousel_pos;
                        if d.abs() > 2.6 {
                            continue;
                        }
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
                        let sr = Rect::from_center_size(Pos2::new(cx + x_off, cy), Vec2::new(w, h));
                        let vis_a = (side_vis * 255.0) as u8;

                        // Soft floor reflection matching draw_home identically
                        let ref_h = (sr.height() * 0.44).min(104.0 * scale);
                        let rr = Rect::from_min_size(
                            Pos2::new(sr.left(), sr.bottom() + 2.0),
                            Vec2::new(sr.width(), ref_h),
                        );
                        if idx == 0 {
                            let mid_y = rr.top() + ref_h * 0.45;
                            let top_a = ((1.0 - dim) * 58.0 * side_vis) as u8;
                            let mid_a = ((1.0 - dim) * 14.0 * side_vis) as u8;
                            paint_fade_v(
                                ui.painter(),
                                Rect::from_min_max(rr.min, Pos2::new(rr.right(), mid_y)),
                                Color32::from_rgba_unmultiplied(
                                    PAPER.r(),
                                    PAPER.g(),
                                    PAPER.b(),
                                    top_a,
                                ),
                                Color32::from_rgba_unmultiplied(
                                    PAPER.r(),
                                    PAPER.g(),
                                    PAPER.b(),
                                    mid_a,
                                ),
                            );
                            paint_fade_v(
                                ui.painter(),
                                Rect::from_min_max(Pos2::new(rr.left(), mid_y), rr.max),
                                Color32::from_rgba_unmultiplied(
                                    PAPER.r(),
                                    PAPER.g(),
                                    PAPER.b(),
                                    mid_a,
                                ),
                                Color32::TRANSPARENT,
                            );

                            // Full Card 0 with white input bar & dark '+'/'▶' button!
                            ui.painter().rect_filled(
                                sr,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(
                                    PAPER.r(),
                                    PAPER.g(),
                                    PAPER.b(),
                                    vis_a,
                                ),
                            );
                            let pad_x = sr.width() * 0.07;
                            let bar_h = (46.0 * (sr.width() / (400.0 * scale))).clamp(28.0, 50.0);
                            let bar_r = Rect::from_min_size(
                                Pos2::new(sr.left() + pad_x, sr.center().y - bar_h * 0.5),
                                Vec2::new(sr.width() - pad_x * 2.0, bar_h),
                            );
                            ui.painter().rect_filled(
                                bar_r,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(255, 255, 255, vis_a),
                            );
                            let act_r = Rect::from_min_size(
                                Pos2::new(bar_r.right() - bar_h, bar_r.top()),
                                Vec2::new(bar_h, bar_h),
                            );
                            ui.painter().rect_filled(
                                act_r,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(
                                    BG_COL.r(),
                                    BG_COL.g(),
                                    BG_COL.b(),
                                    vis_a,
                                ),
                            );
                            ui.painter().text(
                                Pos2::new(bar_r.left() + 12.0, bar_r.center().y),
                                Align2::LEFT_CENTER,
                                "Paste a link or open a file",
                                FontId::proportional(
                                    (13.0 * (sr.width() / (400.0 * scale))).max(10.0),
                                ),
                                Color32::from_rgba_unmultiplied(85, 85, 79, vis_a),
                            );
                        } else {
                            let item = &self.library[idx - 1];
                            let tex_opt = item.poster_thumb.as_ref().or(item.resume_thumb.as_ref());
                            if let Some(tex) = tex_opt {
                                let fit_sr = fit_aspect_rect(sr, tex.aspect_ratio());
                                let fit_rr = Rect::from_min_size(
                                    Pos2::new(fit_sr.left(), sr.bottom() + 2.0),
                                    Vec2::new(fit_sr.width(), ref_h),
                                );
                                let tint_a = ((1.0 - dim) * 120.0 * side_vis) as u8;
                                ui.painter().image(
                                    tex.id(),
                                    fit_rr,
                                    Rect::from_min_max(Pos2::new(0.0, 1.0), Pos2::new(1.0, 0.58)),
                                    Color32::from_rgba_unmultiplied(255, 255, 255, tint_a),
                                );
                                let mid_y = rr.top() + ref_h * 0.42;
                                paint_fade_v(
                                    ui.painter(),
                                    Rect::from_min_max(rr.min, Pos2::new(rr.right(), mid_y)),
                                    Color32::from_rgba_unmultiplied(14, 14, 15, 115),
                                    Color32::from_rgba_unmultiplied(14, 14, 15, 228),
                                );
                                paint_fade_v(
                                    ui.painter(),
                                    Rect::from_min_max(Pos2::new(rr.left(), mid_y), rr.max),
                                    Color32::from_rgba_unmultiplied(14, 14, 15, 228),
                                    BG_COL,
                                );
                                ui.painter().rect_filled(
                                    sr,
                                    CornerRadius::ZERO,
                                    Color32::from_rgba_unmultiplied(0, 0, 0, vis_a),
                                );
                                ui.painter().image(
                                    tex.id(),
                                    fit_sr,
                                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                    Color32::from_rgba_unmultiplied(255, 255, 255, vis_a),
                                );
                            } else {
                                ui.painter().rect_filled(
                                    sr,
                                    CornerRadius::ZERO,
                                    Color32::from_rgba_unmultiplied(
                                        DARK.r(),
                                        DARK.g(),
                                        DARK.b(),
                                        vis_a,
                                    ),
                                );
                            }
                            let fs = (13.5 * (sr.width() / (400.0 * scale))).clamp(10.5, 14.5);
                            let short_name: String = item.name.chars().take(28).collect();
                            ui.painter().text(
                                Pos2::new(sr.left() + 12.0, sr.bottom() - 16.0),
                                Align2::LEFT_CENTER,
                                short_name,
                                FontId::proportional(fs),
                                Color32::from_rgba_unmultiplied(255, 255, 255, vis_a),
                            );
                        }

                        // Distance dim overlay matching draw_home identically
                        let dim_a = ((dim * side_vis + (1.0 - side_vis)) * 255.0).min(255.0) as u8;
                        ui.painter().rect_filled(
                            sr,
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(14, 14, 15, dim_a),
                        );
                    }
                }

                let aspect = if let Some(tex) = &self.morph_tex {
                    tex.aspect_ratio()
                } else if self.snap.width > 0 && self.snap.height > 0 {
                    self.snap.width as f32 / self.snap.height as f32
                } else {
                    16.0 / 9.0
                };
                let start_rect = fit_aspect_rect(card_box, aspect);
                let end_rect = fit_aspect_rect(full_rect, aspect);
                let video_rect = Rect::from_min_max(
                    Pos2::new(
                        start_rect.left() + (end_rect.left() - start_rect.left()) * exp,
                        start_rect.top() + (end_rect.top() - start_rect.top()) * exp,
                    ),
                    Pos2::new(
                        start_rect.right() + (end_rect.right() - start_rect.right()) * exp,
                        start_rect.bottom() + (end_rect.bottom() - start_rect.bottom()) * exp,
                    ),
                );

                // 1. Live GPU mpv ONLY once morph is done AND mpv actually has media loaded & seeked
                // (never flashes black or 0:00 / 0:00 while waiting for first frame!)
                let mpv_ready = !self.closing_morph
                    && exp >= 0.99
                    && self.snap.duration > 0.0
                    && self.pending_resume_pos.is_none();
                if mpv_ready {
                    if self.is_audio_session {
                        // Apple Music-style Audio Stage: Frosted album art backdrop + Cover + Live Synced Lyrics!
                        let art_tex = self.find_item_thumb(&self.session_source);
                        if let Some(tex) = &art_tex {
                            let bg_r = fit_aspect_rect(full_rect, tex.aspect_ratio());
                            let cover_scale = (full_rect.width() / bg_r.width())
                                .max(full_rect.height() / bg_r.height())
                                .max(1.0);
                            let full_bg = Rect::from_center_size(
                                full_rect.center(),
                                bg_r.size() * cover_scale,
                            );
                            ui.painter().image(
                                tex.id(),
                                full_bg,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                            paint_frosted(
                                ui.painter(),
                                &self.blur,
                                full_rect,
                                [0.04, 0.04, 0.05, 0.62],
                                1.0,
                                false,
                            );
                        } else {
                            ui.painter().rect_filled(
                                full_rect,
                                CornerRadius::ZERO,
                                Color32::from_rgb(14, 14, 16),
                            );
                        }

                        // Left: Square Album Art + Track Title + Artist / Album
                        let has_lyrics = !self.lyrics.is_empty();
                        let art_size = (full_rect.height() * 0.42).clamp(180.0, 340.0);
                        let art_cx = if has_lyrics {
                            full_rect.left() + full_rect.width() * 0.28
                        } else {
                            full_rect.center().x
                        };
                        let art_r = Rect::from_center_size(
                            Pos2::new(art_cx, full_rect.center().y - 34.0),
                            Vec2::splat(art_size),
                        );
                        if let Some(tex) = &art_tex {
                            ui.painter().image(
                                tex.id(),
                                art_r,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        } else {
                            ui.painter().rect_filled(
                                art_r,
                                CornerRadius::ZERO,
                                Color32::from_rgb(30, 30, 34),
                            );
                            // Minimal geometric vinyl disc mark
                            ui.painter().circle_stroke(
                                art_r.center(),
                                art_size * 0.28,
                                Stroke::new(
                                    1.5_f32,
                                    Color32::from_rgba_unmultiplied(255, 255, 255, 65),
                                ),
                            );
                            ui.painter().circle_filled(
                                art_r.center(),
                                art_size * 0.06,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 110),
                            );
                        }
                        let title_y = art_r.bottom() + 22.0;
                        ui.painter().text(
                            Pos2::new(art_cx, title_y),
                            Align2::CENTER_CENTER,
                            &self.title,
                            FontId::proportional(20.0),
                            Color32::WHITE,
                        );
                        if !self.snap.media_artist.is_empty() {
                            let sub_line = if !self.snap.media_album.is_empty() {
                                format!("{} · {}", self.snap.media_artist, self.snap.media_album)
                            } else {
                                self.snap.media_artist.clone()
                            };
                            ui.painter().text(
                                Pos2::new(art_cx, title_y + 24.0),
                                Align2::CENTER_CENTER,
                                sub_line,
                                FontId::proportional(13.5),
                                Color32::from_rgba_unmultiplied(255, 255, 255, 155),
                            );
                        }

                        // Right: Live Synced Lyrics (click any line to seek!)
                        if has_lyrics {
                            let lyr_rect = Rect::from_min_max(
                                Pos2::new(
                                    full_rect.left() + full_rect.width() * 0.50,
                                    full_rect.top() + 84.0,
                                ),
                                Pos2::new(full_rect.right() - 48.0, full_rect.bottom() - 104.0),
                            );
                            let t_now = self.snap.position - self.lyrics_delay;
                            let mut active_idx: Option<usize> = None;
                            for (i, line) in self.lyrics.iter().enumerate() {
                                if let Some(lt) = line.time {
                                    if lt <= t_now {
                                        active_idx = Some(i);
                                    }
                                }
                            }
                            let mpv_seek = self.mpv.clone();
                            let lines_clone = self.lyrics.clone();
                            ui.scope_builder(egui::UiBuilder::new().max_rect(lyr_rect), |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("live_lyrics_scroll")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = Vec2::new(0.0, 14.0);
                                        for (i, line) in lines_clone.iter().enumerate() {
                                            let is_active = active_idx == Some(i);
                                            let col = if is_active {
                                                Color32::WHITE
                                            } else if active_idx.map_or(false, |a| i < a) {
                                                Color32::from_rgba_unmultiplied(255, 255, 255, 85)
                                            } else {
                                                Color32::from_rgba_unmultiplied(255, 255, 255, 130)
                                            };
                                            let sz = if is_active { 24.0 } else { 19.0 };
                                            let resp = ui.add(
                                                egui::Button::new(
                                                    egui::RichText::new(&line.text)
                                                        .size(sz)
                                                        .color(col),
                                                )
                                                .wrap()
                                                .fill(Color32::TRANSPARENT),
                                            );
                                            if is_active {
                                                resp.scroll_to_me(Some(Align::Center));
                                            }
                                            if resp.clicked() {
                                                if let Some(lt) = line.time {
                                                    if let Some(m) = &mpv_seek {
                                                        m.seek_absolute(lt + self.lyrics_delay);
                                                    }
                                                }
                                            }
                                        }
                                    });
                            });
                        }
                    } else if let Some(mpv) = self.mpv.clone() {
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
                } else {
                    // Clean shrinking card during close morph (center reflection & Start Over fade in
                    // smoothly right after the card lands in draw_home — zero sudden shadow pop!)
                    let outer_rect = Rect::from_min_max(
                        Pos2::new(
                            card_box.left() + (full_rect.left() - card_box.left()) * exp,
                            card_box.top() + (full_rect.top() - card_box.top()) * exp,
                        ),
                        Pos2::new(
                            card_box.right() + (full_rect.right() - card_box.right()) * exp,
                            card_box.bottom() + (full_rect.bottom() - card_box.bottom()) * exp,
                        ),
                    );
                    let card_chrome_a = (1.0 - exp).powi(2).clamp(0.0, 1.0);

                    ui.painter()
                        .rect_filled(outer_rect, CornerRadius::ZERO, Color32::BLACK);
                    if let Some(tex) = &self.morph_tex {
                        ui.painter().image(
                            tex.id(),
                            video_rect,
                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }

                    if card_chrome_a > 0.02 {
                        let ca = (255.0 * card_chrome_a) as u8;
                        let fs = (13.5 * (outer_rect.width() / (400.0 * scale))).clamp(10.5, 14.5);
                        let short_name: String = self.title.chars().take(28).collect();
                        ui.painter().text(
                            Pos2::new(outer_rect.left() + 12.0, outer_rect.bottom() - 16.0),
                            Align2::LEFT_CENTER,
                            short_name,
                            FontId::proportional(fs),
                            Color32::from_rgba_unmultiplied(255, 255, 255, ca),
                        );
                        if self.snap.duration > 0.0 {
                            ui.painter().text(
                                Pos2::new(outer_rect.right() - 12.0, outer_rect.bottom() - 16.0),
                                Align2::RIGHT_CENTER,
                                format!(
                                    "{} / {}",
                                    fmt_time_precise(self.snap.position, self.snap.duration),
                                    fmt_time_precise(self.snap.duration, self.snap.duration)
                                ),
                                FontId::proportional(fs - 1.0),
                                Color32::from_rgba_unmultiplied(
                                    255,
                                    255,
                                    255,
                                    (215.0 * card_chrome_a) as u8,
                                ),
                            );
                            let pg_r = Rect::from_min_size(
                                Pos2::new(outer_rect.left(), outer_rect.bottom() - 2.5),
                                Vec2::new(outer_rect.width(), 2.5),
                            );
                            ui.painter().rect_filled(
                                pg_r,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(
                                    255,
                                    255,
                                    255,
                                    (48.0 * card_chrome_a) as u8,
                                ),
                            );
                            let pct =
                                (self.snap.position / self.snap.duration).clamp(0.0, 1.0) as f32;
                            ui.painter().rect_filled(
                                Rect::from_min_size(pg_r.min, Vec2::new(pg_r.width() * pct, 2.5)),
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(255, 255, 255, ca),
                            );
                        }
                    }
                }

                // Skip all player chrome, pause blur, HUD, and overlays until mpv is actually ready
                // (also hide chrome while waiting ~20ms for exact close-frame capture!)
                if !mpv_ready || self.pending_close_since.is_some() {
                    return;
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

                // 2. Silky Smooth Gradual GPU Frosted Blur Pause Overlay
                let pe = smoothstep(self.pause_t);
                if pe > 0.005 {
                    let scrim = Rect::from_min_size(
                        full_rect.min,
                        Vec2::new(full_rect.width() * 0.62, full_rect.height()),
                    );
                    paint_frosted(p, &self.blur, scrim, [0.03, 0.03, 0.04, 0.48], pe, true);

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
                    // Constrain pause title width cleanly so long/UUID filenames never cut across the video
                    let display_title: String = if self.title.chars().count() > 28 {
                        format!("{}…", self.title.chars().take(27).collect::<String>())
                    } else {
                        self.title.clone()
                    };
                    let title_size = if display_title.len() > 20 { 30.0 } else { 40.0 };
                    p.text(
                        Pos2::new(lx, cy + 12.0),
                        Align2::LEFT_CENTER,
                        display_title,
                        FontId::proportional(title_size),
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

                    // Top bar (minimal: just Back + Title, no noisy network stats!)
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
                            ui.label(egui::RichText::new(&self.title).size(14.5));
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

                    // Show subtle downloaded portion on rail + network-speed-reactive buffering pulse!
                    if self.status.percent > 0.0 && self.status.percent < 100.0 {
                        let dl_w =
                            rail_rect.width() * (self.status.percent / 100.0).clamp(0.0, 1.0);
                        sp.rect_filled(
                            Rect::from_min_size(rail_rect.min, Vec2::new(dl_w, rail_h)),
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(255, 255, 255, (72.0 * ce) as u8),
                        );
                    }
                    if self.snap.buffering {
                        // Pulse speed scales smoothly with live download throughput (0.35 Hz slow -> 2.4 Hz fast!)
                        let mbps = (self.status.speed_bps as f32) / (1024.0 * 1024.0);
                        let rate = (0.35 + mbps * 0.25).clamp(0.35, 2.4);
                        let t = (ctx.input(|i| i.time) as f32 * rate).fract();
                        let bw = rail_rect.width() * 0.25;
                        let bx = rail_rect.left() + (rail_rect.width() + bw) * t - bw;
                        let pulse_r = Rect::from_min_max(
                            Pos2::new(bx.max(rail_rect.left()), rail_rect.top()),
                            Pos2::new((bx + bw).min(rail_rect.right()), rail_rect.bottom()),
                        );
                        sp.rect_filled(
                            pulse_r,
                            CornerRadius::ZERO,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 135),
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
                                let ha = smoothstep(self.hover_t);
                                // Dynamic aspect-accurate hover preview (portrait stays portrait, landscape stays landscape!)
                                let preview_aspect = if let Some((_, tex)) = &self.active_thumb {
                                    tex.aspect_ratio()
                                } else if self.snap.width > 0 && self.snap.height > 0 {
                                    self.snap.width as f32 / self.snap.height as f32
                                } else {
                                    16.0 / 9.0
                                };
                                let (img_w, img_h) = if preview_aspect >= 1.0 {
                                    let w = 184.0_f32;
                                    (w, (w / preview_aspect).clamp(64.0, 150.0))
                                } else {
                                    let h = 156.0_f32;
                                    ((h * preview_aspect).clamp(64.0, 156.0), h)
                                };
                                let pw = img_w + 6.0;
                                let ph = img_h + 6.0;
                                let half_w = pw * 0.5;
                                let px = self
                                    .hover_x_smooth
                                    .clamp(scrub_rect.left() + half_w, scrub_rect.right() - half_w);
                                let box_r = Rect::from_min_size(
                                    Pos2::new(px - half_w, scrub_y - ph - 32.0),
                                    Vec2::new(pw, ph + 20.0),
                                );
                                sp.rect_filled(
                                    box_r,
                                    CornerRadius::ZERO,
                                    Color32::from_rgba_unmultiplied(0, 0, 0, (195.0 * ha) as u8),
                                );
                                let img_r = Rect::from_min_size(
                                    box_r.min + Vec2::new(3.0, 3.0),
                                    Vec2::new(img_w, img_h),
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

                            let has_prev = self.queue.len() > 1 && self.queue_idx > 0;
                            let prev_resp = icon_btn(ui, Vec2::new(26.0, 28.0), |p, r, c| {
                                let col = if has_prev {
                                    c
                                } else {
                                    Color32::from_rgba_unmultiplied(255, 255, 255, 60)
                                };
                                draw_prev(p, r, col);
                            });
                            if has_prev && prev_resp.clicked() {
                                self.play_queue_offset(-1);
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

                            let has_next =
                                self.queue.len() > 1 && self.queue_idx + 1 < self.queue.len();
                            let next_resp = icon_btn(ui, Vec2::new(26.0, 28.0), |p, r, c| {
                                let col = if has_next {
                                    c
                                } else {
                                    Color32::from_rgba_unmultiplied(255, 255, 255, 60)
                                };
                                draw_next(p, r, col);
                            });
                            if has_next && next_resp.clicked() {
                                self.play_queue_offset(1);
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
                                    fmt_time_precise(self.snap.position, self.snap.duration),
                                    fmt_time_precise(self.snap.duration, self.snap.duration)
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

                                if icon_btn(ui, Vec2::new(30.0, 28.0), draw_sliders_icon).clicked()
                                {
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

                    paint_frosted(
                        ui.painter(),
                        &self.blur,
                        orect,
                        [0.05, 0.05, 0.06, 0.62],
                        oe,
                        false,
                    );

                    ui.scope_builder(egui::UiBuilder::new().max_rect(orect), |ui| {
                        ui.set_opacity(oe);
                        let tab_h = 38.0;
                        let tab_w = ow / 3.0;
                        let foot_h = 38.0;

                        // Pure bright-vs-dim tab headers (NO gray box, NO underline!)
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::ZERO;
                            for (tab, label) in [
                                (OptTab::Audio, "Audio"),
                                (
                                    OptTab::Subs,
                                    if self.is_audio_session {
                                        "Lyrics"
                                    } else {
                                        "Subtitles"
                                    },
                                ),
                                (OptTab::Video, "Video"),
                            ] {
                                let col = if self.opt_tab == tab {
                                    Color32::WHITE
                                } else {
                                    Color32::from_rgba_unmultiplied(255, 255, 255, 105)
                                };
                                let b = egui::Button::new(
                                    egui::RichText::new(label).size(13.0).color(col),
                                )
                                .fill(Color32::TRANSPARENT);
                                if ui.add_sized(Vec2::new(tab_w, tab_h), b).clicked() {
                                    self.opt_tab = tab;
                                }
                            }
                        });

                        // True clipped horizontal sliding pager strictly INSIDE the Options panel!
                        let body_clip = Rect::from_min_size(
                            Pos2::new(orect.left() + 14.0, orect.top() + tab_h + 6.0),
                            Vec2::new(ow - 28.0, oh - tab_h - foot_h - 10.0),
                        );
                        let page_w = body_clip.width();
                        let row_w = page_w - 4.0;
                        let mpv = self.mpv.clone();

                        for (p_idx, page_tab) in [
                            (0.0_f32, OptTab::Audio),
                            (1.0_f32, OptTab::Subs),
                            (2.0_f32, OptTab::Video),
                        ] {
                            let dx = (p_idx - self.tab_slide_x) * (page_w + 16.0);
                            if dx.abs() > page_w + 14.0 {
                                continue;
                            }
                            let page_rect = body_clip.translate(Vec2::new(dx, 0.0));
                            ui.scope_builder(egui::UiBuilder::new().max_rect(page_rect), |ui| {
                                ui.set_clip_rect(body_clip);
                                egui::ScrollArea::vertical()
                                    .id_salt(("opt_page", p_idx as i32))
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                                        match page_tab {
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
                                                                egui::RichText::new(
                                                                    tr.display_label(),
                                                                )
                                                                .size(12.5)
                                                                .color(if active {
                                                                    Color32::WHITE
                                                                } else {
                                                                    Color32::from_gray(145)
                                                                }),
                                                            )
                                                            .fill(Color32::TRANSPARENT),
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
                                            }
                                            OptTab::Subs => {
                                                if self.is_audio_session {
                                                    if ui
                                                        .add_sized(
                                                            Vec2::new(row_w, 26.0),
                                                            egui::Button::new(
                                                                egui::RichText::new(
                                                                    "+ Upload Lyrics (.lrc)",
                                                                )
                                                                .size(12.0),
                                                            )
                                                            .fill(Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 14,
                                                            )),
                                                        )
                                                        .clicked()
                                                    {
                                                        if let Some(p) = rfd::FileDialog::new()
                                                            .add_filter("Lyrics", &["lrc", "txt"])
                                                            .pick_file()
                                                        {
                                                            self.load_lrc_path(&p);
                                                        }
                                                    }
                                                    let (ld, rld) = labeled_bar(
                                                        ui,
                                                        row_w,
                                                        "Lyrics Delay",
                                                        format!("{:+.1}s", self.lyrics_delay),
                                                        self.lyrics_delay,
                                                        -10.0,
                                                        10.0,
                                                        Some(0.0),
                                                    );
                                                    if let Some(v) = ld {
                                                        self.lyrics_delay =
                                                            (v * 10.0).round() / 10.0;
                                                    }
                                                    if rld {
                                                        self.lyrics_delay = 0.0;
                                                    }
                                                    ui.add_space(4.0);
                                                    ui.horizontal(|ui| {
                                                        ui.spacing_mut().item_spacing =
                                                            Vec2::new(4.0, 0.0);
                                                        ui.add(
                                                            egui::TextEdit::singleline(
                                                                &mut self.lyrics_query,
                                                            )
                                                            .hint_text("Search lyrics")
                                                            .frame(false)
                                                            .margin(Margin::symmetric(6, 5))
                                                            .desired_width(row_w - 56.0),
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
                                                        {
                                                            let q = self.lyrics_query.clone();
                                                            self.start_lyrics_search(q);
                                                        }
                                                    });
                                                    if !self.lyrics_msg.is_empty() {
                                                        ui.label(
                                                            egui::RichText::new(&self.lyrics_msg)
                                                                .size(11.5)
                                                                .color(Color32::from_gray(145)),
                                                        );
                                                    }
                                                    for item in self.lyrics_results.clone() {
                                                        let lbl = format!(
                                                            "{} — {}",
                                                            item.track, item.artist
                                                        );
                                                        if ui
                                                            .add_sized(
                                                                Vec2::new(row_w, 24.0),
                                                                egui::Button::new(
                                                                    egui::RichText::new(
                                                                        lbl.chars()
                                                                            .take(34)
                                                                            .collect::<String>(),
                                                                    )
                                                                    .size(11.5),
                                                                )
                                                                .fill(
                                                                    Color32::from_rgba_unmultiplied(
                                                                        255, 255, 255, 10,
                                                                    ),
                                                                ),
                                                            )
                                                            .clicked()
                                                        {
                                                            if let Some(txt) = item
                                                                .synced
                                                                .as_deref()
                                                                .or(item.plain.as_deref())
                                                            {
                                                                self.lyrics = parse_lrc(txt);
                                                                self.flash("Lyrics loaded");
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    if ui
                                                        .add_sized(
                                                            Vec2::new(row_w, 26.0),
                                                            egui::Button::new(
                                                                egui::RichText::new(
                                                                    "+ Upload Subtitle",
                                                                )
                                                                .size(12.0),
                                                            )
                                                            .fill(Color32::from_rgba_unmultiplied(
                                                                255, 255, 255, 14,
                                                            )),
                                                        )
                                                        .clicked()
                                                    {
                                                        if let Some(p) = rfd::FileDialog::new()
                                                            .add_filter(
                                                                "Subtitles",
                                                                &[
                                                                    "srt", "vtt", "ass", "ssa",
                                                                    "sub",
                                                                ],
                                                            )
                                                            .pick_file()
                                                        {
                                                            if let Some(m) = &mpv {
                                                                m.add_sub_file(
                                                                    &p.to_string_lossy(),
                                                                );
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
                                                                    .size(12.5)
                                                                    .color(if off {
                                                                        Color32::WHITE
                                                                    } else {
                                                                        Color32::from_gray(145)
                                                                    }),
                                                            )
                                                            .fill(Color32::TRANSPARENT),
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
                                                                    egui::RichText::new(
                                                                        tr.display_label(),
                                                                    )
                                                                    .size(12.5)
                                                                    .color(if active {
                                                                        Color32::WHITE
                                                                    } else {
                                                                        Color32::from_gray(145)
                                                                    }),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
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
                                                            m.set_sub_delay(
                                                                (nd * 10.0).round() / 10.0,
                                                            );
                                                        }
                                                    }
                                                    if rst {
                                                        if let Some(m) = &mpv {
                                                            m.set_sub_delay(0.0);
                                                        }
                                                    }
                                                }
                                            }
                                            OptTab::Video => {
                                                ui.label(
                                                    egui::RichText::new("Quality")
                                                        .size(11.5)
                                                        .color(Color32::from_gray(145)),
                                                );
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing =
                                                        Vec2::new(4.0, 0.0);
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
                                                                        .size(11.5)
                                                                        .color(if active {
                                                                            Color32::WHITE
                                                                        } else {
                                                                            Color32::from_gray(130)
                                                                        }),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
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
                                                    ui.spacing_mut().item_spacing =
                                                        Vec2::new(4.0, 0.0);
                                                    let bw = (row_w - 20.0) / 6.0;
                                                    for r in [0.5, 0.75, 1.0, 1.25, 1.5, 2.0] {
                                                        let active =
                                                            (self.snap.speed - r).abs() < 0.02;
                                                        if ui
                                                            .add_sized(
                                                                Vec2::new(bw, 26.0),
                                                                egui::Button::new(
                                                                    egui::RichText::new(format!(
                                                                        "{r}x"
                                                                    ))
                                                                    .size(11.5)
                                                                    .color(if active {
                                                                        Color32::WHITE
                                                                    } else {
                                                                        Color32::from_gray(130)
                                                                    }),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
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
                                                    ui.spacing_mut().item_spacing =
                                                        Vec2::new(4.0, 0.0);
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
                                                                        .size(11.5)
                                                                        .color(if active {
                                                                            Color32::WHITE
                                                                        } else {
                                                                            Color32::from_gray(130)
                                                                        }),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
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
                                                ui.add_space(6.0);
                                                ui.label(
                                                    egui::RichText::new("Rotate")
                                                        .size(11.5)
                                                        .color(Color32::from_gray(145)),
                                                );
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing =
                                                        Vec2::new(4.0, 0.0);
                                                    let bw = (row_w - 12.0) / 4.0;
                                                    for deg in [0, 90, 180, 270] {
                                                        let active = self.snap.rotate == deg;
                                                        if ui
                                                            .add_sized(
                                                                Vec2::new(bw, 26.0),
                                                                egui::Button::new(
                                                                    egui::RichText::new(format!(
                                                                        "{deg}°"
                                                                    ))
                                                                    .size(11.5)
                                                                    .color(if active {
                                                                        Color32::WHITE
                                                                    } else {
                                                                        Color32::from_gray(130)
                                                                    }),
                                                                )
                                                                .fill(Color32::TRANSPARENT),
                                                            )
                                                            .clicked()
                                                        {
                                                            if let Some(m) = &mpv {
                                                                m.set_video_rotate(deg);
                                                            }
                                                            self.flash(format!("{deg}°"));
                                                        }
                                                    }
                                                });
                                            }
                                        }
                                    });
                            });
                        }

                        // Pinned bottom "More options" row
                        let foot_rect = Rect::from_min_size(
                            Pos2::new(orect.left() + 14.0, orect.bottom() - foot_h + 2.0),
                            Vec2::new(ow - 28.0, 30.0),
                        );
                        ui.scope_builder(egui::UiBuilder::new().max_rect(foot_rect), |ui| {
                            if more_options_row(ui, ow - 28.0, "More options") {
                                self.studio_tab = Some(self.opt_tab);
                                self.opts_open = false;
                            }
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
                    paint_frosted(
                        ui.painter(),
                        &self.blur,
                        srect,
                        [0.04, 0.04, 0.05, 0.66],
                        se,
                        false,
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
        // Receive decoded full-res photo for PhotoViewer
        while let Ok((src_key, ci)) = self.photo_rx.try_recv() {
            let tex = ctx.load_texture(format!("photo_{src_key}"), ci, TextureOptions::LINEAR);
            if let Some(pv) = self.photo_viewer.as_mut() {
                if pv.source == src_key {
                    pv.tex = Some(tex);
                }
            }
        }

        // Receive decoded Cover-Flow card thumbnails (root items AND isolated folder children!)
        while let Ok((src_key, is_poster, ci)) = self.card_thumb_rx.try_recv() {
            let tex = ctx.load_texture(
                format!("card_{}_{}", is_poster, src_key),
                ci,
                TextureOptions::LINEAR,
            );
            let mut matched = false;
            for item in &mut self.library {
                if item.source == src_key {
                    if is_poster {
                        item.poster_thumb = Some(tex.clone());
                    } else {
                        item.resume_thumb = Some(tex.clone());
                        if item.poster_thumb.is_none() {
                            item.poster_thumb = Some(tex.clone());
                        }
                    }
                    matched = true;
                }
                for ch in &mut item.children {
                    if ch.source == src_key {
                        if is_poster {
                            ch.poster_thumb = Some(tex.clone());
                        } else {
                            ch.resume_thumb = Some(tex.clone());
                            if ch.poster_thumb.is_none() {
                                ch.poster_thumb = Some(tex.clone());
                            }
                        }
                        matched = true;
                    }
                }
            }
            if matched && !is_poster && (self.pending_close_since.is_some() || self.closing_morph) {
                self.morph_tex = Some(tex);
                if self.pending_close_since.take().is_some() {
                    self.closing_morph = true;
                    self.morph_start = Instant::now();
                }
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

        if self.photo_viewer.is_some() {
            self.draw_photo_viewer(ctx);
        } else if self.player_open && self.session.is_some() {
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
