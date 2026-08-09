//! How far apart two renderings are.
//!
//! Deliberately two plain numbers rather than a perceptual metric. The best
//! Rust SSIM implementations (dssim, dssim-core) are AGPL and can never be a
//! dependency of this workspace; `image-compare` (MIT) would be the upgrade
//! path if a structural metric is ever wanted, and is not needed yet, because
//! the question the harness answers is coarse: does hayro draw roughly what
//! the reference rasterizer draws, and did that get worse?
//!
//! Both numbers are computed after a 4x box reduction. Two rasterizers never
//! agree pixel-for-pixel on an antialiased edge -- one of them lands the glyph
//! stem a third of a pixel to the left -- and at full resolution that noise
//! swamps the differences worth seeing (a missing image, an unpainted shading,
//! the wrong colour space). Averaging 4x4 blocks first forgives the edge and
//! keeps the area.

use image::{ImageBuffer, Rgba, RgbaImage};

use super::render::Rendered;

/// A pixel this far apart on any channel counts as "off": comfortably above
/// antialiasing noise, well below a visible difference in content.
const OFF_BY: f32 = 16.0;

/// How much to shrink before measuring.
const REDUCE: u32 = 4;

/// How two engines' renderings of one page differ.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divergence {
    /// Mean absolute difference per colour channel, on the 0-255 scale.
    pub mean_abs: f64,
    /// Fraction of (reduced) pixels differing by more than [`OFF_BY`] on some
    /// channel. Mean alone hides a small area that is completely wrong.
    pub frac_off: f64,
}

/// The overlapping region of two renderings, or `None` when they disagree
/// about the size of the page by more than a rounding.
///
/// hayro truncates a fractional pixel size and PDFium rounds it, so one pixel
/// of difference is expected and is cropped away; more than that means the two
/// engines read different page geometry, which is a finding in itself and not
/// something to average over.
fn common_size(a: &Rendered, b: &Rendered) -> Option<(u32, u32)> {
    let dw = a.width.abs_diff(b.width);
    let dh = a.height.abs_diff(b.height);
    if dw > 1 || dh > 1 {
        return None;
    }
    let (w, h) = (a.width.min(b.width), a.height.min(b.height));
    (w > 0 && h > 0).then_some((w, h))
}

pub fn compare(a: &Rendered, b: &Rendered) -> Option<Divergence> {
    let (w, h) = common_size(a, b)?;
    let (rw, rh, left) = reduce(a, w, h, REDUCE);
    let (_, _, right) = reduce(b, w, h, REDUCE);

    let mut total = 0.0f64;
    let mut off = 0u64;
    for (l, r) in left.chunks_exact(3).zip(right.chunks_exact(3)) {
        let mut worst = 0.0f32;
        for channel in 0..3 {
            let diff = (l[channel] - r[channel]).abs();
            total += f64::from(diff);
            worst = worst.max(diff);
        }
        if worst > OFF_BY {
            off += 1;
        }
    }
    let pixels = u64::from(rw) * u64::from(rh);
    if pixels == 0 {
        return None;
    }
    Some(Divergence {
        mean_abs: total / (pixels * 3) as f64,
        frac_off: off as f64 / pixels as f64,
    })
}

/// Crop to `w` x `h`, then average each `factor` x `factor` block into one RGB
/// triple. Alpha is ignored: every engine here draws onto opaque white.
fn reduce(page: &Rendered, w: u32, h: u32, factor: u32) -> (u32, u32, Vec<f32>) {
    let (rw, rh) = ((w / factor).max(1), (h / factor).max(1));
    let mut out = Vec::with_capacity((rw * rh * 3) as usize);
    let weight = (factor * factor) as f32;
    for block_y in 0..rh {
        for block_x in 0..rw {
            let mut sums = [0.0f32; 3];
            for y in 0..factor {
                let row = (block_y * factor + y).min(h - 1) as usize;
                for x in 0..factor {
                    let column = (block_x * factor + x).min(w - 1) as usize;
                    let at = (row * page.width as usize + column) * 4;
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += f32::from(page.rgba[at + channel]);
                    }
                }
            }
            out.extend(sums.iter().map(|sum| sum / weight));
        }
    }
    (rw, rh, out)
}

/// Three panels side by side: hayro, PDFium, and where they differ.
///
/// Half size, because the point of the picture is to see *which part of the
/// page* went wrong, and a full-resolution triptych of an A4 page at 1.5x is a
/// megabyte of PNG nobody scrolls through.
pub fn diff_image(hayro: &Rendered, pdfium: &Rendered) -> Option<RgbaImage> {
    let (w, h) = common_size(hayro, pdfium)?;
    let (pw, ph, left) = reduce(hayro, w, h, 2);
    let (_, _, right) = reduce(pdfium, w, h, 2);

    let mut out: RgbaImage = ImageBuffer::from_pixel(pw * 3 + 8, ph, Rgba([32, 32, 32, 255]));
    for y in 0..ph {
        for x in 0..pw {
            let at = ((y * pw + x) * 3) as usize;
            let l = &left[at..at + 3];
            let r = &right[at..at + 3];
            out.put_pixel(x, y, rgb(l));
            out.put_pixel(x + pw + 4, y, rgb(r));
            // White where the two agree, darkening fast where they do not:
            // amplified fourfold so a difference worth looking at is visible
            // on a screen rather than merely present in the file.
            let mut amplified = [0.0f32; 3];
            for channel in 0..3 {
                amplified[channel] = 255.0 - ((l[channel] - r[channel]).abs() * 4.0).min(255.0);
            }
            out.put_pixel(x + 2 * pw + 8, y, rgb(&amplified));
        }
    }
    Some(out)
}

fn rgb(channels: &[f32]) -> Rgba<u8> {
    Rgba([
        channels[0].round().clamp(0.0, 255.0) as u8,
        channels[1].round().clamp(0.0, 255.0) as u8,
        channels[2].round().clamp(0.0, 255.0) as u8,
        255,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(width: u32, height: u32, colour: [u8; 3]) -> Rendered {
        let rgba = (0..width * height)
            .flat_map(|_| [colour[0], colour[1], colour[2], 255])
            .collect();
        Rendered {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn identical_pages_do_not_diverge() {
        let page = flat(64, 64, [200, 100, 50]);
        let same = flat(64, 64, [200, 100, 50]);
        let divergence = compare(&page, &same).expect("comparable");
        assert_eq!(divergence.mean_abs, 0.0);
        assert_eq!(divergence.frac_off, 0.0);
    }

    /// A difference below the threshold counts towards the mean and not
    /// towards the fraction: that is the whole reason there are two numbers.
    #[test]
    fn a_small_difference_moves_the_mean_only() {
        let page = flat(64, 64, [100, 100, 100]);
        let shifted = flat(64, 64, [108, 100, 100]);
        let divergence = compare(&page, &shifted).expect("comparable");
        assert!(
            (divergence.mean_abs - 8.0 / 3.0).abs() < 1e-6,
            "{divergence:?}"
        );
        assert_eq!(divergence.frac_off, 0.0);
    }

    #[test]
    fn a_black_page_against_a_white_one_is_completely_off() {
        let page = flat(64, 64, [0, 0, 0]);
        let inverse = flat(64, 64, [255, 255, 255]);
        let divergence = compare(&page, &inverse).expect("comparable");
        assert_eq!(divergence.mean_abs, 255.0);
        assert_eq!(divergence.frac_off, 1.0);
    }

    /// One pixel of size difference is rounding and gets cropped; more than
    /// that is two engines disagreeing about the page, which is not a number.
    #[test]
    fn sizes_may_differ_by_a_rounding_and_no_more() {
        let page = flat(64, 64, [255, 255, 255]);
        assert!(compare(&page, &flat(65, 64, [255, 255, 255])).is_some());
        assert!(compare(&page, &flat(66, 64, [255, 255, 255])).is_none());
        assert!(compare(&page, &flat(0, 0, [255, 255, 255])).is_none());
    }

    #[test]
    fn the_diff_image_is_three_panels_wide() {
        let page = flat(64, 32, [255, 255, 255]);
        let image = diff_image(&page, &flat(64, 32, [0, 0, 0])).expect("comparable");
        assert_eq!(image.height(), 16);
        assert_eq!(image.width(), 32 * 3 + 8);
        // Every channel differs by 255, so the difference panel is black.
        assert_eq!(image.get_pixel(32 * 2 + 8, 0), &Rgba([0, 0, 0, 255]));
        // ... and the first panel is the white one it was given.
        assert_eq!(image.get_pixel(0, 0), &Rgba([255, 255, 255, 255]));
    }
}
