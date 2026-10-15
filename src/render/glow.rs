//! OpenGL ES 2.0 / 3.x renderer on top of `egui_glow`.

use std::sync::Arc;

use egui_glow::glow::{self, HasContext as _};

use super::Renderer;
use crate::{Error, Rotation, geometry::blit_tex_coords};

pub(crate) struct GlowRenderer {
    gl: Arc<glow::Context>,
    painter: egui_glow::Painter,
    /// Present when the rotation is done in software: egui paints into this FBO, which is then
    /// drawn rotated onto the scanout surface.
    rotated: Option<RotatedTarget>,
    destroyed: bool,
}

impl GlowRenderer {
    /// `sw_rotation` is the rotation the renderer has to apply itself (the display controller
    /// could not do it). `panel_size` is the size of the default framebuffer.
    pub(crate) fn new(
        gl: Arc<glow::Context>,
        shader_version: Option<egui_glow::ShaderVersion>,
        dithering: bool,
        sw_rotation: Rotation,
        panel_size: [u32; 2],
    ) -> Result<Self, Error> {
        check_limitations(&gl);

        let painter = egui_glow::Painter::new(gl.clone(), "", shader_version, dithering)
            .map_err(|e| Error::Gl(format!("creating egui painter: {e}")))?;

        let rotated = if sw_rotation == Rotation::Deg0 {
            None
        } else {
            log::info!(
                "Rotating {}° in software (offscreen framebuffer + blit)",
                sw_rotation.degrees()
            );
            Some(unsafe { RotatedTarget::new(&gl, sw_rotation, panel_size)? })
        };

        Ok(Self {
            gl,
            painter,
            rotated,
            destroyed: false,
        })
    }
}

impl Renderer for GlowRenderer {
    fn max_texture_side(&self) -> usize {
        self.painter.max_texture_side()
    }

    fn paint(
        &mut self,
        size_px: [u32; 2],
        pixels_per_point: f32,
        clear_color: [f32; 4],
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &mut egui::TexturesDelta,
    ) {
        if let Some(rotated) = &self.rotated {
            unsafe {
                self.gl
                    .bind_framebuffer(glow::FRAMEBUFFER, Some(rotated.fbo))
            };
        }

        egui_glow::painter::clear(&self.gl, size_px, clear_color);
        self.painter.paint_and_update_textures(
            size_px,
            pixels_per_point,
            primitives,
            textures_delta,
        );

        if let Some(rotated) = &self.rotated {
            unsafe { rotated.blit(&self.gl) };
        }
    }

    fn destroy(&mut self) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        if let Some(rotated) = self.rotated.take() {
            unsafe { rotated.destroy(&self.gl) };
        }
        self.painter.destroy();
    }
}

/// Logs the GL implementation and warns about limits that break egui.
fn check_limitations(gl: &glow::Context) {
    let extensions = gl.supported_extensions();
    log::debug!("GL extensions: {extensions:?}");

    let version = gl.version();
    // egui meshes use 32-bit indices. GLES 3.0 has them in core, GLES 2.0 needs the extension
    // (missing on some Vivante GC2000 / Mali-400 drivers).
    if version.is_embedded && version.major < 3 && !extensions.contains("GL_OES_element_index_uint")
    {
        log::warn!(
            "GL_OES_element_index_uint is not supported: egui's 32-bit index draws will fail on \
             this GPU"
        );
    }
}

/// `GL_VENDOR`, `GL_RENDERER`, `GL_VERSION` and the GLSL version on one line.
pub(crate) fn renderer_info(gl: &glow::Context) -> String {
    unsafe {
        format!(
            "{} / {} / {} / GLSL {}",
            gl.get_parameter_string(glow::VENDOR),
            gl.get_parameter_string(glow::RENDERER),
            gl.get_parameter_string(glow::VERSION),
            gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
        )
    }
}

// GLSL ES 1.00 works in both GLES 2 and GLES 3 contexts.
const BLIT_VERT: &str = "#version 100
attribute vec2 a_pos;
attribute vec2 a_tc;
varying vec2 v_tc;
void main() {
    v_tc = a_tc;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
";

// highp where available: mediump texture coordinates are off by more than half a texel at
// 1280+ pixels, which would show as smeared text with NEAREST sampling.
const BLIT_FRAG: &str = "#version 100
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform sampler2D u_tex;
varying vec2 v_tc;
void main() {
    gl_FragColor = texture2D(u_tex, v_tc);
}
";

const A_POS: u32 = 0;
const A_TC: u32 = 1;

/// Offscreen color buffer at the logical size, plus what is needed to draw it rotated.
struct RotatedTarget {
    fbo: glow::Framebuffer,
    texture: glow::Texture,
    program: glow::Program,
    u_tex: Option<glow::UniformLocation>,
    vbo: glow::Buffer,
    panel_size: [u32; 2],
}

impl RotatedTarget {
    unsafe fn new(
        gl: &glow::Context,
        rotation: Rotation,
        panel_size: [u32; 2],
    ) -> Result<Self, Error> {
        let [w, h] = rotation.logical_size(panel_size);
        unsafe {
            let texture = gl.create_texture().map_err(Error::Gl)?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // 1:1 texel to pixel mapping, so NEAREST is an exact copy.
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
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
                glow::RGBA as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);

            let fbo = gl.create_framebuffer().map_err(Error::Gl)?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(texture);
                return Err(Error::Gl(format!(
                    "rotation framebuffer {w}x{h} is incomplete (status {status:#x})"
                )));
            }

            let program = link_blit_program(gl)?;
            let u_tex = gl.get_uniform_location(program, "u_tex");

            let positions = [[-1.0f32, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]];
            let tex_coords = blit_tex_coords(rotation);
            let vertices: Vec<f32> = positions
                .iter()
                .zip(tex_coords)
                .flat_map(|(p, t)| [p[0], p[1], t[0], t[1]])
                .collect();
            let bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
            let vbo = gl.create_buffer().map_err(Error::Gl)?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            Ok(Self {
                fbo,
                texture,
                program,
                u_tex,
                vbo,
                panel_size,
            })
        }
    }

    /// Draws the offscreen image rotated onto the default framebuffer.
    unsafe fn blit(&self, gl: &glow::Context) {
        let [w, h] = self.panel_size;
        let stride = 4 * size_of::<f32>() as i32;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);

            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.uniform_1_i32(self.u_tex.as_ref(), 0);

            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.enable_vertex_attrib_array(A_POS);
            gl.vertex_attrib_pointer_f32(A_POS, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(A_TC);
            gl.vertex_attrib_pointer_f32(A_TC, 2, glow::FLOAT, false, stride, 2 * 4);

            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            gl.disable_vertex_attrib_array(A_POS);
            gl.disable_vertex_attrib_array(A_TC);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
    }

    unsafe fn destroy(self, gl: &glow::Context) {
        unsafe {
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.texture);
            gl.delete_program(self.program);
            gl.delete_buffer(self.vbo);
        }
    }
}

unsafe fn link_blit_program(gl: &glow::Context) -> Result<glow::Program, Error> {
    unsafe {
        let program = gl.create_program().map_err(Error::Gl)?;
        let mut shaders = Vec::new();
        for (kind, src) in [
            (glow::VERTEX_SHADER, BLIT_VERT),
            (glow::FRAGMENT_SHADER, BLIT_FRAG),
        ] {
            let shader = gl.create_shader(kind).map_err(Error::Gl)?;
            gl.shader_source(shader, src);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                gl.delete_program(program);
                return Err(Error::Gl(format!("blit shader: {log}")));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.bind_attrib_location(program, A_POS, "a_pos");
        gl.bind_attrib_location(program, A_TC, "a_tc");
        gl.link_program(program);
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(Error::Gl(format!("blit program: {log}")));
        }
        Ok(program)
    }
}
