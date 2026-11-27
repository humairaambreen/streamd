use eframe::glow::{self, HasContext};
use std::sync::Mutex;

const VS_SRC: &str = r#"#version 150 core
const vec2 POS[4] = vec2[4](
    vec2(-1.0, -1.0),
    vec2( 1.0, -1.0),
    vec2(-1.0,  1.0),
    vec2( 1.0,  1.0)
);
out vec2 v_uv;
void main() {
    vec2 p = POS[gl_VertexID];
    v_uv = p * 0.5 + 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

const FS_BLUR_SRC: &str = r#"#version 150 core
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_tex;
uniform vec2 u_dir; // (1/w, 0) or (0, 1/h)

void main() {
    // 9-tap Gaussian blur
    vec4 sum = texture(u_tex, v_uv) * 0.2270270270;
    sum += texture(u_tex, v_uv + u_dir * 1.3846153846) * 0.3162162162;
    sum += texture(u_tex, v_uv - u_dir * 1.3846153846) * 0.3162162162;
    sum += texture(u_tex, v_uv + u_dir * 3.2307692308) * 0.0702702703;
    sum += texture(u_tex, v_uv - u_dir * 3.2307692308) * 0.0702702703;
    frag_color = sum;
}
"#;

const FS_COMP_SRC: &str = r#"#version 150 core
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_tex;
uniform vec4 u_tint; // rgb + alpha tint over blurred video
uniform float u_alpha;
uniform int u_fade_right; // 1 = progressive horizontal fade out to right (for pause scrim)

void main() {
    vec3 blurred = texture(u_tex, v_uv).rgb;
    vec3 col = mix(blurred, u_tint.rgb, u_tint.a);
    float a = u_alpha;
    if (u_fade_right == 1) {
        a *= smoothstep(1.0, 0.0, pow(v_uv.x, 1.3));
    }
    frag_color = vec4(col, a);
}
"#;

struct BlurInner {
    vao: glow::VertexArray,
    prog_blur: glow::Program,
    prog_comp: glow::Program,
    fbo_a: glow::Framebuffer,
    tex_a: glow::Texture,
    fbo_b: glow::Framebuffer,
    tex_b: glow::Texture,
    bw: i32,
    bh: i32,
}

pub struct GpuBlur {
    inner: Mutex<Option<BlurInner>>,
}

unsafe impl Send for GpuBlur {}
unsafe impl Sync for GpuBlur {}

impl GpuBlur {
    pub fn new(gl: &glow::Context) -> Self {
        let inner = unsafe { Self::init_gl(gl) };
        Self {
            inner: Mutex::new(inner),
        }
    }

    unsafe fn compile(gl: &glow::Context, vs: &str, fs: &str) -> Option<glow::Program> {
        let p = gl.create_program().ok()?;
        let vsh = gl.create_shader(glow::VERTEX_SHADER).ok()?;
        gl.shader_source(vsh, vs);
        gl.compile_shader(vsh);
        if !gl.get_shader_compile_status(vsh) {
            gl.delete_shader(vsh);
            gl.delete_program(p);
            return None;
        }
        let fsh = gl.create_shader(glow::FRAGMENT_SHADER).ok()?;
        gl.shader_source(fsh, fs);
        gl.compile_shader(fsh);
        if !gl.get_shader_compile_status(fsh) {
            gl.delete_shader(vsh);
            gl.delete_shader(fsh);
            gl.delete_program(p);
            return None;
        }
        gl.attach_shader(p, vsh);
        gl.attach_shader(p, fsh);
        gl.link_program(p);
        gl.delete_shader(vsh);
        gl.delete_shader(fsh);
        if !gl.get_program_link_status(p) {
            gl.delete_program(p);
            return None;
        }
        Some(p)
    }

    unsafe fn make_rt(
        gl: &glow::Context,
        w: i32,
        h: i32,
    ) -> Option<(glow::Framebuffer, glow::Texture)> {
        let tex = gl.create_texture().ok()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        let fbo = gl.create_framebuffer().ok()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        Some((fbo, tex))
    }

    unsafe fn init_gl(gl: &glow::Context) -> Option<BlurInner> {
        let vao = gl.create_vertex_array().ok()?;
        let prog_blur = Self::compile(gl, VS_SRC, FS_BLUR_SRC)?;
        let prog_comp = Self::compile(gl, VS_SRC, FS_COMP_SRC)?;
        let (fbo_a, tex_a) = Self::make_rt(gl, 64, 64)?;
        let (fbo_b, tex_b) = Self::make_rt(gl, 64, 64)?;
        Some(BlurInner {
            vao,
            prog_blur,
            prog_comp,
            fbo_a,
            tex_a,
            fbo_b,
            tex_b,
            bw: 64,
            bh: 64,
        })
    }

    /// Blurs the live framebuffer region `(x, y_bottom, w, h)` in pixel coordinates and composites
    /// frosted glass back onto `dst_fbo`.
    pub fn draw_frosted_region(
        &self,
        gl: &glow::Context,
        dst_fbo: Option<glow::Framebuffer>,
        x: i32,
        y_bottom: i32,
        w: i32,
        h: i32,
        tint_rgba: [f32; 4],
        alpha: f32,
        fade_right: bool,
    ) {
        if w < 4 || h < 4 || alpha <= 0.01 {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        let Some(st) = guard.as_mut() else { return };

        let bw = (w / 4).clamp(16, 512);
        let bh = (h / 4).clamp(16, 512);

        unsafe {
            // Resize downsampled ping-pong targets if needed
            if st.bw != bw || st.bh != bh {
                for tex in [st.tex_a, st.tex_b] {
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        bw,
                        bh,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(None),
                    );
                }
                st.bw = bw;
                st.bh = bh;
            }

            let was_scissor = gl.is_enabled(glow::SCISSOR_TEST);
            let was_blend = gl.is_enabled(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);

            // 1. Downsample copy from current framebuffer region into fbo_a
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, dst_fbo);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(st.fbo_a));
            gl.blit_framebuffer(
                x,
                y_bottom,
                x + w,
                y_bottom + h,
                0,
                0,
                bw,
                bh,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );

            gl.bind_vertex_array(Some(st.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.disable(glow::BLEND);

            // 2. Two iterations of separable H+V Gaussian blur for deep Apple-like frosted glass
            gl.use_program(Some(st.prog_blur));
            if let Some(loc) = gl.get_uniform_location(st.prog_blur, "u_tex") {
                gl.uniform_1_i32(Some(&loc), 0);
            }
            let dir_loc = gl.get_uniform_location(st.prog_blur, "u_dir");
            gl.viewport(0, 0, bw, bh);

            for pass in 0..2 {
                let spread = if pass == 0 { 1.4_f32 } else { 2.6_f32 };
                // Horizontal: tex_a -> fbo_b
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(st.fbo_b));
                gl.bind_texture(glow::TEXTURE_2D, Some(st.tex_a));
                if let Some(loc) = &dir_loc {
                    gl.uniform_2_f32(Some(loc), spread / bw as f32, 0.0);
                }
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Vertical: tex_b -> fbo_a
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(st.fbo_a));
                gl.bind_texture(glow::TEXTURE_2D, Some(st.tex_b));
                if let Some(loc) = &dir_loc {
                    gl.uniform_2_f32(Some(loc), 0.0, spread / bh as f32);
                }
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // 3. Composite frosted blur back onto dst_fbo at (x, y_bottom, w, h)
            gl.bind_framebuffer(glow::FRAMEBUFFER, dst_fbo);
            gl.viewport(x, y_bottom, w, h);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

            gl.use_program(Some(st.prog_comp));
            gl.bind_texture(glow::TEXTURE_2D, Some(st.tex_a));
            if let Some(loc) = gl.get_uniform_location(st.prog_comp, "u_tex") {
                gl.uniform_1_i32(Some(&loc), 0);
            }
            if let Some(loc) = gl.get_uniform_location(st.prog_comp, "u_tint") {
                gl.uniform_4_f32(
                    Some(&loc),
                    tint_rgba[0],
                    tint_rgba[1],
                    tint_rgba[2],
                    tint_rgba[3],
                );
            }
            if let Some(loc) = gl.get_uniform_location(st.prog_comp, "u_alpha") {
                gl.uniform_1_f32(Some(&loc), alpha);
            }
            if let Some(loc) = gl.get_uniform_location(st.prog_comp, "u_fade_right") {
                gl.uniform_1_i32(Some(&loc), if fade_right { 1 } else { 0 });
            }
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Restore GL state cleanly for egui
            gl.use_program(None);
            gl.bind_vertex_array(None);
            if was_scissor {
                gl.enable(glow::SCISSOR_TEST);
            }
            if !was_blend {
                gl.disable(glow::BLEND);
            }
        }
    }
}
