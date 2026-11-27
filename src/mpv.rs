use libloading::{Library, Symbol};
use serde::Deserialize;
use std::{
    ffi::{c_char, c_double, c_int, c_void, CStr, CString},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex,
    },
};

const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_RENDER_PARAM_INVALID: c_int = 0;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;

#[repr(C)]
struct MpvRenderParam {
    type_: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

type FnCreate = unsafe extern "C" fn() -> *mut c_void;
type FnInitialize = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnTerminateDestroy = unsafe extern "C" fn(*mut c_void);
type FnSetOptionString = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnCommand = unsafe extern "C" fn(*mut c_void, *mut *const c_char) -> c_int;
type FnCommandAsync = unsafe extern "C" fn(*mut c_void, u64, *mut *const c_char) -> c_int;
type FnSetProperty = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
type FnGetProperty = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
type FnGetPropertyString = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type FnFree = unsafe extern "C" fn(*mut c_void);
type FnWaitEvent = unsafe extern "C" fn(*mut c_void, c_double) -> *mut c_void;
type FnRenderContextCreate =
    unsafe extern "C" fn(*mut *mut c_void, *mut c_void, *mut MpvRenderParam) -> c_int;
type FnRenderContextSetUpdateCallback =
    unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void), *mut c_void);
type FnRenderContextRender = unsafe extern "C" fn(*mut c_void, *mut MpvRenderParam) -> c_int;
type FnRenderContextFree = unsafe extern "C" fn(*mut c_void);

#[derive(Clone, Debug, Deserialize)]
pub struct TrackInfo {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub selected: bool,
}

impl TrackInfo {
    pub fn display_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = self.title.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(t.trim().to_string());
        }
        if let Some(l) = self.lang.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(l.to_uppercase());
        }
        if parts.is_empty() {
            parts.push(format!("Track {}", self.id));
        }
        if let Some(c) = self.codec.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(c.to_uppercase());
        }
        parts.join(" · ")
    }
}

#[derive(Clone, Default)]
pub struct PlayerSnapshot {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub buffering: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub width: i64,
    pub height: i64,
    pub sub_delay: f64,
    pub sub_speed: f64,
    pub sub_scale: f64,
    pub sub_pos: i64,
    pub video_zoom: f64,
    pub panscan: f64,
    pub align_x: f64,
    pub align_y: f64,
    pub rotate: i64,
    pub brightness: i64,
    pub contrast: i64,
    pub saturation: i64,
    pub gamma: i64,
    pub deband: bool,
    pub audio_delay: f64,
    pub pitch_correction: bool,
    pub media_title: String,
    pub audio_tracks: Vec<TrackInfo>,
    pub sub_tracks: Vec<TrackInfo>,
    pub active_aid: i64,
    pub active_sid: i64,
}

struct MpvApi {
    _lib: Library,
    create: FnCreate,
    initialize: FnInitialize,
    terminate_destroy: FnTerminateDestroy,
    set_option_string: FnSetOptionString,
    command: FnCommand,
    command_async: FnCommandAsync,
    set_property: FnSetProperty,
    get_property: FnGetProperty,
    get_property_string: FnGetPropertyString,
    free: FnFree,
    wait_event: FnWaitEvent,
    render_context_create: FnRenderContextCreate,
    render_context_set_update_callback: FnRenderContextSetUpdateCallback,
    render_context_render: FnRenderContextRender,
    render_context_free: FnRenderContextFree,
}

impl MpvApi {
    fn load() -> anyhow::Result<Self> {
        let candidates: Vec<String> = {
            let mut v = Vec::new();
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    #[cfg(target_os = "windows")]
                    {
                        v.push(dir.join("libmpv-2.dll").to_string_lossy().into_owned());
                        v.push(dir.join("mpv-2.dll").to_string_lossy().into_owned());
                    }
                    #[cfg(target_os = "macos")]
                    {
                        v.push(
                            dir.join("../Frameworks/libmpv.2.dylib")
                                .to_string_lossy()
                                .into_owned(),
                        );
                        v.push(
                            dir.join("../Frameworks/libmpv.dylib")
                                .to_string_lossy()
                                .into_owned(),
                        );
                        v.push(dir.join("libmpv.2.dylib").to_string_lossy().into_owned());
                        v.push(dir.join("libmpv.dylib").to_string_lossy().into_owned());
                    }
                    #[cfg(all(unix, not(target_os = "macos")))]
                    {
                        v.push(dir.join("libmpv.so.2").to_string_lossy().into_owned());
                        v.push(dir.join("libmpv.so").to_string_lossy().into_owned());
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                v.push("/opt/homebrew/lib/libmpv.2.dylib".into());
                v.push("/opt/homebrew/lib/libmpv.dylib".into());
                v.push("/usr/local/lib/libmpv.2.dylib".into());
                v.push("/usr/local/lib/libmpv.dylib".into());
                v.push("libmpv.2.dylib".into());
                v.push("libmpv.dylib".into());
            }
            #[cfg(target_os = "windows")]
            {
                v.push("libmpv-2.dll".into());
                v.push("mpv-2.dll".into());
                v.push("mpv-1.dll".into());
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                v.push("libmpv.so.2".into());
                v.push("libmpv.so".into());
            }
            v
        };

        let mut last_err = String::new();
        let mut loaded = None;
        for path in &candidates {
            match unsafe { Library::new(path) } {
                Ok(lib) => {
                    loaded = Some(lib);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let lib = loaded.ok_or_else(|| {
            #[cfg(target_os = "macos")]
            let hint = "Install with: brew install mpv";
            #[cfg(target_os = "windows")]
            let hint = "Place libmpv-2.dll next to streamd.exe (from shinchiro/mpv-winbuild-cmake)";
            #[cfg(all(unix, not(target_os = "macos")))]
            let hint = "Install libmpv (e.g. apt install libmpv2)";
            anyhow::anyhow!("Could not load libmpv ({last_err}). {hint}")
        })?;

        unsafe {
            let get = |name: &[u8]| -> anyhow::Result<*const c_void> {
                let sym: Symbol<*const c_void> = lib.get(name)?;
                Ok(*sym)
            };
            let create: FnCreate = std::mem::transmute(get(b"mpv_create\0")?);
            let initialize: FnInitialize = std::mem::transmute(get(b"mpv_initialize\0")?);
            let terminate_destroy: FnTerminateDestroy =
                std::mem::transmute(get(b"mpv_terminate_destroy\0")?);
            let set_option_string: FnSetOptionString =
                std::mem::transmute(get(b"mpv_set_option_string\0")?);
            let command: FnCommand = std::mem::transmute(get(b"mpv_command\0")?);
            let command_async: FnCommandAsync = std::mem::transmute(get(b"mpv_command_async\0")?);
            let set_property: FnSetProperty = std::mem::transmute(get(b"mpv_set_property\0")?);
            let get_property: FnGetProperty = std::mem::transmute(get(b"mpv_get_property\0")?);
            let get_property_string: FnGetPropertyString =
                std::mem::transmute(get(b"mpv_get_property_string\0")?);
            let free: FnFree = std::mem::transmute(get(b"mpv_free\0")?);
            let wait_event: FnWaitEvent = std::mem::transmute(get(b"mpv_wait_event\0")?);
            let render_context_create: FnRenderContextCreate =
                std::mem::transmute(get(b"mpv_render_context_create\0")?);
            let render_context_set_update_callback: FnRenderContextSetUpdateCallback =
                std::mem::transmute(get(b"mpv_render_context_set_update_callback\0")?);
            let render_context_render: FnRenderContextRender =
                std::mem::transmute(get(b"mpv_render_context_render\0")?);
            let render_context_free: FnRenderContextFree =
                std::mem::transmute(get(b"mpv_render_context_free\0")?);

            Ok(Self {
                _lib: lib,
                create,
                initialize,
                terminate_destroy,
                set_option_string,
                command,
                command_async,
                set_property,
                get_property,
                get_property_string,
                free,
                wait_event,
                render_context_create,
                render_context_set_update_callback,
                render_context_render,
                render_context_free,
            })
        }
    }
}

struct RepaintCtx {
    egui_ctx: eframe::egui::Context,
    has_frame: AtomicBool,
}

unsafe extern "C" fn mpv_update_cb(ctx: *mut c_void) {
    if !ctx.is_null() {
        let rc = &*(ctx as *const RepaintCtx);
        rc.has_frame.store(true, Relaxed);
        rc.egui_ctx.request_repaint();
    }
}

unsafe extern "C" fn gl_get_proc_addr(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    if ctx.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    let f = &*(ctx as *const &dyn Fn(&CStr) -> *const c_void);
    let cstr = CStr::from_ptr(name);
    f(cstr) as *mut c_void
}

pub struct MpvPlayer {
    api: MpvApi,
    handle: *mut c_void,
    render_ctx: Mutex<*mut c_void>,
    repaint_ctx: *mut RepaintCtx,
    destroyed: AtomicBool,
}

unsafe impl Send for MpvPlayer {}
unsafe impl Sync for MpvPlayer {}

impl MpvPlayer {
    pub fn new(
        egui_ctx: eframe::egui::Context,
        get_proc_address: &dyn Fn(&CStr) -> *const c_void,
    ) -> anyhow::Result<Arc<Self>> {
        // libmpv requires C numeric locale on Unix
        #[cfg(unix)]
        unsafe {
            extern "C" {
                fn setlocale(category: c_int, locale: *const c_char) -> *const c_char;
            }
            // LC_NUMERIC is 4 on macOS, 1 on Linux
            #[cfg(target_os = "macos")]
            const LC_NUMERIC: c_int = 4;
            #[cfg(not(target_os = "macos"))]
            const LC_NUMERIC: c_int = 1;
            setlocale(LC_NUMERIC, b"C\0".as_ptr() as *const c_char);
        }

        let api = MpvApi::load()?;
        let handle = unsafe { (api.create)() };
        anyhow::ensure!(!handle.is_null(), "mpv_create returned null");

        let set_opt = |k: &str, v: &str| {
            let ck = CString::new(k).unwrap();
            let cv = CString::new(v).unwrap();
            unsafe { (api.set_option_string)(handle, ck.as_ptr(), cv.as_ptr()) };
        };

        set_opt("vo", "libmpv");
        set_opt("hwdec", "auto-safe");
        set_opt("keep-open", "yes");
        set_opt("cache", "yes");
        set_opt("cache-pause-initial", "no");
        set_opt("cache-pause-wait", "0.5");
        set_opt("demuxer-readahead-secs", "60");
        set_opt("demuxer-max-bytes", "128MiB");
        set_opt("demuxer-max-back-bytes", "64MiB");
        set_opt("hr-seek", "no");
        set_opt("vd-lavc-fast", "yes");
        set_opt("volume-max", "300");
        set_opt("osc", "no");
        set_opt("osd-level", "0");
        set_opt("sub-font-size", "42");
        set_opt("sub-border-size", "2");
        set_opt("sub-shadow-offset", "1");

        let rc = unsafe { (api.initialize)(handle) };
        if rc < 0 {
            unsafe { (api.terminate_destroy)(handle) };
            anyhow::bail!("mpv_initialize failed (code {rc})");
        }

        let api_type = CString::new("opengl").unwrap();
        let mut proc_fn_ref: &dyn Fn(&CStr) -> *const c_void = get_proc_address;
        let mut gl_init = MpvOpenglInitParams {
            get_proc_address: gl_get_proc_addr,
            get_proc_address_ctx: (&mut proc_fn_ref as *mut &dyn Fn(&CStr) -> *const c_void)
                as *mut c_void,
        };
        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr() as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&mut gl_init as *mut MpvOpenglInitParams) as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        let mut render_ctx: *mut c_void = ptr::null_mut();
        let rrc =
            unsafe { (api.render_context_create)(&mut render_ctx, handle, params.as_mut_ptr()) };
        if rrc < 0 || render_ctx.is_null() {
            unsafe { (api.terminate_destroy)(handle) };
            anyhow::bail!("mpv_render_context_create failed (code {rrc})");
        }

        let repaint_ctx = Box::into_raw(Box::new(RepaintCtx {
            egui_ctx,
            has_frame: AtomicBool::new(false),
        }));
        unsafe {
            (api.render_context_set_update_callback)(
                render_ctx,
                mpv_update_cb,
                repaint_ctx as *mut c_void,
            );
        }

        Ok(Arc::new(Self {
            api,
            handle,
            render_ctx: Mutex::new(render_ctx),
            repaint_ctx,
            destroyed: AtomicBool::new(false),
        }))
    }

    pub fn render_gl(&self, fbo_id: i32, width: i32, height: i32) {
        if self.destroyed.load(Relaxed) || width <= 0 || height <= 0 {
            return;
        }
        let guard = self.render_ctx.lock().unwrap();
        let rctx = *guard;
        if rctx.is_null() {
            return;
        }
        let mut fbo = MpvOpenglFbo {
            fbo: fbo_id,
            w: width,
            h: height,
            internal_format: 0,
        };
        let mut flip_y: c_int = 1;
        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_FBO,
                data: (&mut fbo as *mut MpvOpenglFbo) as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_FLIP_Y,
                data: (&mut flip_y as *mut c_int) as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        unsafe {
            (self.api.render_context_render)(rctx, params.as_mut_ptr());
        }
    }

    fn cmd_async(&self, args: &[&str]) {
        if self.destroyed.load(Relaxed) {
            return;
        }
        let cstrings: Vec<CString> = args.iter().filter_map(|s| CString::new(*s).ok()).collect();
        let mut ptrs: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        unsafe {
            (self.api.command_async)(self.handle, 0, ptrs.as_mut_ptr());
        }
    }

    fn cmd_sync(&self, args: &[&str]) {
        if self.destroyed.load(Relaxed) {
            return;
        }
        let cstrings: Vec<CString> = args.iter().filter_map(|s| CString::new(*s).ok()).collect();
        let mut ptrs: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        unsafe {
            (self.api.command)(self.handle, ptrs.as_mut_ptr());
        }
    }

    pub fn load_url(&self, url: &str) {
        self.set_flag("pause", false);
        self.cmd_async(&["loadfile", url, "replace"]);
    }

    pub fn stop(&self) {
        self.cmd_sync(&["stop"]);
    }

    pub fn toggle_pause(&self) {
        self.cmd_async(&["cycle", "pause"]);
    }

    pub fn seek_relative(&self, sec: f64) {
        let s = format!("{sec:.2}");
        self.cmd_async(&["seek", &s, "relative"]);
    }

    pub fn seek_absolute(&self, sec: f64) {
        let s = format!("{:.2}", sec.max(0.0));
        self.cmd_async(&["seek", &s, "absolute"]);
    }

    pub fn set_volume(&self, vol: f64) {
        self.set_double("volume", vol.clamp(0.0, 300.0));
    }

    pub fn toggle_mute(&self) {
        self.cmd_async(&["cycle", "mute"]);
    }

    pub fn set_speed(&self, speed: f64) {
        self.set_double("speed", speed.clamp(0.25, 4.0));
    }

    pub fn set_audio_track(&self, id: i64) {
        self.set_int64("aid", id);
    }

    pub fn set_sub_track(&self, id: i64) {
        if id <= 0 {
            self.set_string("sid", "no");
        } else {
            self.set_int64("sid", id);
        }
    }

    pub fn set_sub_delay(&self, sec: f64) {
        self.set_double("sub-delay", sec);
    }

    pub fn set_sub_speed(&self, speed: f64) {
        self.set_double("sub-speed", speed.clamp(0.5, 2.0));
    }

    pub fn set_sub_scale(&self, scale: f64) {
        self.set_double("sub-scale", scale.clamp(0.4, 2.5));
    }

    pub fn add_sub_file(&self, path: &str) {
        self.cmd_async(&["sub-add", path, "select"]);
    }

    pub fn set_sub_pos(&self, pos: i64) {
        self.set_int64("sub-pos", pos.clamp(0, 100));
    }

    pub fn reset_subs(&self) {
        self.set_double("sub-delay", 0.0);
        self.set_double("sub-speed", 1.0);
        self.set_double("sub-scale", 1.0);
        self.set_int64("sub-pos", 100);
    }

    // ---------- Video Studio ----------
    pub fn set_video_zoom(&self, z: f64) {
        self.set_double("video-zoom", z.clamp(-0.5, 1.5));
    }

    pub fn set_panscan(&self, p: f64) {
        self.set_double("panscan", p.clamp(0.0, 1.0));
    }

    pub fn set_aspect_mode(&self, mode: &str) {
        match mode {
            "stretch" => {
                self.set_flag("keepaspect", false);
                self.set_string("video-aspect-override", "-1");
            }
            "auto" => {
                self.set_flag("keepaspect", true);
                self.set_string("video-aspect-override", "-1");
            }
            other => {
                self.set_flag("keepaspect", true);
                self.set_string("video-aspect-override", other);
            }
        }
    }

    pub fn set_video_align_x(&self, x: f64) {
        self.set_double("video-align-x", x.clamp(-1.0, 1.0));
    }

    pub fn set_video_align_y(&self, y: f64) {
        self.set_double("video-align-y", y.clamp(-1.0, 1.0));
    }

    pub fn set_video_rotate(&self, deg: i64) {
        self.set_int64("video-rotate", deg.rem_euclid(360));
    }

    pub fn set_upscaler(&self, scaler: &str) {
        self.set_string("scale", scaler);
    }

    pub fn set_deband(&self, on: bool) {
        self.set_flag("deband", on);
    }

    pub fn set_video_quality_cap(&self, max_h: i64) {
        if max_h <= 0 {
            self.set_string("vf", "");
        } else {
            self.set_string("vf", &format!("scale=-2:'min(ih,{max_h})'"));
        }
    }

    pub fn set_brightness(&self, v: i64) {
        self.set_int64("brightness", v.clamp(-50, 50));
    }

    pub fn set_contrast(&self, v: i64) {
        self.set_int64("contrast", v.clamp(-50, 50));
    }

    pub fn set_saturation(&self, v: i64) {
        self.set_int64("saturation", v.clamp(-50, 50));
    }

    pub fn set_gamma(&self, v: i64) {
        self.set_int64("gamma", v.clamp(-50, 50));
    }

    pub fn reset_video(&self) {
        self.set_double("video-zoom", 0.0);
        self.set_double("panscan", 0.0);
        self.set_flag("keepaspect", true);
        self.set_string("video-aspect-override", "-1");
        self.set_double("video-align-x", 0.0);
        self.set_double("video-align-y", 0.0);
        self.set_int64("video-rotate", 0);
        self.set_string("scale", "spline36");
        self.set_flag("deband", false);
        self.set_int64("brightness", 0);
        self.set_int64("contrast", 0);
        self.set_int64("saturation", 0);
        self.set_int64("gamma", 0);
    }

    // ---------- Audio Studio ----------
    pub fn set_audio_delay(&self, sec: f64) {
        self.set_double("audio-delay", sec.clamp(-5.0, 5.0));
    }

    pub fn set_pitch_correction(&self, on: bool) {
        self.set_flag("audio-pitch-correction", on);
    }

    pub fn set_night_mode(&self, on: bool) {
        if on {
            self.set_string("af", "lavfi=[dynaudnorm=f=150:g=15]");
        } else {
            self.set_string("af", "");
        }
    }

    pub fn set_stereo_downmix(&self, on: bool) {
        self.set_string("audio-channels", if on { "stereo" } else { "auto-safe" });
    }

    pub fn reset_audio(&self) {
        self.set_double("volume", 100.0);
        self.set_double("audio-delay", 0.0);
        self.set_flag("audio-pitch-correction", true);
        self.set_string("af", "");
        self.set_string("audio-channels", "auto-safe");
    }

    fn set_flag(&self, name: &str, val: bool) {
        let cname = CString::new(name).unwrap();
        let mut v: c_int = if val { 1 } else { 0 };
        unsafe {
            (self.api.set_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_FLAG,
                (&mut v as *mut c_int) as *mut c_void,
            );
        }
    }

    fn set_int64(&self, name: &str, mut val: i64) {
        let cname = CString::new(name).unwrap();
        unsafe {
            (self.api.set_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_INT64,
                (&mut val as *mut i64) as *mut c_void,
            );
        }
    }

    fn set_double(&self, name: &str, mut val: f64) {
        let cname = CString::new(name).unwrap();
        unsafe {
            (self.api.set_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_DOUBLE,
                (&mut val as *mut f64) as *mut c_void,
            );
        }
    }

    fn set_string(&self, name: &str, val: &str) {
        let cname = CString::new(name).unwrap();
        let cval = CString::new(val).unwrap();
        let mut ptr = cval.as_ptr();
        unsafe {
            (self.api.set_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_STRING,
                (&mut ptr as *mut *const c_char) as *mut c_void,
            );
        }
    }

    fn get_double(&self, name: &str) -> f64 {
        let cname = CString::new(name).unwrap();
        let mut v: f64 = 0.0;
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_DOUBLE,
                (&mut v as *mut f64) as *mut c_void,
            )
        };
        if rc >= 0 {
            v
        } else {
            0.0
        }
    }

    fn get_int64(&self, name: &str) -> i64 {
        let cname = CString::new(name).unwrap();
        let mut v: i64 = 0;
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_INT64,
                (&mut v as *mut i64) as *mut c_void,
            )
        };
        if rc >= 0 {
            v
        } else {
            0
        }
    }

    fn get_flag(&self, name: &str) -> bool {
        let cname = CString::new(name).unwrap();
        let mut v: c_int = 0;
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                cname.as_ptr(),
                MPV_FORMAT_FLAG,
                (&mut v as *mut c_int) as *mut c_void,
            )
        };
        rc >= 0 && v != 0
    }

    fn get_string(&self, name: &str) -> Option<String> {
        let cname = CString::new(name).unwrap();
        let ptr = unsafe { (self.api.get_property_string)(self.handle, cname.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { (self.api.free)(ptr as *mut c_void) };
        Some(s)
    }

    pub fn poll_state(&self) -> PlayerSnapshot {
        if self.destroyed.load(Relaxed) {
            return PlayerSnapshot::default();
        }
        // Drain pending mpv events non-blockingly so the event queue never backs up
        for _ in 0..32 {
            let ev = unsafe { (self.api.wait_event)(self.handle, 0.0) };
            if ev.is_null() {
                break;
            }
            let event_id = unsafe { *(ev as *const c_int) };
            if event_id == 0 {
                break;
            }
        }

        let tracks: Vec<TrackInfo> = self
            .get_string("track-list")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();

        let audio_tracks: Vec<TrackInfo> = tracks
            .iter()
            .filter(|t| t.kind == "audio")
            .cloned()
            .collect();
        let sub_tracks: Vec<TrackInfo> =
            tracks.iter().filter(|t| t.kind == "sub").cloned().collect();

        let active_aid = audio_tracks
            .iter()
            .find(|t| t.selected)
            .map(|t| t.id)
            .unwrap_or(0);
        let active_sid = sub_tracks
            .iter()
            .find(|t| t.selected)
            .map(|t| t.id)
            .unwrap_or(0);

        PlayerSnapshot {
            position: self.get_double("time-pos").max(0.0),
            duration: self.get_double("duration").max(0.0),
            paused: self.get_flag("pause"),
            buffering: self.get_flag("paused-for-cache"),
            volume: self.get_double("volume"),
            muted: self.get_flag("mute"),
            speed: self.get_double("speed").max(0.25),
            width: self.get_int64("video-params/w"),
            height: self.get_int64("video-params/h"),
            sub_delay: self.get_double("sub-delay"),
            sub_speed: {
                let ss = self.get_double("sub-speed");
                if ss > 0.0 {
                    ss
                } else {
                    1.0
                }
            },
            sub_scale: self.get_double("sub-scale").max(0.5),
            sub_pos: {
                let sp = self.get_int64("sub-pos");
                if sp == 0 && !self.get_flag("sub-visibility") {
                    100
                } else {
                    sp
                }
            },
            video_zoom: self.get_double("video-zoom"),
            panscan: self.get_double("panscan"),
            align_x: self.get_double("video-align-x"),
            align_y: self.get_double("video-align-y"),
            rotate: self.get_int64("video-rotate"),
            brightness: self.get_int64("brightness"),
            contrast: self.get_int64("contrast"),
            saturation: self.get_int64("saturation"),
            gamma: self.get_int64("gamma"),
            deband: self.get_flag("deband"),
            audio_delay: self.get_double("audio-delay"),
            pitch_correction: self.get_flag("audio-pitch-correction"),
            media_title: self
                .get_string("metadata/by-key/title")
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_default(),
            audio_tracks,
            sub_tracks,
            active_aid,
            active_sid,
        }
    }

    pub fn destroy(&self) {
        if self.destroyed.swap(true, Relaxed) {
            return;
        }
        let mut guard = self.render_ctx.lock().unwrap();
        if !guard.is_null() {
            unsafe { (self.api.render_context_free)(*guard) };
            *guard = ptr::null_mut();
        }
        drop(guard);
        unsafe {
            (self.api.terminate_destroy)(self.handle);
            if !self.repaint_ctx.is_null() {
                drop(Box::from_raw(self.repaint_ctx));
            }
        }
    }
}
