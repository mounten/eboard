//! Renderers turn tessellated egui output into pixels on the platform's current surface.

#[cfg(feature = "glow")]
pub(crate) mod glow;
#[cfg(feature = "soft")]
pub(crate) mod soft;

/// Paints one egui frame. The platform makes the target surface current before and presents it
/// after.
pub(crate) trait Renderer {
    /// Largest texture side the GPU supports, handed to egui via `RawInput::max_texture_side`.
    fn max_texture_side(&self) -> usize;

    /// Paints `primitives` at `size_px` (the logical, rotated size egui laid out for) and applies
    /// the texture updates.
    fn paint(
        &mut self,
        size_px: [u32; 2],
        pixels_per_point: f32,
        clear_color: [f32; 4],
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &mut egui::TexturesDelta,
    );

    /// Frees GPU resources. Must be called while the context is still current.
    fn destroy(&mut self);
}
