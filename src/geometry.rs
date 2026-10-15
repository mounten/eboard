//! Pure screen math: rotation and pixels-per-point. No platform dependencies, unit tested on any host.

use std::str::FromStr;

/// Rotation of the UI on the panel, clockwise.
///
/// `Deg90` means the egui content is turned 90° clockwise before it reaches the panel. Use it for a
/// native portrait panel (e.g. the Raspberry Pi Touch Display 2, 720×1280) that is mounted in
/// landscape: egui then sees a 1280×720 screen. Use `Deg270` if the panel is mounted the other way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotation {
    /// Accepts any multiple of 90, including negative values (`-90` == `270`).
    pub fn from_degrees(degrees: i32) -> Option<Self> {
        match degrees.rem_euclid(360) {
            0 => Some(Self::Deg0),
            90 => Some(Self::Deg90),
            180 => Some(Self::Deg180),
            270 => Some(Self::Deg270),
            _ => None,
        }
    }

    pub fn degrees(self) -> u32 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 90,
            Self::Deg180 => 180,
            Self::Deg270 => 270,
        }
    }

    /// True for 90° and 270°, where width and height trade places.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::Deg90 | Self::Deg270)
    }

    /// The size egui sees, given the panel size in its native orientation.
    pub fn logical_size<T>(self, [w, h]: [T; 2]) -> [T; 2] {
        if self.swaps_axes() { [h, w] } else { [w, h] }
    }

    /// Maps a position on the panel (native orientation, origin top-left, in pixels) to the
    /// position in the rotated egui screen (origin top-left, in pixels).
    ///
    /// Used for the rotated blit and, later, for touch input in panel coordinates.
    pub fn panel_to_logical(self, [x, y]: [f32; 2], [panel_w, panel_h]: [f32; 2]) -> [f32; 2] {
        match self {
            Self::Deg0 => [x, y],
            // The logical top-left corner sits at the panel's top-right corner.
            Self::Deg90 => [y, panel_w - x],
            Self::Deg180 => [panel_w - x, panel_h - y],
            // The logical top-left corner sits at the panel's bottom-left corner.
            Self::Deg270 => [panel_h - y, x],
        }
    }

    #[cfg_attr(not(feature = "soft"), allow(dead_code))]
    /// Pixel-index version of [`Self::panel_to_logical`]: which logical pixel is shown at panel
    /// pixel `(x, y)`. Used by the software rotation in the framebuffer copy.
    pub(crate) fn panel_to_logical_px(
        self,
        [x, y]: [usize; 2],
        [panel_w, panel_h]: [usize; 2],
    ) -> [usize; 2] {
        match self {
            Self::Deg0 => [x, y],
            Self::Deg90 => [y, panel_w - 1 - x],
            Self::Deg180 => [panel_w - 1 - x, panel_h - 1 - y],
            Self::Deg270 => [panel_h - 1 - y, x],
        }
    }

    #[cfg_attr(not(feature = "soft"), allow(dead_code))]
    /// Inverse of [`Self::panel_to_logical_px`]: where logical pixel `(x, y)` lands on the panel.
    pub(crate) fn logical_to_panel_px(
        self,
        [x, y]: [usize; 2],
        [panel_w, panel_h]: [usize; 2],
    ) -> [usize; 2] {
        match self {
            Self::Deg0 => [x, y],
            Self::Deg90 => [panel_w - 1 - y, x],
            Self::Deg180 => [panel_w - 1 - x, panel_h - 1 - y],
            Self::Deg270 => [y, panel_h - 1 - x],
        }
    }
}

impl FromStr for Rotation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let degrees: i32 = s
            .trim()
            .parse()
            .map_err(|_| format!("rotation {s:?} is not a number"))?;
        Self::from_degrees(degrees)
            .ok_or_else(|| format!("rotation {degrees} is not a multiple of 90"))
    }
}

/// Texture coordinates (GL convention: origin bottom-left) for the four corners of a full-screen
/// quad, so that sampling an FBO holding the logical (unrotated) egui image shows it rotated on the
/// panel.
///
/// Corner order: panel bottom-left, bottom-right, top-left, top-right, matching clip-space
/// positions `(-1,-1), (1,-1), (-1,1), (1,1)` (a triangle strip).
#[cfg_attr(not(feature = "glow"), allow(dead_code))]
pub(crate) fn blit_tex_coords(rotation: Rotation) -> [[f32; 2]; 4] {
    // Panel corners as (u, v) with v pointing down.
    const CORNERS: [[f32; 2]; 4] = [[0.0, 1.0], [1.0, 1.0], [0.0, 0.0], [1.0, 0.0]];
    CORNERS.map(|corner| {
        let [u, v] = rotation.panel_to_logical(corner, [1.0, 1.0]);
        // egui rendered the logical top row at the top of the FBO, which is t = 1 in GL.
        [u, 1.0 - v]
    })
}

/// Physical sizes smaller than this (in mm) are treated as bogus. Some EDIDs report only the
/// aspect ratio, and some drivers report 0 or a few mm for virtual panels.
const MIN_PLAUSIBLE_MM: u32 = 20;

/// Pixel density at which egui gets `pixels_per_point == 1.0`. This is Android's `mdpi` baseline,
/// which suits touch panels viewed at arm's length better than the desktop's 96 dpi.
const BASELINE_DPI: f32 = 160.0;

/// Picks `native_pixels_per_point` from the screen size in pixels and its physical size in mm.
///
/// Returns 1.0 if the physical size is unknown or implausible. The result is rounded to a quarter
/// step and clamped to `1.0..=4.0` so typical desktop monitors (~90–110 dpi) stay at 1.0.
pub fn pixels_per_point_from_size(size_px: [u32; 2], size_mm: Option<[u32; 2]>) -> f32 {
    let Some([w_mm, h_mm]) = size_mm else {
        return 1.0;
    };
    if w_mm < MIN_PLAUSIBLE_MM || h_mm < MIN_PLAUSIBLE_MM || size_px[0] == 0 || size_px[1] == 0 {
        return 1.0;
    }
    let diag_px = (size_px[0] as f32).hypot(size_px[1] as f32);
    let diag_inch = (w_mm as f32).hypot(h_mm as f32) / 25.4;
    let dpi = diag_px / diag_inch;
    ((dpi / BASELINE_DPI) * 4.0).round().clamp(4.0, 16.0) / 4.0
}

/// Parses `"WIDTHxHEIGHT"`, e.g. `"1280x720"` or `"155x88"`.
pub(crate) fn parse_size(s: &str) -> Option<[u32; 2]> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    Some([w.trim().parse().ok()?, h.trim().parse().ok()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_parsing() {
        assert_eq!("90".parse::<Rotation>(), Ok(Rotation::Deg90));
        assert_eq!("-90".parse::<Rotation>(), Ok(Rotation::Deg270));
        assert_eq!(" 180 ".parse::<Rotation>(), Ok(Rotation::Deg180));
        assert_eq!("360".parse::<Rotation>(), Ok(Rotation::Deg0));
        assert!("45".parse::<Rotation>().is_err());
        assert!("left".parse::<Rotation>().is_err());
    }

    #[test]
    fn logical_size_swaps_for_quarter_turns() {
        assert_eq!(Rotation::Deg0.logical_size([720, 1280]), [720, 1280]);
        assert_eq!(Rotation::Deg90.logical_size([720, 1280]), [1280, 720]);
        assert_eq!(Rotation::Deg180.logical_size([720, 1280]), [720, 1280]);
        assert_eq!(Rotation::Deg270.logical_size([720, 1280]), [1280, 720]);
    }

    #[test]
    fn panel_to_logical_maps_corners() {
        // Portrait 720x1280 panel showing a landscape 1280x720 UI.
        let panel = [720.0, 1280.0];
        // 90° clockwise: logical top-left is at the panel's top-right.
        assert_eq!(
            Rotation::Deg90.panel_to_logical([720.0, 0.0], panel),
            [0.0, 0.0]
        );
        // Logical top-right (1280, 0) is at the panel's bottom-right.
        assert_eq!(
            Rotation::Deg90.panel_to_logical([720.0, 1280.0], panel),
            [1280.0, 0.0]
        );
        // Logical bottom-left (0, 720) is at the panel's top-left.
        assert_eq!(
            Rotation::Deg90.panel_to_logical([0.0, 0.0], panel),
            [0.0, 720.0]
        );

        // 270°: logical top-left is at the panel's bottom-left.
        assert_eq!(
            Rotation::Deg270.panel_to_logical([0.0, 1280.0], panel),
            [0.0, 0.0]
        );
        assert_eq!(
            Rotation::Deg270.panel_to_logical([0.0, 0.0], panel),
            [1280.0, 0.0]
        );

        assert_eq!(
            Rotation::Deg180.panel_to_logical([0.0, 0.0], panel),
            [720.0, 1280.0]
        );
        assert_eq!(
            Rotation::Deg0.panel_to_logical([10.0, 20.0], panel),
            [10.0, 20.0]
        );
    }

    #[test]
    fn pixel_mapping_matches_continuous_mapping_and_inverts() {
        let panel = [4usize, 6usize];
        for rotation in [
            Rotation::Deg0,
            Rotation::Deg90,
            Rotation::Deg180,
            Rotation::Deg270,
        ] {
            for y in 0..panel[1] {
                for x in 0..panel[0] {
                    let l = rotation.panel_to_logical_px([x, y], panel);
                    // The pixel center maps to the logical pixel's center.
                    let center = rotation.panel_to_logical(
                        [x as f32 + 0.5, y as f32 + 0.5],
                        [panel[0] as f32, panel[1] as f32],
                    );
                    assert_eq!(
                        [l[0] as f32 + 0.5, l[1] as f32 + 0.5],
                        center,
                        "{rotation:?}"
                    );
                    assert_eq!(
                        rotation.logical_to_panel_px(l, panel),
                        [x, y],
                        "{rotation:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn panel_to_logical_round_trips_through_all_rotations() {
        // Rotating by 90° four times must give the identity.
        let panel = [720.0, 1280.0];
        let p = [100.0, 300.0];
        let once = Rotation::Deg90.panel_to_logical(p, panel);
        let twice = Rotation::Deg90.panel_to_logical(once, Rotation::Deg90.logical_size(panel));
        assert_eq!(twice, Rotation::Deg180.panel_to_logical(p, panel));
    }

    #[test]
    fn blit_tex_coords_identity() {
        assert_eq!(
            blit_tex_coords(Rotation::Deg0),
            [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]
        );
    }

    #[test]
    fn blit_tex_coords_quarter_turn() {
        // 90° clockwise: the panel's top-right corner must sample the logical top-left,
        // which is texel (0, 1) in GL texture space.
        let tc = blit_tex_coords(Rotation::Deg90);
        assert_eq!(tc[3], [0.0, 1.0]);
        // Panel top-left shows the logical bottom-left: texel (0, 0).
        assert_eq!(tc[2], [0.0, 0.0]);
        // Panel bottom-right shows the logical top-right: texel (1, 1).
        assert_eq!(tc[1], [1.0, 1.0]);
    }

    #[test]
    fn ppp_from_physical_size() {
        // Raspberry Pi Touch Display 2, 7": 720x1280, ~88x155 mm -> ~210 dpi.
        assert_eq!(
            pixels_per_point_from_size([720, 1280], Some([88, 155])),
            1.25
        );
        // 24" 1080p monitor -> ~92 dpi, clamped to 1.0.
        assert_eq!(
            pixels_per_point_from_size([1920, 1080], Some([531, 299])),
            1.0
        );
        // 5" 720x1280 -> ~294 dpi.
        assert_eq!(
            pixels_per_point_from_size([720, 1280], Some([62, 110])),
            1.75
        );
        // Unknown or bogus sizes.
        assert_eq!(pixels_per_point_from_size([720, 1280], None), 1.0);
        assert_eq!(pixels_per_point_from_size([1920, 1080], Some([16, 9])), 1.0);
        assert_eq!(pixels_per_point_from_size([1920, 1080], Some([0, 0])), 1.0);
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("1280x720"), Some([1280, 720]));
        assert_eq!(parse_size(" 155 X 88 "), Some([155, 88]));
        assert_eq!(parse_size("1280"), None);
        assert_eq!(parse_size("ax720"), None);
    }
}
