//! Copying the canvas to framebuffer memory: damage detection, rotation and pixel-format
//! conversion. Pure functions, unit tested on the host.

use crate::Rotation;

/// One color channel of a packed pixel: `length` bits starting at bit `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Channel {
    pub offset: u32,
    pub length: u32,
}

/// A packed truecolor pixel layout (little-endian pixel values), as described by the fbdev
/// `fb_var_screeninfo` bitfields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelFormat {
    pub bytes_per_pixel: usize,
    pub red: Channel,
    pub green: Channel,
    pub blue: Channel,
    /// Set to fully opaque when present.
    pub alpha: Option<Channel>,
}

impl PixelFormat {
    #[cfg(test)]
    pub(crate) const XRGB8888: Self = Self {
        bytes_per_pixel: 4,
        red: Channel {
            offset: 16,
            length: 8,
        },
        green: Channel {
            offset: 8,
            length: 8,
        },
        blue: Channel {
            offset: 0,
            length: 8,
        },
        alpha: None,
    };

    #[cfg(test)]
    pub(crate) const RGB565: Self = Self {
        bytes_per_pixel: 2,
        red: Channel {
            offset: 11,
            length: 5,
        },
        green: Channel {
            offset: 5,
            length: 6,
        },
        blue: Channel {
            offset: 0,
            length: 5,
        },
        alpha: None,
    };

    /// Checks that the layout is something we can write.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(2..=4).contains(&self.bytes_per_pixel) {
            return Err(format!(
                "{} bytes per pixel is not supported",
                self.bytes_per_pixel
            ));
        }
        let bits = self.bytes_per_pixel as u32 * 8;
        let channels = [
            Some(self.red),
            Some(self.green),
            Some(self.blue),
            self.alpha,
        ];
        for c in channels.into_iter().flatten() {
            if c.length == 0 || c.length > 8 || c.offset + c.length > bits {
                return Err(format!("unsupported channel layout {c:?} in {bits} bits"));
            }
        }
        Ok(())
    }

    /// e.g. `"RGB565"`, `"XRGB8888"`, `"ARGB8888"`.
    pub(crate) fn name(&self) -> String {
        let mut channels: Vec<(u32, char, u32)> = vec![
            (self.red.offset, 'R', self.red.length),
            (self.green.offset, 'G', self.green.length),
            (self.blue.offset, 'B', self.blue.length),
        ];
        let used: u32 =
            channels.iter().map(|c| c.2).sum::<u32>() + self.alpha.map_or(0, |a| a.length);
        let total = self.bytes_per_pixel as u32 * 8;
        match self.alpha {
            Some(a) => channels.push((a.offset, 'A', a.length)),
            None if used < total => {
                let x_offset = (0..total)
                    .find(|&bit| {
                        ![self.red, self.green, self.blue]
                            .iter()
                            .any(|c| (c.offset..c.offset + c.length).contains(&bit))
                    })
                    .unwrap_or(total);
                channels.push((x_offset, 'X', total - used));
            }
            None => {}
        }
        channels.sort_by_key(|c| std::cmp::Reverse(c.0));
        let letters: String = channels.iter().map(|c| c.1).collect();
        let sizes: String = channels.iter().map(|c| c.2.to_string()).collect();
        format!("{letters}{sizes}")
    }

    /// Packs a premultiplied RGBA pixel (shown over black, so the RGB is the final color).
    #[inline]
    pub(crate) fn pack(&self, [r, g, b, _]: [u8; 4]) -> u32 {
        #[inline]
        fn field(value: u8, c: Channel) -> u32 {
            (u32::from(value) >> (8 - c.length)) << c.offset
        }
        let mut pixel = field(r, self.red) | field(g, self.green) | field(b, self.blue);
        if let Some(a) = self.alpha {
            pixel |= field(255, a);
        }
        pixel
    }
}

/// Half-open pixel rectangle `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl PixelRect {
    pub(crate) fn full([w, h]: [usize; 2]) -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: w,
            y1: h,
        }
    }

    /// The bounding box of both rectangles.
    pub(crate) fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

/// How a framebuffer can be double buffered (page flipping with `FBIOPAN_DISPLAY`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Paging {
    /// One page: frames are drawn into the visible page.
    Single,
    /// The virtual height holds two pages.
    Flip,
    /// The memory holds two pages, but the virtual height has to be raised first.
    Resize,
}

/// Decides the [`Paging`] for `yres` visible lines of `stride` bytes. `ypanstep` 0 means the
/// driver cannot pan.
pub(crate) fn paging(
    yres: usize,
    yres_virtual: usize,
    ypanstep: usize,
    stride: usize,
    memory_len: usize,
) -> Paging {
    if yres == 0 || ypanstep == 0 || !yres.is_multiple_of(ypanstep) {
        Paging::Single
    } else if yres_virtual >= 2 * yres {
        Paging::Flip
    } else if memory_len >= 2 * yres * stride {
        Paging::Resize
    } else {
        Paging::Single
    }
}

/// The bounding box of the pixels that differ between `previous` and `current`, or `None` if the
/// frames are equal. Updates `previous` to `current` inside that box.
pub(crate) fn damage(
    previous: &mut [[u8; 4]],
    current: &[[u8; 4]],
    width: usize,
) -> Option<PixelRect> {
    debug_assert_eq!(previous.len(), current.len());
    let mut rect: Option<PixelRect> = None;
    for (y, (prev_row, cur_row)) in previous
        .chunks_exact(width)
        .zip(current.chunks_exact(width))
        .enumerate()
    {
        if prev_row == cur_row {
            continue;
        }
        let first = prev_row
            .iter()
            .zip(cur_row)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let last = width
            - prev_row
                .iter()
                .zip(cur_row)
                .rev()
                .position(|(a, b)| a != b)
                .unwrap_or(0);
        rect = Some(match rect {
            None => PixelRect {
                x0: first,
                y0: y,
                x1: last,
                y1: y + 1,
            },
            Some(r) => PixelRect {
                x0: r.x0.min(first),
                y0: r.y0,
                x1: r.x1.max(last),
                y1: y + 1,
            },
        });
    }
    if let Some(r) = rect {
        for y in r.y0..r.y1 {
            let range = y * width + r.x0..y * width + r.x1;
            previous[range.clone()].copy_from_slice(&current[range]);
        }
    }
    rect
}

/// Where a logical (canvas) rectangle ends up on the panel.
pub(crate) fn logical_to_panel_rect(
    r: PixelRect,
    rotation: Rotation,
    panel_size: [usize; 2],
) -> PixelRect {
    let a = rotation.logical_to_panel_px([r.x0, r.y0], panel_size);
    let b = rotation.logical_to_panel_px([r.x1 - 1, r.y1 - 1], panel_size);
    PixelRect {
        x0: a[0].min(b[0]),
        y0: a[1].min(b[1]),
        x1: a[0].max(b[0]) + 1,
        y1: a[1].max(b[1]) + 1,
    }
}

/// Destination framebuffer memory.
pub(crate) struct Target<'a> {
    pub memory: &'a mut [u8],
    /// Bytes per row.
    pub stride: usize,
    pub format: PixelFormat,
    /// Panel size in pixels (native orientation).
    pub size: [usize; 2],
}

/// Converts `rect` (panel coordinates) of the canvas into the framebuffer, rotating as needed.
/// Each row is assembled in `row` and written with one copy, which suits uncached framebuffer
/// memory.
pub(crate) fn blit(
    canvas: &[[u8; 4]],
    canvas_width: usize,
    rotation: Rotation,
    rect: PixelRect,
    target: &mut Target<'_>,
    row: &mut Vec<u8>,
) {
    let bpp = target.format.bytes_per_pixel;
    for y in rect.y0..rect.y1 {
        row.clear();
        for x in rect.x0..rect.x1 {
            let [lx, ly] = rotation.panel_to_logical_px([x, y], target.size);
            let pixel = target.format.pack(canvas[ly * canvas_width + lx]);
            row.extend_from_slice(&pixel.to_le_bytes()[..bpp]);
        }
        let start = y * target.stride + rect.x0 * bpp;
        target.memory[start..start + row.len()].copy_from_slice(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names() {
        assert_eq!(PixelFormat::XRGB8888.name(), "XRGB8888");
        assert_eq!(PixelFormat::RGB565.name(), "RGB565");
        let abgr = PixelFormat {
            alpha: Some(Channel {
                offset: 24,
                length: 8,
            }),
            red: Channel {
                offset: 0,
                length: 8,
            },
            blue: Channel {
                offset: 16,
                length: 8,
            },
            ..PixelFormat::XRGB8888
        };
        assert_eq!(abgr.name(), "ABGR8888");
    }

    #[test]
    fn packing() {
        assert_eq!(
            PixelFormat::XRGB8888.pack([0x12, 0x34, 0x56, 0xff]),
            0x0012_3456
        );
        assert_eq!(PixelFormat::RGB565.pack([255, 0, 0, 255]), 0xf800);
        assert_eq!(PixelFormat::RGB565.pack([0, 255, 0, 255]), 0x07e0);
        assert_eq!(PixelFormat::RGB565.pack([0, 0, 255, 255]), 0x001f);
        let argb = PixelFormat {
            alpha: Some(Channel {
                offset: 24,
                length: 8,
            }),
            ..PixelFormat::XRGB8888
        };
        assert_eq!(argb.pack([0, 0, 0, 0]), 0xff00_0000);
    }

    #[test]
    fn validation() {
        assert!(PixelFormat::XRGB8888.validate().is_ok());
        assert!(PixelFormat::RGB565.validate().is_ok());
        let bad = PixelFormat {
            bytes_per_pixel: 1,
            ..PixelFormat::RGB565
        };
        assert!(bad.validate().is_err());
        let ten_bit = PixelFormat {
            red: Channel {
                offset: 20,
                length: 10,
            },
            ..PixelFormat::XRGB8888
        };
        assert!(ten_bit.validate().is_err());
    }

    #[test]
    fn damage_finds_bounding_box_and_syncs() {
        let w = 5;
        let mut prev = vec![[0u8; 4]; w * 4];
        let mut cur = prev.clone();
        assert_eq!(damage(&mut prev, &cur, w), None);
        cur[w + 1] = [1; 4];
        cur[2 * w + 3] = [2; 4];
        assert_eq!(
            damage(&mut prev, &cur, w),
            Some(PixelRect {
                x0: 1,
                y0: 1,
                x1: 4,
                y1: 3
            })
        );
        assert_eq!(prev, cur);
        assert_eq!(damage(&mut prev, &cur, w), None);
    }

    #[test]
    fn rect_union() {
        let a = PixelRect {
            x0: 1,
            y0: 5,
            x1: 3,
            y1: 6,
        };
        let b = PixelRect {
            x0: 2,
            y0: 0,
            x1: 8,
            y1: 2,
        };
        let both = PixelRect {
            x0: 1,
            y0: 0,
            x1: 8,
            y1: 6,
        };
        assert_eq!(a.union(b), both);
        assert_eq!(b.union(a), both);
        assert_eq!(a.union(a), a);
    }

    #[test]
    fn paging_needs_panning_and_two_pages() {
        let (yres, stride) = (768, 4096);
        let page = yres * stride;
        // The i.MX6 BSP's triple-buffered fb0.
        assert_eq!(paging(yres, 3 * yres, 1, stride, 3 * page), Paging::Flip);
        assert_eq!(paging(yres, 2 * yres, 1, stride, 2 * page), Paging::Flip);
        // Enough memory, but a one-page virtual size.
        assert_eq!(paging(yres, yres, 1, stride, 2 * page), Paging::Resize);
        // One page (DRM fbdev emulation without overallocation).
        assert_eq!(paging(yres, yres, 1, stride, page), Paging::Single);
        assert_eq!(paging(yres, yres, 1, stride, 2 * page - 1), Paging::Single);
        // No panning, or pages that don't start on a pan step.
        assert_eq!(paging(yres, 2 * yres, 0, stride, 2 * page), Paging::Single);
        assert_eq!(paging(yres, 2 * yres, 5, stride, 2 * page), Paging::Single);
        assert_eq!(paging(yres, 2 * yres, 8, stride, 2 * page), Paging::Flip);
        assert_eq!(paging(0, 0, 1, stride, page), Paging::Single);
    }

    #[test]
    fn rect_rotation() {
        // Logical 6x4 canvas on a 4x6 portrait panel, rotated 90°.
        let panel = [4, 6];
        let r = PixelRect {
            x0: 0,
            y0: 0,
            x1: 2,
            y1: 1,
        }; // top-left of the logical screen
        // ...shows at the panel's top-right, as a vertical strip.
        assert_eq!(
            logical_to_panel_rect(r, Rotation::Deg90, panel),
            PixelRect {
                x0: 3,
                y0: 0,
                x1: 4,
                y1: 2
            }
        );
        let full = PixelRect::full([6, 4]);
        for rotation in [Rotation::Deg90, Rotation::Deg270] {
            assert_eq!(
                logical_to_panel_rect(full, rotation, panel),
                PixelRect::full(panel)
            );
        }
        assert_eq!(
            logical_to_panel_rect(PixelRect::full([4, 6]), Rotation::Deg180, panel),
            PixelRect::full(panel)
        );
    }

    #[test]
    fn blit_rotates_and_converts() {
        // 2x1 logical canvas (red, green) on a 1x2 panel, rotated 90° clockwise:
        // red (logical left) goes to the top.
        let canvas = [[255, 0, 0, 255], [0, 255, 0, 255]];
        let mut memory = vec![0u8; 2 * 4];
        let mut target = Target {
            memory: &mut memory,
            stride: 4,
            format: PixelFormat::XRGB8888,
            size: [1, 2],
        };
        blit(
            &canvas,
            2,
            Rotation::Deg90,
            PixelRect::full([1, 2]),
            &mut target,
            &mut Vec::new(),
        );
        assert_eq!(memory, [0, 0, 255, 0, 0, 255, 0, 0]);
    }

    #[test]
    fn blit_partial_rect_respects_stride() {
        let canvas = [[0, 0, 255, 255]; 4]; // 2x2 blue
        let mut memory = vec![0u8; 2 * 8]; // stride 8 = 2 pixels of RGB565 + 4 bytes padding
        let mut target = Target {
            memory: &mut memory,
            stride: 8,
            format: PixelFormat::RGB565,
            size: [2, 2],
        };
        blit(
            &canvas,
            2,
            Rotation::Deg0,
            PixelRect {
                x0: 1,
                y0: 1,
                x1: 2,
                y1: 2,
            },
            &mut target,
            &mut Vec::new(),
        );
        assert_eq!(memory, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x1f, 0, 0, 0, 0, 0]);
    }
}
