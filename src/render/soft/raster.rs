//! Triangle rasterizer for epaint meshes.
//!
//! Matches egui_glow's output: vertex colors and texels are premultiplied sRGBA and are multiplied
//! and blended in gamma space (`dst = src + dst * (1 - src.a)`).
//!
//! Vertex positions are snapped to 1/16 pixel and edge functions are evaluated in integers, so
//! triangles that share an edge (egui meshes are full of them) cover every pixel exactly once
//! (top-left fill rule). Seams or double-blended edges would show on translucent shapes.

use egui::epaint::{Mesh, Vertex};
use egui::{TextureFilter, TextureWrapMode};

use super::{Canvas, Texture};

const SUBPIXEL: i64 = 16;
const HALF_PIXEL: i64 = SUBPIXEL / 2;
/// Keeps edge-function products well inside `i64` for absurd off-screen coordinates.
const COORD_LIMIT: f32 = (1 << 27) as f32;

/// Half-open pixel rectangle `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipRect {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
}

pub(crate) fn draw_mesh(
    canvas: &mut Canvas,
    clip: ClipRect,
    pixels_per_point: f32,
    mesh: &Mesh,
    texture: &Texture,
) {
    for indices in mesh.indices.as_chunks::<3>().0 {
        let vertex = |i: u32| mesh.vertices.get(i as usize);
        if let (Some(a), Some(b), Some(c)) =
            (vertex(indices[0]), vertex(indices[1]), vertex(indices[2]))
        {
            draw_triangle(canvas, clip, pixels_per_point, [a, b, c], texture);
        }
    }
}

type Point = (i64, i64);

fn to_subpixel(v: f32, pixels_per_point: f32) -> i64 {
    // NaN becomes 0 through the saturating cast.
    (v * pixels_per_point * SUBPIXEL as f32)
        .clamp(-COORD_LIMIT, COORD_LIMIT)
        .round() as i64
}

/// Twice the signed area of `(a, b, p)`; positive when `p` is on the inner side of `a → b` for
/// triangles with positive area (y pointing down).
fn edge_function(a: Point, b: Point, p: Point) -> i64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Top-left fill rule: pixels exactly on a top or left edge belong to the triangle.
fn is_top_left(a: Point, b: Point) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    (dy == 0 && dx > 0) || dy < 0
}

fn div_floor(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}

fn div_ceil(a: i64, b: i64) -> i64 {
    -(-a).div_euclid(b)
}

/// One edge, evaluated at pixel centers: `w(x, y) = w_origin + x * step_x + y * step_y`, inside
/// when `w >= threshold`.
#[derive(Clone, Copy)]
struct Edge {
    w_origin: i64,
    step_x: i64,
    step_y: i64,
    threshold: i64,
}

impl Edge {
    fn new(a: Point, b: Point, origin: Point) -> Self {
        Self {
            w_origin: edge_function(a, b, origin),
            step_x: -(b.1 - a.1) * SUBPIXEL,
            step_y: (b.0 - a.0) * SUBPIXEL,
            threshold: if is_top_left(a, b) { 0 } else { 1 },
        }
    }

    /// Narrows `[lo, hi)` (offsets from the origin column) to where this edge is inside, given
    /// the edge value `w` at offset 0 of the current row.
    fn clip_span(&self, w: i64, lo: &mut i64, hi: &mut i64) {
        let need = self.threshold - w; // inside when k * step_x >= need
        match self.step_x {
            0 => {
                if need > 0 {
                    *hi = *lo;
                }
            }
            s if s > 0 => *lo = (*lo).max(div_ceil(need, s)),
            s => *hi = (*hi).min(div_floor(-need, -s) + 1),
        }
    }
}

enum Shading {
    /// One color for the whole triangle (constant color, constant texture coordinate).
    Solid([u8; 4]),
    /// Interpolated vertex color times one texel (feathered edges).
    Gradient([u8; 4]),
    /// Per-pixel texture lookup.
    Textured { filter: TextureFilter },
}

fn draw_triangle(
    canvas: &mut Canvas,
    clip: ClipRect,
    pixels_per_point: f32,
    mut v: [&Vertex; 3],
    texture: &Texture,
) {
    let mut p: [Point; 3] = v.map(|v| {
        (
            to_subpixel(v.pos.x, pixels_per_point),
            to_subpixel(v.pos.y, pixels_per_point),
        )
    });
    let mut area = edge_function(p[0], p[1], p[2]);
    if area == 0 {
        return;
    }
    if area < 0 {
        // egui emits both windings; normalize so "inside" is always w >= 0.
        p.swap(1, 2);
        v.swap(1, 2);
        area = -area;
    }

    // Pixel x is sampled at x * SUBPIXEL + HALF_PIXEL.
    let min = |f: fn(&Point) -> i64| p.iter().map(f).min().unwrap_or(0);
    let max = |f: fn(&Point) -> i64| p.iter().map(f).max().unwrap_or(0);
    let x0 = div_ceil(min(|q| q.0) - HALF_PIXEL, SUBPIXEL).max(clip.x0);
    let x1 = (div_floor(max(|q| q.0) - HALF_PIXEL, SUBPIXEL) + 1).min(clip.x1);
    let y0 = div_ceil(min(|q| q.1) - HALF_PIXEL, SUBPIXEL).max(clip.y0);
    let y1 = (div_floor(max(|q| q.1) - HALF_PIXEL, SUBPIXEL) + 1).min(clip.y1);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let shading = match shading(v, area, texture) {
        Some(s) => s,
        None => return, // fully transparent
    };

    let origin = (x0 * SUBPIXEL + HALF_PIXEL, y0 * SUBPIXEL + HALF_PIXEL);
    // Edge i is opposite vertex i, so its value is the barycentric weight of vertex i.
    let edges = [
        Edge::new(p[1], p[2], origin),
        Edge::new(p[2], p[0], origin),
        Edge::new(p[0], p[1], origin),
    ];

    // Attribute interpolation: a(x) = a0 + l1 * (a1 - a0) + l2 * (a2 - a0), l_i = w_i / area.
    let inv_area = 1.0 / area as f32;
    let colors = v.map(|v| v.color.to_array().map(f32::from));
    let d_color1 = sub4(colors[1], colors[0]);
    let d_color2 = sub4(colors[2], colors[0]);
    let uv = v.map(|v| [v.uv.x, v.uv.y]);
    let d_uv1 = [uv[1][0] - uv[0][0], uv[1][1] - uv[0][1]];
    let d_uv2 = [uv[2][0] - uv[0][0], uv[2][1] - uv[0][1]];
    let constant_color = v[0].color == v[1].color && v[1].color == v[2].color;

    let width = canvas.width;
    for row in 0..(y1 - y0) {
        let w: [i64; 3] = edges.map(|e| e.w_origin + row * e.step_y);
        let (mut lo, mut hi) = (0, x1 - x0);
        for (edge, &w) in edges.iter().zip(&w) {
            edge.clip_span(w, &mut lo, &mut hi);
        }
        if lo >= hi {
            continue;
        }

        let y = (y0 + row) as usize;
        let start = y * width + (x0 + lo) as usize;
        let span = &mut canvas.pixels[start..start + (hi - lo) as usize];

        match shading {
            Shading::Solid(src) => {
                if src[3] == 255 {
                    span.fill(src);
                } else {
                    span.iter_mut().for_each(|dst| blend(dst, src));
                }
            }
            Shading::Gradient(texel) => {
                let mut l1 = (w[1] + lo * edges[1].step_x) as f32 * inv_area;
                let mut l2 = (w[2] + lo * edges[2].step_x) as f32 * inv_area;
                let (dl1, dl2) = (
                    edges[1].step_x as f32 * inv_area,
                    edges[2].step_x as f32 * inv_area,
                );
                for dst in span {
                    let color = lerp4(colors[0], d_color1, d_color2, l1, l2);
                    blend(dst, mul_f(color, texel));
                    l1 += dl1;
                    l2 += dl2;
                }
            }
            Shading::Textured { filter } => {
                let mut l1 = (w[1] + lo * edges[1].step_x) as f32 * inv_area;
                let mut l2 = (w[2] + lo * edges[2].step_x) as f32 * inv_area;
                let (dl1, dl2) = (
                    edges[1].step_x as f32 * inv_area,
                    edges[2].step_x as f32 * inv_area,
                );
                let color0 = v[0].color.to_array();
                for dst in span {
                    let u = uv[0][0] + l1 * d_uv1[0] + l2 * d_uv2[0];
                    let t = uv[0][1] + l1 * d_uv1[1] + l2 * d_uv2[1];
                    let texel = texture.sample(u, t, filter);
                    let src = if constant_color {
                        mul(color0, texel)
                    } else {
                        mul_f(lerp4(colors[0], d_color1, d_color2, l1, l2), texel)
                    };
                    blend(dst, src);
                    l1 += dl1;
                    l2 += dl2;
                }
            }
        }
    }
}

/// Picks the cheapest shading for the triangle. `None` if it is invisible.
fn shading(v: [&Vertex; 3], area_subpx: i64, texture: &Texture) -> Option<Shading> {
    let constant_color = v[0].color == v[1].color && v[1].color == v[2].color;
    let constant_uv = v[0].uv == v[1].uv && v[1].uv == v[2].uv;
    if constant_color && v[0].color == egui::Color32::TRANSPARENT {
        return None;
    }
    if constant_uv {
        // Untextured shapes all point at the white texel of the font atlas.
        let texel = texture.sample(v[0].uv.x, v[0].uv.y, texture.options.magnification);
        if constant_color {
            let src = mul(v[0].color.to_array(), texel);
            return (src != [0; 4]).then_some(Shading::Solid(src));
        }
        return Some(Shading::Gradient(texel));
    }

    // Like a GPU, use the minification filter when texels are smaller than pixels.
    let uv_area = ((v[1].uv.x - v[0].uv.x) * (v[2].uv.y - v[0].uv.y)
        - (v[2].uv.x - v[0].uv.x) * (v[1].uv.y - v[0].uv.y))
        .abs()
        * (texture.width * texture.height) as f32;
    let pixel_area = area_subpx as f32 / (SUBPIXEL * SUBPIXEL) as f32;
    let filter = if uv_area > pixel_area * 1.01 {
        texture.options.minification
    } else {
        texture.options.magnification
    };
    Some(Shading::Textured { filter })
}

impl Texture {
    /// Samples at normalized coordinates, like `texture2D` with the texture's wrap mode.
    pub(crate) fn sample(&self, u: f32, v: f32, filter: TextureFilter) -> [u8; 4] {
        if self.width == 0 || self.height == 0 {
            return [0; 4];
        }
        let (w, h) = (self.width as i64, self.height as i64);
        let mode = self.options.wrap_mode;
        let texel =
            |x: i64, y: i64| self.pixels[(wrap(y, h, mode) * w + wrap(x, w, mode)) as usize];
        match filter {
            TextureFilter::Nearest => {
                texel((u * w as f32).floor() as i64, (v * h as f32).floor() as i64)
            }
            TextureFilter::Linear => {
                let x = u * w as f32 - 0.5;
                let y = v * h as f32 - 0.5;
                let (xf, yf) = (x.floor(), y.floor());
                let fx = ((x - xf) * 256.0) as u32;
                let fy = ((y - yf) * 256.0) as u32;
                let (xi, yi) = (xf as i64, yf as i64);
                let [p00, p10, p01, p11] = [
                    texel(xi, yi),
                    texel(xi + 1, yi),
                    texel(xi, yi + 1),
                    texel(xi + 1, yi + 1),
                ];
                std::array::from_fn(|i| {
                    let top = u32::from(p00[i]) * (256 - fx) + u32::from(p10[i]) * fx;
                    let bottom = u32::from(p01[i]) * (256 - fx) + u32::from(p11[i]) * fx;
                    ((top * (256 - fy) + bottom * fy + (1 << 15)) >> 16) as u8
                })
            }
        }
    }
}

fn wrap(i: i64, n: i64, mode: TextureWrapMode) -> i64 {
    match mode {
        TextureWrapMode::ClampToEdge => i.clamp(0, n - 1),
        TextureWrapMode::Repeat => i.rem_euclid(n),
        TextureWrapMode::MirroredRepeat => {
            let p = i.rem_euclid(2 * n);
            if p < n { p } else { 2 * n - 1 - p }
        }
    }
}

/// `round(x / 255)` for `x <= 255 * 255`.
#[inline]
fn div255(x: u32) -> u32 {
    let x = x + 128;
    (x + (x >> 8)) >> 8
}

#[inline]
fn mul(a: [u8; 4], b: [u8; 4]) -> [u8; 4] {
    std::array::from_fn(|i| div255(u32::from(a[i]) * u32::from(b[i])) as u8)
}

#[inline]
fn mul_f(color: [f32; 4], texel: [u8; 4]) -> [u8; 4] {
    std::array::from_fn(|i| {
        ((color[i] * f32::from(texel[i])) / 255.0 + 0.5).clamp(0.0, 255.0) as u8
    })
}

/// Premultiplied "over": `dst = src + dst * (1 - src.a)`.
#[inline]
pub(crate) fn blend(dst: &mut [u8; 4], src: [u8; 4]) {
    let inv = 255 - u32::from(src[3]);
    for i in 0..4 {
        dst[i] = (u32::from(src[i]) + div255(u32::from(dst[i]) * inv)).min(255) as u8;
    }
}

#[inline]
fn sub4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|i| a[i] - b[i])
}

#[inline]
fn lerp4(base: [f32; 4], d1: [f32; 4], d2: [f32; 4], l1: f32, l2: f32) -> [f32; 4] {
    std::array::from_fn(|i| base[i] + l1 * d1[i] + l2 * d2[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Color32, Pos2, TextureOptions, pos2};

    fn white_texture() -> Texture {
        Texture {
            width: 1,
            height: 1,
            pixels: vec![[255; 4]],
            options: TextureOptions::LINEAR,
        }
    }

    fn canvas(w: usize, h: usize) -> Canvas {
        Canvas {
            width: w,
            height: h,
            pixels: vec![[0, 0, 0, 255]; w * h],
        }
    }

    fn full_clip(c: &Canvas) -> ClipRect {
        ClipRect {
            x0: 0,
            y0: 0,
            x1: c.width as i64,
            y1: c.height as i64,
        }
    }

    fn rect_mesh(min: Pos2, max: Pos2, color: Color32) -> Mesh {
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(egui::Rect::from_min_max(min, max), color);
        mesh
    }

    #[test]
    fn div255_rounds_exactly() {
        for x in 0..=255 * 255 {
            assert_eq!(div255(x), (x as f64 / 255.0).round() as u32, "{x}");
        }
    }

    #[test]
    fn opaque_rect_covers_exactly_its_pixels() {
        let mut c = canvas(8, 8);
        let mesh = rect_mesh(pos2(2.0, 3.0), pos2(5.0, 7.0), Color32::RED);
        let clip = full_clip(&c);
        draw_mesh(&mut c, clip, 1.0, &mesh, &white_texture());
        for y in 0..8 {
            for x in 0..8 {
                let inside = (2..5).contains(&x) && (3..7).contains(&y);
                let expected = if inside {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 0, 255]
                };
                assert_eq!(c.pixels[y * 8 + x], expected, "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn shared_diagonal_is_not_blended_twice() {
        // A translucent rect is two triangles sharing a diagonal. Every pixel must be blended
        // exactly once, or the diagonal shows up as a darker seam.
        let mut c = canvas(16, 16);
        let half = Color32::from_rgba_premultiplied(128, 0, 0, 128);
        let mesh = rect_mesh(pos2(0.0, 0.0), pos2(16.0, 16.0), half);
        let clip = full_clip(&c);
        draw_mesh(&mut c, clip, 1.0, &mesh, &white_texture());
        let expected = {
            let mut p = [0, 0, 0, 255];
            blend(&mut p, [128, 0, 0, 128]);
            p
        };
        assert!(c.pixels.iter().all(|&p| p == expected));
    }

    #[test]
    fn adjacent_rects_tile_without_gaps_or_overlap() {
        let mut c = canvas(10, 4);
        let half = Color32::from_rgba_premultiplied(0, 100, 0, 100);
        let clip = full_clip(&c);
        // Fractional boundary at x = 3.3 (with ppp 1.5 → 4.95 px).
        draw_mesh(
            &mut c,
            clip,
            1.5,
            &rect_mesh(pos2(0.0, 0.0), pos2(3.3, 2.0), half),
            &white_texture(),
        );
        draw_mesh(
            &mut c,
            clip,
            1.5,
            &rect_mesh(pos2(3.3, 0.0), pos2(6.0, 2.0), half),
            &white_texture(),
        );
        let mut once = [0, 0, 0, 255];
        blend(&mut once, half.to_array());
        assert!(
            c.pixels[..9].iter().all(|&p| p == once),
            "{:?}",
            &c.pixels[..10]
        );
    }

    #[test]
    fn clip_rect_limits_drawing() {
        let mut c = canvas(8, 8);
        let mesh = rect_mesh(pos2(0.0, 0.0), pos2(8.0, 8.0), Color32::WHITE);
        draw_mesh(
            &mut c,
            ClipRect {
                x0: 2,
                y0: 2,
                x1: 4,
                y1: 3,
            },
            1.0,
            &mesh,
            &white_texture(),
        );
        let white: Vec<usize> = (0..64).filter(|&i| c.pixels[i] == [255; 4]).collect();
        assert_eq!(white, vec![2 * 8 + 2, 2 * 8 + 3]);
    }

    #[test]
    fn both_windings_are_drawn() {
        for indices in [[0, 1, 2], [0, 2, 1]] {
            let mut c = canvas(4, 4);
            let mut mesh = Mesh::default();
            for p in [pos2(0.0, 0.0), pos2(4.0, 0.0), pos2(0.0, 4.0)] {
                mesh.colored_vertex(p, Color32::WHITE);
            }
            mesh.indices.extend(indices);
            let clip = full_clip(&c);
            draw_mesh(&mut c, clip, 1.0, &mesh, &white_texture());
            assert_eq!(c.pixels[0], [255; 4]);
        }
    }

    #[test]
    fn texture_is_sampled_one_to_one() {
        // A 2x2 texture drawn on 2x2 pixels must reproduce each texel exactly.
        let pixels = vec![
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        let texture = Texture {
            width: 2,
            height: 2,
            pixels: pixels.clone(),
            options: TextureOptions::LINEAR,
        };
        let mut mesh = Mesh::with_texture(egui::TextureId::default());
        mesh.add_rect_with_uv(
            egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(2.0, 2.0)),
            egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let mut c = canvas(2, 2);
        let clip = full_clip(&c);
        draw_mesh(&mut c, clip, 1.0, &mesh, &texture);
        assert_eq!(c.pixels, pixels);
    }

    #[test]
    fn wrap_modes() {
        assert_eq!(wrap(-1, 4, TextureWrapMode::ClampToEdge), 0);
        assert_eq!(wrap(5, 4, TextureWrapMode::ClampToEdge), 3);
        assert_eq!(wrap(-1, 4, TextureWrapMode::Repeat), 3);
        assert_eq!(wrap(4, 4, TextureWrapMode::MirroredRepeat), 3);
        assert_eq!(wrap(-1, 4, TextureWrapMode::MirroredRepeat), 0);
    }
}
