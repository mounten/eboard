//! CPU renderer: rasterizes egui meshes into an RGBA canvas. A platform (e.g. `linuxfb`) then
//! copies the canvas to the screen. Like Qt's raster paint engine on `linuxfb`, it needs no GPU.
//!
//! The canvas keeps the last frame. Each frame's shapes (runs of triangles) are compared with the
//! last frame's, and only the rectangles around the shapes that differ are cleared and drawn
//! again.

pub(crate) mod blit;
mod diff;
mod raster;

use std::collections::HashMap;
use std::ops::RangeInclusive;

use egui::epaint::{Primitive, Vertex};
use egui::{ImageData, TextureId, TextureOptions};

use super::Renderer;
use blit::PixelRect;
use raster::ClipRect;

/// Most rectangles redrawn per frame. Each one goes over all shapes again.
const MAX_REDRAW_RECTS: usize = 4;
/// Shapes added or removed before the diff gives up and everything between the first and the
/// last change counts as changed. The diff takes up to about this squared steps.
const MAX_EDITS: usize = 128;

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

/// A run of a mesh's triangles that only use each other's vertices. egui appends each shape's
/// vertices and triangles to the mesh in turn, so this is usually one egui shape (a circle, a
/// glyph, a rounded rect). Frames are compared by these.
#[derive(Clone, Copy)]
struct Shape {
    /// Of everything that decides its pixels: clip rectangle, texture, vertices and triangles.
    hash: u64,
    /// The pixels it may cover, inside the clip rectangle.
    bounds: ClipRect,
    /// Number of index triples.
    triangles: usize,
    /// It may sample texels that changed in the frame it is from.
    texture_changed: bool,
}

impl Shape {
    /// `next` (from this frame) draws the same pixels as `self` did, given the same canvas size
    /// and pixels per point. Equal hashes count as equal shapes: with 64 bits, a false match
    /// (which would leave a stale shape on screen until it changes again) is not a concern.
    fn same(&self, next: &Self) -> bool {
        self.hash == next.hash && !next.texture_changed
    }
}

/// FxHash-like mixing: each step is a bijection of the state, so inputs that differ in one word
/// never collide. Finished with MurmurHash3's finalizer.
#[derive(Clone, Copy)]
struct Hasher(u64);

impl Hasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    #[inline]
    fn finish(self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

/// Starts a shape hash with what all shapes of a mesh share.
fn mesh_hasher(clip: ClipRect, texture: TextureId) -> Hasher {
    let mut h = Hasher(0);
    for v in [clip.x0, clip.y0, clip.x1, clip.y1] {
        h.add(v as u64);
    }
    match texture {
        TextureId::Managed(id) => h.add(id << 1),
        TextureId::User(id) => h.add((id << 1) | 1),
    }
    h
}

/// Hashes the triangles (indices relative to `first_vertex`) and the vertices they use.
fn shape_hash(
    mut h: Hasher,
    triangles: &[[u32; 3]],
    first_vertex: u32,
    vertices: &[Vertex],
) -> u64 {
    for t in triangles {
        let [a, b, c] = t.map(|i| u64::from(i.wrapping_sub(first_vertex)));
        h.add(a | (b << 32));
        h.add(c);
    }
    let pair = |a: f32, b: f32| u64::from(a.to_bits()) | (u64::from(b.to_bits()) << 32);
    for v in vertices {
        h.add(pair(v.pos.x, v.pos.y));
        h.add(pair(v.uv.x, v.uv.y));
        h.add(u64::from(u32::from_le_bytes(v.color.to_array())));
    }
    h.add(triangles.len() as u64);
    h.finish()
}

/// Splits a mesh's triangles into shapes: a shape ends before a triangle whose vertices all come
/// after the ones used so far. Yields the number of triangles and the vertex index range.
fn split_shapes(triangles: &[[u32; 3]]) -> impl Iterator<Item = (usize, RangeInclusive<u32>)> {
    let mut rest = triangles;
    std::iter::from_fn(move || {
        let (first, others) = rest.split_first()?;
        let (mut lo, mut hi) = (min3(first), max3(first));
        let len = 1 + others
            .iter()
            .take_while(|t| {
                let starts_shape = min3(t) > hi;
                if !starts_shape {
                    lo = lo.min(min3(t));
                    hi = hi.max(max3(t));
                }
                !starts_shape
            })
            .count();
        rest = &rest[len..];
        Some((len, lo..=hi))
    })
}

fn min3(t: &[u32; 3]) -> u32 {
    t[0].min(t[1]).min(t[2])
}

fn max3(t: &[u32; 3]) -> u32 {
    t[0].max(t[1]).max(t[2])
}

/// The meshes that can be drawn, with their clip rectangles in pixels.
fn meshes(
    primitives: &[egui::ClippedPrimitive],
    pixels_per_point: f32,
    size: [usize; 2],
) -> impl Iterator<Item = (ClipRect, &egui::Mesh)> {
    primitives.iter().filter_map(move |p| {
        let clip = clip_to_pixels(p.clip_rect, pixels_per_point, size);
        match &p.primitive {
            Primitive::Mesh(mesh) if !clip.is_empty() => Some((clip, mesh)),
            _ => None,
        }
    })
}

/// A triangle's vertices, `None` for out-of-range indices.
fn triangle<'a>(mesh: &'a egui::Mesh, indices: &[u32; 3]) -> Option<[&'a Vertex; 3]> {
    let vertex = |i: u32| mesh.vertices.get(i as usize);
    Some([
        vertex(indices[0])?,
        vertex(indices[1])?,
        vertex(indices[2])?,
    ])
}

/// How a texture changed in this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextureChange {
    /// New, replaced, freed or different options: anything using it may look different.
    All,
    /// Only these texels changed (e.g. glyphs added to the font atlas).
    Texels(ClipRect),
}

impl TextureChange {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Texels(a), Self::Texels(b)) => Self::Texels(a.union(b)),
            _ => Self::All,
        }
    }

    /// Whether triangles between these vertices may sample a changed texel.
    fn affects(self, vertices: &[Vertex], texture: &Texture) -> bool {
        let Self::Texels(changed) = self else {
            return true;
        };
        let mut min = egui::Pos2::new(f32::INFINITY, f32::INFINITY);
        let mut max = egui::Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
        for v in vertices {
            if !v.uv.x.is_finite() || !v.uv.y.is_finite() {
                return true;
            }
            min = min.min(v.uv);
            max = max.max(v.uv);
        }
        if vertices.is_empty() {
            return false;
        }
        let inside = |c: f32| (0.0..=1.0).contains(&c);
        if texture.options.wrap_mode != egui::TextureWrapMode::ClampToEdge
            && !(inside(min.x) && inside(min.y) && inside(max.x) && inside(max.y))
        {
            return true;
        }
        // One texel more on each side for linear filtering; clamped like the sampler.
        let span = |lo: f32, hi: f32, size: usize| {
            let n = size as i64;
            (
                ((lo * size as f32).floor() as i64 - 1).clamp(0, n - 1),
                ((hi * size as f32).ceil() as i64 + 1).clamp(1, n),
            )
        };
        let (x0, x1) = span(min.x, max.x, texture.width);
        let (y0, y1) = span(min.y, max.y, texture.height);
        ClipRect { x0, y0, x1, y1 }.near(changed, 0)
    }
}

pub(crate) struct SoftRenderer {
    canvas: Canvas,
    textures: HashMap<TextureId, Texture>,
    /// The canvas shows these, drawn at `pixels_per_point` over `clear`.
    shapes: Vec<Shape>,
    pixels_per_point: f32,
    clear: [u8; 4],
    /// This frame's shapes.
    next: Vec<Shape>,
    /// Textures changed by this frame's `TexturesDelta`.
    texture_changes: Vec<(TextureId, TextureChange)>,
    /// Canvas areas changed since the last `take_damage`.
    damage: Vec<PixelRect>,
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
            shapes: Vec::new(),
            pixels_per_point: 0.0,
            clear: [0; 4],
            next: Vec::new(),
            texture_changes: Vec::new(),
            damage: Vec::new(),
            warned_callback: false,
            warned_missing_texture: false,
        }
    }

    /// The last painted frame, at the logical (unrotated) size.
    pub(crate) fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// The canvas areas painted since the last call. Pixels outside them did not change.
    pub(crate) fn take_damage(&mut self) -> Vec<PixelRect> {
        std::mem::take(&mut self.damage)
    }

    /// Collects the shapes of `primitives` into `self.next`: the meshes from [`meshes`] with a
    /// known texture, in the order [`Self::redraw`] goes through them.
    fn collect_shapes(&mut self, pixels_per_point: f32, primitives: &[egui::ClippedPrimitive]) {
        if !self.warned_callback
            && primitives
                .iter()
                .any(|p| matches!(p.primitive, Primitive::Callback(_)))
        {
            self.warned_callback = true;
            log::warn!("Paint callbacks are not supported by the software renderer");
        }
        let size = [self.canvas.width, self.canvas.height];
        let next = &mut self.next;
        next.clear();
        for (clip, mesh) in meshes(primitives, pixels_per_point, size) {
            let Some(texture) = self.textures.get(&mesh.texture_id) else {
                if !self.warned_missing_texture {
                    self.warned_missing_texture = true;
                    log::warn!("Mesh uses unknown texture {:?}", mesh.texture_id);
                }
                continue;
            };
            let change = self
                .texture_changes
                .iter()
                .find(|(id, _)| *id == mesh.texture_id)
                .map(|&(_, change)| change);
            let hasher = mesh_hasher(clip, mesh.texture_id);
            let mut triangles = mesh.indices.as_chunks::<3>().0;
            for (len, used) in split_shapes(triangles) {
                let (shape, rest) = triangles.split_at(len);
                triangles = rest;
                let all = mesh.vertices.len();
                let start = (*used.start() as usize).min(all);
                let vertices = &mesh.vertices[start..(*used.end() as usize + 1).min(all)];
                next.push(Shape {
                    hash: shape_hash(hasher, shape, *used.start(), vertices),
                    bounds: raster::vertex_bounds(vertices, pixels_per_point)
                        .map_or(clip, |b| b.intersect(clip)),
                    triangles: len,
                    texture_changed: change.is_some_and(|c| c.affects(vertices, texture)),
                });
            }
        }
    }

    /// The canvas areas that differ between the drawn shapes and `self.next`.
    fn changed_areas(&self) -> Vec<ClipRect> {
        match diff::diff(&self.shapes, &self.next, Shape::same, MAX_EDITS) {
            diff::Diff::Edits(edits) => {
                let removed = edits.removed.iter().map(|&i| self.shapes[i].bounds);
                let added = edits.added.iter().map(|&i| self.next[i].bounds);
                diff::merge_rects(removed.chain(added), MAX_REDRAW_RECTS)
            }
            diff::Diff::Ranges { old, new } => {
                let shapes = self.shapes[old].iter().chain(&self.next[new]);
                let all = shapes
                    .map(|s| s.bounds)
                    .filter(|b| !b.is_empty())
                    .reduce(ClipRect::union);
                all.into_iter().collect()
            }
        }
    }

    /// Clears `area` and draws the shapes of `primitives` (collected in `self.next`) that reach
    /// into it.
    fn redraw(
        &mut self,
        area: ClipRect,
        pixels_per_point: f32,
        primitives: &[egui::ClippedPrimitive],
    ) {
        let canvas = &mut self.canvas;
        let width = canvas.width;
        for y in area.y0 as usize..area.y1 as usize {
            canvas.pixels[y * width + area.x0 as usize..y * width + area.x1 as usize]
                .fill(self.clear);
        }
        let size = [canvas.width, canvas.height];
        let mut shapes = self.next.iter();
        for (clip, mesh) in meshes(primitives, pixels_per_point, size) {
            let Some(texture) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = clip.intersect(area);
            let mut triangles = mesh.indices.as_chunks::<3>().0;
            while !triangles.is_empty() {
                let Some(shape) = shapes.next() else {
                    return; // collected from the same primitives, can't happen
                };
                let (run, rest) = triangles.split_at(shape.triangles.min(triangles.len()));
                triangles = rest;
                if !shape.bounds.near(area, 0) {
                    continue;
                }
                for vertices in run.iter().filter_map(|t| triangle(mesh, t)) {
                    raster::draw_triangle(canvas, clip, pixels_per_point, vertices, texture);
                }
            }
        }
    }

    fn texture_changed(&mut self, id: TextureId, change: TextureChange) {
        match self.texture_changes.iter_mut().find(|(i, _)| *i == id) {
            Some((_, c)) => *c = c.merge(change),
            None => self.texture_changes.push((id, change)),
        }
    }

    fn set_texture(&mut self, id: TextureId, delta: &egui::epaint::ImageDelta) {
        let ImageData::Color(image) = &delta.image;
        let [w, h] = image.size;
        let pixels = image.pixels.iter().map(|c| c.to_array());
        match delta.pos {
            None => {
                self.texture_changed(id, TextureChange::All);
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
                // egui sends the font atlas area around new glyphs, old ones included: only
                // texels that really change count.
                let mut change = (texture.options != delta.options).then_some(TextureChange::All);
                texture.options = delta.options;
                let patch: Vec<[u8; 4]> = pixels.collect();
                for (row, src) in patch.chunks_exact(w.max(1)).enumerate() {
                    let ty = y + row;
                    if ty >= texture.height || x >= texture.width {
                        continue;
                    }
                    let n = w.min(texture.width - x);
                    let start = ty * texture.width + x;
                    let dst = &mut texture.pixels[start..start + n];
                    let differs = |i: &usize| dst[*i] != src[*i];
                    if let (Some(first), Some(last)) =
                        ((0..n).find(differs), (0..n).rev().find(differs))
                    {
                        let texels = TextureChange::Texels(ClipRect {
                            x0: (x + first) as i64,
                            y0: ty as i64,
                            x1: (x + last + 1) as i64,
                            y1: ty as i64 + 1,
                        });
                        change = Some(change.map_or(texels, |c| c.merge(texels)));
                    }
                    dst.copy_from_slice(&src[..n]);
                }
                if let Some(change) = change {
                    self.texture_changed(id, change);
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
        // Shapes using these may look different with the same vertices.
        self.texture_changes.clear();
        for &id in &textures_delta.free {
            self.texture_changed(id, TextureChange::All);
        }
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
        let clear = clear_color.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
        let full = ClipRect {
            x0: 0,
            y0: 0,
            x1: width as i64,
            y1: height as i64,
        };
        let redraw_all = self.canvas.width != width
            || self.canvas.height != height
            || self.pixels_per_point != pixels_per_point
            || self.clear != clear;
        if self.canvas.width != width || self.canvas.height != height {
            self.canvas = Canvas {
                width,
                height,
                pixels: vec![[0; 4]; width * height],
            };
        }
        self.clear = clear;
        self.pixels_per_point = pixels_per_point;

        self.collect_shapes(pixels_per_point, primitives);
        let mut areas = if redraw_all {
            vec![full]
        } else {
            self.changed_areas()
        };
        // Past half the screen, one pass over the shapes beats several.
        if areas.iter().map(|a| a.area()).sum::<i64>() > full.area() / 2 {
            areas = vec![full];
        }
        for &area in &areas {
            self.redraw(area, pixels_per_point, primitives);
        }
        self.damage.extend(areas.iter().map(|a| PixelRect {
            x0: a.x0 as usize,
            y0: a.y0 as usize,
            x1: a.x1 as usize,
            y1: a.y1 as usize,
        }));
        std::mem::swap(&mut self.shapes, &mut self.next);

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

// These tests render text, so they need egui's fonts.
#[cfg(all(test, feature = "default_fonts"))]
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

    #[test]
    fn partial_texture_updates_track_changed_texels() {
        let mut renderer = SoftRenderer::new();
        let image = |size: [usize; 2], gray: u8| {
            egui::ImageData::Color(std::sync::Arc::new(egui::ColorImage::new(
                size,
                vec![egui::Color32::from_gray(gray); size[0] * size[1]],
            )))
        };
        let id = TextureId::Managed(0);
        let options = TextureOptions::LINEAR;
        renderer.set_texture(
            id,
            &egui::epaint::ImageDelta::full(image([8, 8], 0), options),
        );
        assert_eq!(renderer.texture_changes, vec![(id, TextureChange::All)]);

        // Resending texels unchanged is no change.
        renderer.texture_changes.clear();
        let same = egui::epaint::ImageDelta::partial([2, 2], image([4, 4], 0), options);
        renderer.set_texture(id, &same);
        assert_eq!(renderer.texture_changes, vec![]);

        let mut patch = egui::ColorImage::new([4, 2], vec![egui::Color32::BLACK; 8]);
        patch[(3, 1)] = egui::Color32::WHITE;
        let delta = egui::epaint::ImageDelta::partial(
            [2, 2],
            egui::ImageData::Color(std::sync::Arc::new(patch)),
            options,
        );
        renderer.set_texture(id, &delta);
        let texel = ClipRect {
            x0: 5,
            y0: 3,
            x1: 6,
            y1: 4,
        };
        assert_eq!(
            renderer.texture_changes,
            vec![(id, TextureChange::Texels(texel))]
        );

        // Triangles sampling next to it are affected (linear filtering), others are not.
        let texture = &renderer.textures[&id];
        let triangle = |u0: f32, v0: f32| {
            let uv = |du: f32, dv: f32| Vertex {
                pos: egui::Pos2::ZERO,
                uv: egui::pos2((u0 + du) / 8.0, (v0 + dv) / 8.0),
                color: egui::Color32::WHITE,
            };
            [uv(0.0, 0.0), uv(1.0, 0.0), uv(0.0, 1.0)]
        };
        let change = TextureChange::Texels(texel);
        assert!(change.affects(&triangle(4.0, 3.0), texture));
        assert!(!change.affects(&triangle(0.0, 0.0), texture));
        assert!(TextureChange::All.affects(&triangle(0.0, 0.0), texture));
    }

    /// Redrawing only what changed must give exactly the pixels of a full redraw.
    #[test]
    fn partial_redraw_matches_full_redraw() {
        let ctx = egui::Context::default();
        let ppp = 1.25;
        let size = [400, 300];
        let frame = |count: u32, time: f64| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(size[0] as f32, size[1] as f32) / ppp,
                )),
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
            let output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("Partial redraw");
                    ui.label(format!("Count {count}"));
                    let _ = ui.button("Button");
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(250.0, 60.0), egui::Sense::hover());
                    let x = rect.left() + 30.0 + 20.0 * count as f32;
                    ui.painter().circle_filled(
                        egui::pos2(x, rect.center().y),
                        25.0,
                        egui::Color32::from_rgba_unmultiplied(200, 80, 40, 160),
                    );
                    ui.label("The quick brown fox jumps over the lazy dog.");
                    if count >= 9 {
                        ui.label("Ω"); // a new glyph in the font atlas
                    }
                });
            });
            (
                ctx.tessellate(output.shapes, output.pixels_per_point),
                output.textures_delta,
            )
        };
        let clear = [0.1, 0.1, 0.1, 1.0];

        let mut partial = SoftRenderer::new();
        let mut full = SoftRenderer::new();
        // The first pass only measures text.
        for (count, time) in [(0, 0.0), (1, 0.1)] {
            let (primitives, mut delta) = frame(count, time);
            partial.paint(size, ppp, clear, &primitives, &mut delta.clone());
            full.paint(size, ppp, clear, &primitives, &mut delta);
        }
        let _ = partial.take_damage();
        for (count, time) in [(2, 0.2), (2, 0.3), (5, 0.4), (9, 0.5)] {
            let (primitives, mut delta) = frame(count, time);
            partial.paint(size, ppp, clear, &primitives, &mut delta.clone());
            full.pixels_per_point = 0.0; // forces a full redraw
            full.paint(size, ppp, clear, &primitives, &mut delta);
            assert!(
                partial.canvas.pixels == full.canvas.pixels,
                "count {count}: partial redraw differs"
            );
            // Glyphs added to the font atlas (new characters, or new subpixel offsets) don't make
            // all text count as changed.
            let damage = partial.take_damage();
            let area: usize = damage.iter().map(|r| (r.x1 - r.x0) * (r.y1 - r.y0)).sum();
            assert!(area < (size[0] * size[1] / 4) as usize, "{damage:?}");
            if time == 0.3 {
                assert_eq!(damage, vec![], "nothing changed");
            }
        }
    }

    /// Overlay HUDs (see `examples/hud.rs`) rely on every pixel outside the UI being exactly the
    /// clear color, down to the framebuffer, so a keyer can replace it with video. With
    /// feathering off, shape edges are not blended with it either.
    #[test]
    fn clear_color_is_exact_outside_the_ui() {
        let key = egui::Color32::from_rgb(0, 255, 0);
        let ctx = egui::Context::default();
        ctx.tessellation_options_mut(|o| o.feathering = false);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(200.0, 100.0),
            )),
            ..Default::default()
        };
        let mut plate = egui::Rect::NOTHING;
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    plate = egui::Frame::NONE
                        .fill(egui::Color32::from_gray(20))
                        .inner_margin(8.0)
                        .show(ui, |ui| ui.label("HUD"))
                        .response
                        .rect;
                    let painter = ui.painter();
                    painter.circle_filled(egui::pos2(150.0, 40.0), 20.0, egui::Color32::WHITE);
                    painter.line_segment(
                        [egui::pos2(90.0, 95.0), egui::pos2(195.0, 70.0)],
                        egui::Stroke::new(3.0, egui::Color32::WHITE),
                    );
                });
        });
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let mut textures = output.textures_delta;
        let mut renderer = SoftRenderer::new();
        renderer.paint(
            [200, 100],
            1.0,
            key.to_normalized_gamma_f32(),
            &primitives,
            &mut textures,
        );

        let canvas = renderer.canvas();
        let (x0, y0) = (plate.min.x.floor() as usize, plate.min.y.floor() as usize);
        let (x1, y1) = (plate.max.x.ceil() as usize, plate.max.y.ceil() as usize);
        assert!(
            x1 > x0 && y1 > y0 && x1 < 200 && y1 < 100,
            "plate {plate:?}"
        );
        let (mut keyed, mut white) = (0, 0);
        for (i, &pixel) in canvas.pixels.iter().enumerate() {
            let (x, y) = (i % canvas.width, i / canvas.width);
            if (x0..x1).contains(&x) && (y0..y1).contains(&y) {
                continue;
            }
            if pixel == key.to_array() {
                keyed += 1;
            } else {
                assert_eq!(pixel, [255; 4], "pixel ({x}, {y}) is blended with the key");
                white += 1;
            }
        }
        assert!(
            keyed > 200 * 100 / 2 && white > 1000,
            "{keyed} keyed, {white} white"
        );

        // Exact in the framebuffer formats too.
        for (format, expected) in [
            (blit::PixelFormat::XRGB8888, 0x0000_ff00),
            (blit::PixelFormat::RGB565, 0x07e0),
        ] {
            let bpp = format.bytes_per_pixel;
            let mut memory = vec![0u8; 200 * 100 * bpp];
            let mut target = blit::Target {
                memory: &mut memory,
                stride: 200 * bpp,
                format,
                size: [200, 100],
            };
            blit::blit(
                &canvas.pixels,
                canvas.width,
                crate::Rotation::Deg0,
                blit::PixelRect::full([200, 100]),
                &mut target,
                &mut Vec::new(),
            );
            let corner = &memory[(99 * 200 + 199) * bpp..][..bpp];
            let mut value = [0u8; 4];
            value[..bpp].copy_from_slice(corner);
            assert_eq!(u32::from_le_bytes(value), expected, "{}", format.name());
        }
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
}

/// Minimal uncompressed PNG writer, enough for test snapshots.
#[cfg(all(test, feature = "default_fonts"))]
pub(crate) mod png {
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

    pub(crate) fn encode(canvas: &Canvas) -> Vec<u8> {
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
