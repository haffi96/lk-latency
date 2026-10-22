/// Generates an SMPTE-style 75% color-bar pattern in I420 format, optionally animated.
pub struct TestPattern {
    width: usize,
    height: usize,
    chroma_width: usize,
    chroma_height: usize,
    y_plane: Vec<u8>,
    u_plane: Vec<u8>,
    v_plane: Vec<u8>,
    motion: Option<Motion>,
}

/// Per-frame animation state for the moving pattern.
struct Motion {
    frame: usize,
    noise: u32,
}

/// Height of the changing-block band, as a fraction of the frame height.
const NOISE_BAND_DIVISOR: usize = 6;
/// Edge length of each luma block in the changing band (even, to align with chroma).
const NOISE_BLOCK: usize = 16;

#[derive(Clone, Copy)]
struct I420Color {
    y: u8,
    u: u8,
    v: u8,
}

const BARS: [I420Color; 7] = [
    rgb_to_i420(191, 191, 191), // white
    rgb_to_i420(191, 191, 0),   // yellow
    rgb_to_i420(0, 191, 191),   // cyan
    rgb_to_i420(0, 191, 0),     // green
    rgb_to_i420(191, 0, 191),   // magenta
    rgb_to_i420(191, 0, 0),     // red
    rgb_to_i420(0, 0, 191),     // blue
];

impl TestPattern {
    /// Precompute a static SMPTE-style 75% color-bar pattern for the requested resolution.
    pub fn new(width: u32, height: u32) -> Self {
        let width = width as usize;
        let height = height as usize;
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let mut y_plane = vec![0; width * height];
        let mut u_plane = vec![128; chroma_width * chroma_height];
        let mut v_plane = vec![128; chroma_width * chroma_height];

        for row in 0..height {
            let row_start = row * width;
            for col in 0..width {
                y_plane[row_start + col] = color_for_luma_column(col, width).y;
            }
        }

        for row in 0..chroma_height {
            let row_start = row * chroma_width;
            for col in 0..chroma_width {
                let color = color_for_luma_column(col * 2, width);
                u_plane[row_start + col] = color.u;
                v_plane[row_start + col] = color.v;
            }
        }

        Self { width, height, chroma_width, chroma_height, y_plane, u_plane, v_plane, motion: None }
    }

    /// Animate the bars so encoders see sustained motion and detail, closer to real video.
    ///
    /// Each frame scrolls the bars, moves a box, and refreshes a band of pseudo-random
    /// blocks. The output is deterministic, so runs with equal settings are comparable.
    pub fn moving(width: u32, height: u32) -> Self {
        Self { motion: Some(Motion { frame: 0, noise: 0x9e37_79b9 }), ..Self::new(width, height) }
    }

    /// Render the next frame of the pattern into the provided I420 destination planes.
    pub fn render(
        &mut self,
        data_y: &mut [u8],
        stride_y: i32,
        data_u: &mut [u8],
        stride_u: i32,
        data_v: &mut [u8],
        stride_v: i32,
    ) {
        let (stride_y, stride_u, stride_v) = (stride_y as usize, stride_u as usize, stride_v as usize);
        let Some(motion) = &mut self.motion else {
            copy_plane(data_y, stride_y, &self.y_plane, self.width, self.height);
            copy_plane(data_u, stride_u, &self.u_plane, self.chroma_width, self.chroma_height);
            copy_plane(data_v, stride_v, &self.v_plane, self.chroma_width, self.chroma_height);
            return;
        };
        if self.width < 2 || self.height < 2 {
            return;
        }

        // Scroll by an even luma offset so chroma stays aligned with luma.
        let shift = (motion.frame * 4) % self.width & !1;
        scroll_plane(data_y, stride_y, &self.y_plane, self.width, self.height, shift);
        let (cw, ch) = (self.chroma_width, self.chroma_height);
        scroll_plane(data_u, stride_u, &self.u_plane, cw, ch, (shift / 2) % cw);
        scroll_plane(data_v, stride_v, &self.v_plane, cw, ch, (shift / 2) % cw);

        // A box bouncing diagonally across the bars.
        let size = (self.width.min(self.height) / 5).max(2) & !1;
        let travel = |extent: usize, speed: usize| {
            let range = extent.saturating_sub(size).max(1);
            let position = (motion.frame * speed) % (2 * range);
            (if position < range { position } else { 2 * range - position }) & !1
        };
        let (box_x, box_y) = (travel(self.width, 6), travel(self.height, 4));
        let white = rgb_to_i420(235, 235, 235);
        fill_rect(data_y, stride_y, box_x, box_y, size, size, self.width, self.height, white.y);
        let (bx, by, bs) = (box_x / 2, box_y / 2, size / 2);
        fill_rect(data_u, stride_u, bx, by, bs, bs, cw, ch, white.u);
        fill_rect(data_v, stride_v, bx, by, bs, bs, cw, ch, white.v);

        // A band of grey blocks that change every frame, so the encoder always
        // has new detail to code, as it would with real camera content.
        let band_top = (self.height - self.height / NOISE_BAND_DIVISOR) & !1;
        for top in (band_top..self.height).step_by(NOISE_BLOCK) {
            for left in (0..self.width).step_by(NOISE_BLOCK) {
                motion.noise ^= motion.noise << 13;
                motion.noise ^= motion.noise >> 17;
                motion.noise ^= motion.noise << 5;
                let luma = 16 + (motion.noise % 220) as u8;
                fill_rect(data_y, stride_y, left, top, NOISE_BLOCK, NOISE_BLOCK, self.width, self.height, luma);
            }
        }
        let chroma_top = band_top / 2;
        fill_rect(data_u, stride_u, 0, chroma_top, cw, ch - chroma_top, cw, ch, 128);
        fill_rect(data_v, stride_v, 0, chroma_top, cw, ch - chroma_top, cw, ch, 128);

        motion.frame = motion.frame.wrapping_add(1);
    }
}

/// Copy `src` into `dst`, rotated left by `shift` columns.
fn scroll_plane(dst: &mut [u8], dst_stride: usize, src: &[u8], width: usize, height: usize, shift: usize) {
    for row in 0..height {
        let src_row = &src[row * width..(row + 1) * width];
        let dst_row = &mut dst[row * dst_stride..row * dst_stride + width];
        dst_row[..width - shift].copy_from_slice(&src_row[shift..]);
        dst_row[width - shift..].copy_from_slice(&src_row[..shift]);
    }
}

/// Fill a rectangle, clipped to the plane bounds.
#[allow(clippy::too_many_arguments)]
fn fill_rect(
    dst: &mut [u8],
    stride: usize,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    plane_width: usize,
    plane_height: usize,
    value: u8,
) {
    let right = (left + width).min(plane_width);
    for row in top..(top + height).min(plane_height) {
        if left < right {
            dst[row * stride + left..row * stride + right].fill(value);
        }
    }
}

const fn rgb_to_i420(r: u8, g: u8, b: u8) -> I420Color {
    let r = r as i32;
    let g = g as i32;
    let b = b as i32;
    I420Color {
        y: clamp_to_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16),
        u: clamp_to_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128),
        v: clamp_to_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128),
    }
}

const fn clamp_to_u8(value: i32) -> u8 {
    if value < 0 {
        0
    } else if value > u8::MAX as i32 {
        u8::MAX
    } else {
        value as u8
    }
}

fn color_for_luma_column(col: usize, width: usize) -> I420Color {
    if width == 0 {
        return BARS[0];
    }

    let bar = (col * BARS.len()) / width;
    BARS[bar.min(BARS.len() - 1)]
}

fn copy_plane(dst: &mut [u8], dst_stride: usize, src: &[u8], width: usize, height: usize) {
    if width == 0 || height == 0 {
        return;
    }

    if dst_stride == width {
        let len = width * height;
        dst[..len].copy_from_slice(&src[..len]);
        return;
    }

    for row in 0..height {
        let dst_start = row * dst_stride;
        let src_start = row * width;
        dst[dst_start..dst_start + width].copy_from_slice(&src[src_start..src_start + width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(pattern: &mut TestPattern, width: usize, height: usize) -> Vec<u8> {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let (mut y, mut u, mut v) = (vec![0; width * height], vec![0; cw * ch], vec![0; cw * ch]);
        pattern.render(&mut y, width as i32, &mut u, cw as i32, &mut v, cw as i32);
        [y, u, v].concat()
    }

    #[test]
    fn static_pattern_repeats_and_moving_pattern_changes_deterministically() {
        for (width, height) in [(640, 360), (1280, 720), (33, 17)] {
            let mut bars = TestPattern::new(width as u32, height as u32);
            assert_eq!(render(&mut bars, width, height), render(&mut bars, width, height));

            let mut moving = TestPattern::moving(width as u32, height as u32);
            let first = render(&mut moving, width, height);
            let second = render(&mut moving, width, height);
            assert_ne!(first, second);
            let mut replay = TestPattern::moving(width as u32, height as u32);
            assert_eq!(render(&mut replay, width, height), first);
        }
        // Degenerate sizes must not panic.
        for (width, height) in [(1, 1), (2, 2), (3, 2)] {
            render(&mut TestPattern::moving(width as u32, height as u32), width, height);
        }
    }
}
