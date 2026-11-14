//! Live image enhancement without AI: multi-frame fusion plus detail sharpening.
//!
//! Each new frame is aligned to the running result (global shift, coarse to fine),
//! then blended per pixel: where the scene is static up to `MAX_FRAMES` frames are
//! averaged (noise and JPEG artefacts go down), where it moved the new frame wins
//! (no ghosting). Finally fine detail and local contrast are boosted on luma only.
//! Real detail only: nothing is invented.

use image::RgbImage;
use rayon::prelude::*;
use std::time::{Duration, Instant};

// Tuning knobs (8-bit units). Calibrated on synthetic noise; retune on real device footage.
/// Most frames averaged at one pixel: more = cleaner but slower to react.
const MAX_FRAMES: f32 = 8.0;
/// Luma difference (3×3 mean) below which a pixel is "static"…
const STILL: f32 = 6.0;
/// …and above which it "moved" and restarts from the new frame.
const MOVED: f32 = 18.0;
/// Fine-detail gain (unsharp mask, radius ~1 px) and noise floor it ignores.
const DETAIL: f32 = 1.2;
const DETAIL_FLOOR: f32 = 1.5;
/// Local-contrast gain (radius ~8 px): makes structures pop without global contrast.
const CLARITY: f32 = 0.35;
/// Largest motion between frames, in 1/8-resolution pixels (×8 at full size).
const SEARCH: i32 = 4;

/// Fused red, green, blue, number of frames merged, luma.
type Px = [f32; 5];

#[derive(Default)]
pub struct Enhancer {
    w: usize,
    h: usize,
    acc: Vec<Px>,
    // Reused every frame: fresh multi-megabyte buffers cost more than the maths.
    back: Vec<Px>,
    cur_luma: Vec<f32>,
    last: Option<Instant>,
}

impl Enhancer {
    /// Forgets the history (e.g. when enhancement is switched back on).
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Enhanced version of the next frame of the stream.
    pub fn process(&mut self, img: &RgbImage) -> RgbImage {
        self.fuse(img);
        sharpen(&self.acc, self.w, self.h)
    }

    /// Temporal fusion only (exposed for tests).
    fn fuse(&mut self, img: &RgbImage) {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let raw = img.as_raw();
        let cur = |i: usize| [raw[3 * i] as f32, raw[3 * i + 1] as f32, raw[3 * i + 2] as f32];
        par_fill(&mut self.cur_luma, w, h, |i| luma(&cur(i)));
        let cur_luma = &self.cur_luma;
        let stale = self.last.is_none_or(|t| t.elapsed() > Duration::from_millis(500));
        self.last = Some(Instant::now());
        if stale || (w, h) != (self.w, self.h) || w < 64 || h < 64 {
            (self.w, self.h) = (w, h);
            par_fill(&mut self.acc, w, h, |i| {
                let [r, g, b] = cur(i);
                [r, g, b, 1.0, cur_luma[i]]
            });
            return;
        }
        let acc = &self.acc;
        let (dx, dy) = estimate_shift(|i| acc[i][4], |i| cur_luma[i], w, h);

        // Pixel i of the new frame continues history pixel i - off; rows and columns
        // that come into view this frame (outside the shifted history) start fresh.
        let off = dy as isize * w as isize + dx as isize;
        let xs = (dx.max(0) as usize).min(w)..((w as i32 + dx).clamp(0, w as i32) as usize);
        let ys = (dy.max(0) as usize).min(h)..((h as i32 + dy).clamp(0, h as i32) as usize);
        let src = |x: usize, y: usize| (ys.contains(&y) && xs.contains(&x)).then(|| ((y * w + x) as isize - off) as usize);
        let diff = |x: usize, y: usize| src(x, y).map_or(MOVED, |s| (cur_luma[y * w + x] - acc[s][4]).abs());

        par_fill(&mut self.back, w, h, |i| {
            let (x, y) = (i % w, i / w);
            let [r, g, b] = cur(i);
            let Some(s) = src(x, y) else { return [r, g, b, 1.0, cur_luma[i]] };
            // 3×3 mean difference, so that sensor noise alone does not look like motion.
            let (mut sum, mut cnt) = (0.0, 0.0);
            for yy in y.saturating_sub(1)..(y + 2).min(h) {
                for xx in x.saturating_sub(1)..(x + 2).min(w) {
                    sum += diff(xx, yy);
                    cnt += 1.0;
                }
            }
            let a = acc[s];
            let trust = ((MOVED - sum / cnt) / (MOVED - STILL)).clamp(0.0, 1.0);
            let k = (a[3] * trust).min(MAX_FRAMES - 1.0);
            let mix = |old: f32, new: f32| (old * k + new) / (k + 1.0);
            let p = [mix(a[0], r), mix(a[1], g), mix(a[2], b)];
            [p[0], p[1], p[2], k + 1.0, luma(&p)]
        });
        std::mem::swap(&mut self.acc, &mut self.back);
    }
}

fn luma(p: &[f32]) -> f32 {
    0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]
}

/// Runs `f(first_row, band)` on bands of 16 rows of `out` (rows of `w` items) on rayon's pool.
fn par_rows<T: Send>(out: &mut [T], w: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    const ROWS: usize = 16;
    out.par_chunks_mut(ROWS * w.max(1)).enumerate().for_each(|(b, band)| f(b * ROWS, band));
}

/// Fills `out` (resized to w*h, keeping its allocation) with `f(index)`, in parallel.
fn par_fill<T: Send + Clone + Default>(out: &mut Vec<T>, w: usize, h: usize, f: impl Fn(usize) -> T + Sync) {
    out.resize(w * h, T::default());
    par_rows(out, w, |y0, band| {
        for (j, o) in band.iter_mut().enumerate() {
            *o = f(y0 * w + j);
        }
    });
}

fn par_map<T: Send + Clone + Default>(w: usize, h: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let mut out = Vec::new();
    par_fill(&mut out, w, h, f);
    out
}

/// Global translation (dx, dy) such that `cur(x, y) ≈ prev(x - dx, y - dy)`:
/// wide search at 1/8 size, then ±1 refinement at 1/4, 1/2 and full size.
fn estimate_shift(prev: impl Fn(usize) -> f32 + Sync, cur: impl Fn(usize) -> f32 + Sync, w: usize, h: usize) -> (i32, i32) {
    let (p1, c1) = (half(&prev, w, h), half(&cur, w, h));
    let (p2, c2) = (half(|i| p1[i], w / 2, h / 2), half(|i| c1[i], w / 2, h / 2));
    let (p3, c3) = (half(|i| p2[i], w / 4, h / 4), half(|i| c2[i], w / 4, h / 4));
    let d = best_shift(|i| p3[i], |i| c3[i], w / 8, h / 8, (0, 0), SEARCH, 1);
    let d = best_shift(|i| p2[i], |i| c2[i], w / 4, h / 4, (d.0 * 2, d.1 * 2), 1, 1);
    let d = best_shift(|i| p1[i], |i| c1[i], w / 2, h / 2, (d.0 * 2, d.1 * 2), 1, 1);
    best_shift(prev, cur, w, h, (d.0 * 2, d.1 * 2), 1, 2)
}

/// Shift within `center ± radius` with the lowest mean absolute difference.
fn best_shift(
    prev: impl Fn(usize) -> f32 + Sync,
    cur: impl Fn(usize) -> f32 + Sync,
    w: usize,
    h: usize,
    center: (i32, i32),
    radius: i32,
    step: usize,
) -> (i32, i32) {
    let m = (center.0.abs().max(center.1.abs()) + radius) as usize;
    if 2 * m + 8 >= w.min(h) {
        return (0, 0);
    }
    let side = 2 * radius + 1;
    let shift = |k: usize| (center.0 - radius + k as i32 % side, center.1 - radius + k as i32 / side);
    let sads: Vec<f32> = (0..(side * side) as usize).into_par_iter().map(|k| {
        let (dx, dy) = shift(k);
        let mut sad = 0.0;
        for y in (m..h - m).step_by(step) {
            let (rc, rp) = (y * w, (y as i32 - dy) as usize * w);
            for x in (m..w - m).step_by(step) {
                sad += (cur(rc + x) - prev(rp + (x as i32 - dx) as usize)).abs();
            }
        }
        // Ties go to the smallest motion, so flat scenes stay put.
        sad * (1.0 + 1e-4 * (dx.abs() + dy.abs()) as f32)
    }).collect();
    let best = (0..sads.len()).min_by(|&a, &b| sads[a].total_cmp(&sads[b])).unwrap_or(0);
    shift(best)
}

/// 2×2 average downscale.
fn half(src: impl Fn(usize) -> f32 + Sync, w: usize, h: usize) -> Vec<f32> {
    let w2 = w / 2;
    par_map(w2, h / 2, |i| {
        let j = 2 * (i / w2) * w + 2 * (i % w2);
        (src(j) + src(j + 1) + src(j + w) + src(j + w + 1)) * 0.25
    })
}

/// 3×3 box blur with clamped edges (for the small 1/4-size images).
fn blur3(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    par_map(w, h, |i| {
        let (x, y) = (i % w, i / w);
        let (mut sum, mut cnt) = (0.0, 0.0);
        for yy in y.saturating_sub(1)..(y + 2).min(h) {
            for xx in x.saturating_sub(1)..(x + 2).min(w) {
                sum += src[yy * w + xx];
                cnt += 1.0;
            }
        }
        sum / cnt
    })
}

/// Boosts fine detail (above the noise floor) and local contrast, on luma only
/// so colours do not fringe.
fn sharpen(px: &[Px], w: usize, h: usize) -> RgbImage {
    // Local mean over ~8 px, computed at 1/4 size and read back bilinearly.
    let (qw, qh) = (w / 4, h / 4);
    let q1 = half(|i| px[i][4], w, h);
    let q = half(|i| q1[i], w / 2, h / 2);
    let q = blur3(&blur3(&q, qw, qh), qw, qh);
    let coarse = |x: usize, y: usize| {
        let fx = ((x as f32 + 0.5) / 4.0 - 0.5).clamp(0.0, (qw - 1) as f32);
        let fy = ((y as f32 + 0.5) / 4.0 - 0.5).clamp(0.0, (qh - 1) as f32);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(qw - 1), (y0 + 1).min(qh - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let row = |y: usize| q[y * qw + x0] * (1.0 - tx) + q[y * qw + x1] * tx;
        row(y0) * (1.0 - ty) + row(y1) * ty
    };
    let lum = |x: usize, y: usize| px[y.min(h - 1) * w + x.min(w - 1)][4];
    let mut out = vec![0u8; w * h * 3];
    par_rows(&mut out, w * 3, |y0, band| {
        for (k, row) in band.chunks_exact_mut(w * 3).enumerate() {
            let y = y0 + k;
            let (yu, yd) = (y.saturating_sub(1), y + 1);
            for x in 0..w {
                let (xl, xr) = (x.saturating_sub(1), x + 1);
                // 3×3 binomial blur = the "smooth" image the fine detail is measured against.
                let fine = (lum(xl, yu) + lum(xr, yu) + lum(xl, yd) + lum(xr, yd)
                    + 2.0 * (lum(x, yu) + lum(x, yd) + lum(xl, y) + lum(xr, y))
                    + 4.0 * lum(x, y))
                    / 16.0;
                let p = px[y * w + x];
                let d = p[4] - fine;
                let d = d.signum() * (d.abs() - DETAIL_FLOOR).max(0.0);
                let delta = DETAIL * d + CLARITY * (fine - coarse(x, y));
                for j in 0..3 {
                    row[3 * x + j] = (p[j] + delta).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    });
    RgbImage::from_raw(w as u32, h as u32, out).expect("w*h*3 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic texture with structure at several scales.
    fn scene(x: i32, y: i32) -> f32 {
        let h = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) as u32;
        let (fx, fy) = (x as f32, y as f32);
        128.0 + 50.0 * (fx * 0.11).sin() * (fy * 0.07).cos() + 30.0 * ((h >> 8) % 3) as f32 - 30.0
    }

    fn frame(shift: (i32, i32), seed: u32) -> (RgbImage, RgbImage) {
        let mut rng = seed.wrapping_mul(2_654_435_761) | 1;
        let mut noise = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng % 25) as f32 - 12.0 // uniform ±12, σ ≈ 7
        };
        let clean = RgbImage::from_fn(160, 120, |x, y| {
            let v = scene(x as i32 - shift.0, y as i32 - shift.1).round() as u8;
            image::Rgb([v; 3])
        });
        let noisy = RgbImage::from_fn(160, 120, |x, y| {
            let v = clean.get_pixel(x, y).0[0] as f32 + noise();
            image::Rgb([v.clamp(0.0, 255.0) as u8; 3])
        });
        (clean, noisy)
    }

    fn rmse(a: &[Px], b: &RgbImage) -> f32 {
        let s: f32 = a.iter().zip(b.pixels()).map(|(p, q)| (p[0] - q.0[0] as f32).powi(2)).sum();
        (s / a.len() as f32).sqrt()
    }

    #[test]
    fn finds_the_hand_motion() {
        let (a, _) = frame((0, 0), 1);
        let (b, _) = frame((7, -3), 2);
        let f = |i: &RgbImage| i.pixels().map(|p| p.0[0] as f32).collect::<Vec<_>>();
        let (fa, fb) = (f(&a), f(&b));
        assert_eq!(estimate_shift(|i| fa[i], |i| fb[i], 160, 120), (7, -3));
    }

    #[test]
    fn fusion_removes_noise_while_the_camera_moves() {
        let mut e = Enhancer::default();
        let mut last = (0.0, 0.0);
        for i in 0..10 {
            let (clean, noisy) = frame((i * 2, -i), i as u32 + 1);
            e.fuse(&noisy);
            let single: Vec<Px> = noisy.pixels().map(|p| [p.0[0] as f32, 0.0, 0.0, 0.0, 0.0]).collect();
            last = (rmse(&single, &clean), rmse(&e.acc, &clean));
        }
        let (single, fused) = last;
        assert!(fused < single * 0.6, "single frame {single:.2}, fused {fused:.2}");
    }

    #[test]
    fn a_moving_object_leaves_no_ghost() {
        let mut e = Enhancer::default();
        let flat = RgbImage::from_pixel(96, 96, image::Rgb([60; 3]));
        for _ in 0..8 {
            e.fuse(&flat);
        }
        let mut moved = flat.clone();
        for y in 40..60 {
            for x in 40..60 {
                moved.put_pixel(x, y, image::Rgb([220; 3]));
            }
        }
        e.fuse(&moved);
        assert!(e.acc[50 * 96 + 50][0] > 200.0, "object faded into the history");
    }
}
