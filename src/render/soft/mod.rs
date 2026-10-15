//! CPU renderer: rasterizes egui meshes into an RGBA canvas. A platform (e.g. `linuxfb`) then
//! copies the canvas to the screen. Like Qt's raster paint engine on `linuxfb`, it needs no GPU.

pub(crate) mod blit;
mod raster;

use std::collections::HashMap;

use egui::epaint::Primitive;
use egui::{ImageData, TextureId, TextureOptions};

use super::Renderer;
use raster::ClipRect;

/// Premultiplied sRGBA pixels, row-major, no padding.
pub(crate) struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<[u8; 4]>,
}

pub(crate) struct Texture {
    pub width: usize,
    pub height: usize,
    /// Premultiplied sRGBA, like `egui::Color32`.
    pub pixels: Vec<[u8; 4]>,
    pub options: TextureOptions,
}

pub(crate) struct SoftRenderer {
    canvas: Canvas,
    textures: HashMap<TextureId, Texture>,
    warned_callback: bool,
    warned_missing_texture: bool,
}

impl SoftRenderer {
    pub(crate) fn new() -> Self {
        Self {
            canvas: Canvas {
                width: 0,
                height: 0,
                pixels: Vec::new(),
            },
            textures: HashMap::new(),
            warned_callback: false,
            warned_missing_texture: false,
        }
    }

    /// The last painted frame, at the logical (unrotated) size.
    pub(crate) fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    fn set_texture(&mut self, id: TextureId, delta: &egui::epaint::ImageDelta) {
        let ImageData::Color(image) = &delta.image;
        let [w, h] = image.size;
        let pixels = image.pixels.iter().map(|c| c.to_array());
        match delta.pos {
            None => {
                self.textures.insert(
                    id,
                    Texture {
                        width: w,
                        height: h,
                        pixels: pixels.collect(),
                        options: delta.options,
                    },
                );
            }
            Some([x, y]) => {
                let Some(texture) = self.textures.get_mut(&id) else {
                    log::warn!("Partial update of unknown texture {id:?}");
                    return;
                };
                texture.options = delta.options;
                let patch: Vec<[u8; 4]> = pixels.collect();
                for (row, src) in patch.chunks_exact(w.max(1)).enumerate() {
                    let ty = y + row;
                    if ty >= texture.height || x >= texture.width {
                        continue;
                    }
                    let n = w.min(texture.width - x);
                    let start = ty * texture.width + x;
                    texture.pixels[start..start + n].copy_from_slice(&src[..n]);
                }
            }
        }
    }
}

impl Renderer for SoftRenderer {
    fn max_texture_side(&self) -> usize {
        // No hardware limit; this only bounds the font atlas and user images.
        8192
    }

    fn paint(
        &mut self,
        [width, height]: [u32; 2],
        pixels_per_point: f32,
        clear_color: [f32; 4],
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &mut egui::TexturesDelta,
    ) {
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order of different textures does not matter"
        )]
        for (id, deltas) in textures_delta.set.drain() {
            for delta in deltas {
                self.set_texture(id, &delta);
            }
        }

        let (width, height) = (width as usize, height as usize);
        let canvas = &mut self.canvas;
        if canvas.width != width || canvas.height != height {
            *canvas = Canvas {
                width,
                height,
                pixels: vec![[0; 4]; width * height],
            };
        }
        let clear = clear_color.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
        canvas.pixels.fill(clear);

        for egui::ClippedPrimitive {
            clip_rect,
            primitive,
        } in primitives
        {
            let clip = clip_to_pixels(*clip_rect, pixels_per_point, [width, height]);
            if clip.x0 >= clip.x1 || clip.y0 >= clip.y1 {
                continue;
            }
            match primitive {
                Primitive::Mesh(mesh) => match self.textures.get(&mesh.texture_id) {
                    Some(texture) => {
                        raster::draw_mesh(canvas, clip, pixels_per_point, mesh, texture)
                    }
                    None => {
                        if !self.warned_missing_texture {
                            self.warned_missing_texture = true;
                            log::warn!("Mesh uses unknown texture {:?}", mesh.texture_id);
                        }
                    }
                },
                Primitive::Callback(_) => {
                    if !self.warned_callback {
                        self.warned_callback = true;
                        log::warn!("Paint callbacks are not supported by the software renderer");
                    }
                }
            }
        }

        #[expect(clippy::iter_over_hash_type, reason = "order does not matter")]
        for id in textures_delta.free.drain() {
            self.textures.remove(&id);
        }
    }

    fn destroy(&mut self) {
        self.textures.clear();
    }
}

/// Same rounding as egui_glow's scissor rectangle.
fn clip_to_pixels(
    clip: egui::Rect,
    pixels_per_point: f32,
    [width, height]: [usize; 2],
) -> ClipRect {
    let px = |v: f32, max: usize| (v * pixels_per_point).round().clamp(0.0, max as f32) as i64;
    ClipRect {
        x0: px(clip.min.x, width),
        y0: px(clip.min.y, height),
        x1: px(clip.max.x, width),
        y1: px(clip.max.y, height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paints_a_frame_from_egui() {
        let ctx = egui::Context::default();
        let mut renderer = SoftRenderer::new();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(200.0, 100.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label("Hello");
                let _ = ui.button("Button");
            });
        });
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let mut textures = output.textures_delta;
        renderer.paint(
            [200, 100],
            1.0,
            [0.0, 0.0, 0.0, 1.0],
            &primitives,
            &mut textures,
        );

        let canvas = renderer.canvas();
        assert_eq!((canvas.width, canvas.height), (200, 100));
        let panel = ctx.global_style().visuals.panel_fill.to_array();
        let distinct: std::collections::HashSet<[u8; 4]> = canvas.pixels.iter().copied().collect();
        assert!(distinct.contains(&panel), "panel background missing");
        // Text and the button add more colors (anti-aliased glyph edges).
        assert!(distinct.len() > 10, "only {} colors drawn", distinct.len());
    }

    /// Renders a typical UI to `target/soft_snapshot.png` for a visual check:
    /// `cargo test soft_snapshot -- --ignored`.
    #[test]
    #[ignore = "writes a file for manual inspection"]
    fn soft_snapshot() {
        let ctx = egui::Context::default();
        let size = egui::vec2(480.0, 320.0);
        let ppp = 1.25;
        let input = |time| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size / ppp)),
            time: Some(time),
            viewports: std::iter::once((
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    native_pixels_per_point: Some(ppp),
                    ..Default::default()
                },
            ))
            .collect(),
            ..Default::default()
        };
        let mut renderer = SoftRenderer::new();
        let (mut checked, mut value) = (true, 0.4);
        // Two passes: the first one only measures text.
        for time in [0.0, 0.1] {
            let output = ctx.run_ui(input(time), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("Software renderer");
                    ui.label("The quick brown fox jumps over the lazy dog.");
                    ui.horizontal(|ui| {
                        let _ = ui.button("Button");
                        ui.checkbox(&mut checked, "Checkbox");
                    });
                    ui.add(egui::Slider::new(&mut value, 0.0..=1.0).text("Slider"));
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(200.0, 60.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        rect.left_center() + egui::vec2(30.0, 0.0),
                        25.0,
                        egui::Color32::from_rgb(200, 80, 40),
                    );
                    ui.painter().line_segment(
                        [rect.left_top() + egui::vec2(70.0, 0.0), rect.right_bottom()],
                        egui::Stroke::new(2.0, egui::Color32::LIGHT_BLUE),
                    );
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(rect.center(), egui::vec2(60.0, 25.0)),
                        6.0,
                        egui::Color32::from_rgba_unmultiplied(0, 200, 0, 128),
                    );
                });
            });
            let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
            let mut textures = output.textures_delta;
            let clear = ctx
                .global_style()
                .visuals
                .panel_fill
                .to_normalized_gamma_f32();
            renderer.paint([480, 320], ppp, clear, &primitives, &mut textures);
        }

        let canvas = renderer.canvas();
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/soft_snapshot.png");
        std::fs::write(&path, png::encode(canvas)).unwrap();
        println!("wrote {}", path.display());
    }

    /// Minimal uncompressed PNG writer, enough for test snapshots.
    mod png {
        use super::Canvas;

        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in data {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }

        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend(kind);
            out.extend(data);
            let crc = crc32(&out[start..]);
            out.extend(crc.to_be_bytes());
        }

        pub(super) fn encode(canvas: &Canvas) -> Vec<u8> {
            let mut raw = Vec::new();
            for row in canvas.pixels.chunks_exact(canvas.width) {
                raw.push(0); // filter: none
                raw.extend(row.iter().flat_map(|p| [p[0], p[1], p[2]]));
            }
            // zlib stream of "stored" deflate blocks.
            let mut zlib = vec![0x78, 0x01];
            let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
            for (i, block) in blocks.iter().enumerate() {
                zlib.push(u8::from(i + 1 == blocks.len()));
                let len = block.len() as u16;
                zlib.extend(len.to_le_bytes());
                zlib.extend((!len).to_le_bytes());
                zlib.extend(*block);
            }
            let (mut a, mut b) = (1u32, 0u32);
            for &byte in &raw {
                a = (a + u32::from(byte)) % 65_521;
                b = (b + a) % 65_521;
            }
            zlib.extend(((b << 16) | a).to_be_bytes());

            let mut header = Vec::new();
            header.extend((canvas.width as u32).to_be_bytes());
            header.extend((canvas.height as u32).to_be_bytes());
            header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
            let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
            chunk(&mut out, b"IHDR", &header);
            chunk(&mut out, b"IDAT", &zlib);
            chunk(&mut out, b"IEND", &[]);
            out
        }
    }
}
