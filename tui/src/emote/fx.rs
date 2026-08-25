//! effect pixels — BTTV `w!`-style prefixes and FFZ `ffzX`-style words, applied
//! to a composited emote stack's RGBA frames before scaling/encoding. core
//! parses the grammar ([`Effect`]); this is the render half.
//!
//! effects apply in enum order (geometry → color → motion), which core already
//! sorted the list into. rainbow and shake ANIMATE a static emote: they
//! synthesize a short loop first, then treat it like any animation — the rest
//! of the pipeline (subsample, palette share, per-frame encode) is unchanged.

use heatsync_core::emote::Effect;
use image::imageops::FilterType;
use image::RgbaImage;

/// frames a synthesized (rainbow/shake) loop runs over — enough for a smooth
/// hue sweep, few enough to stay cheap on a passive-cooled box.
const SPIN_FRAMES: usize = 18;
const SPIN_DELAY_MS: u32 = 70;
const SHAKE_FRAMES: usize = 6;
const SHAKE_DELAY_MS: u32 = 40;

/// apply `fx` (sorted, from the stack key) to composited frames.
pub fn apply(mut frames: Vec<(RgbaImage, u32)>, fx: &[Effect]) -> Vec<(RgbaImage, u32)> {
    for &f in fx {
        match f {
            Effect::RotateL => each(&mut frames, image::imageops::rotate270),
            Effect::RotateR => each(&mut frames, image::imageops::rotate90),
            Effect::FlipX => each(&mut frames, image::imageops::flip_horizontal),
            Effect::FlipY => each(&mut frames, image::imageops::flip_vertical),
            Effect::Wide => each(&mut frames, |i| {
                let (w, h) = i.dimensions();
                image::imageops::resize(i, w.saturating_mul(2).max(1), h, FilterType::Triangle)
            }),
            Effect::Cursed => {
                for (img, _) in &mut frames {
                    curse(img);
                }
            }
            Effect::Rainbow => frames = rainbow(frames),
            Effect::Shake => frames = shake(frames),
        }
    }
    frames
}

fn each(frames: &mut [(RgbaImage, u32)], f: impl Fn(&RgbaImage) -> RgbaImage) {
    for (img, _) in frames.iter_mut() {
        *img = f(img);
    }
}

/// grayscale + darken, alpha untouched — the "cursed" look.
fn curse(img: &mut RgbaImage) {
    for px in img.pixels_mut() {
        // rec.601 luma, then dimmed to ~55%
        let l = (px[0] as u32 * 299 + px[1] as u32 * 587 + px[2] as u32 * 114) / 1000;
        let d = (l * 55 / 100) as u8;
        px[0] = d;
        px[1] = d;
        px[2] = d;
    }
}

/// make sure a loop exists to paint an over-time effect onto: a static emote
/// becomes `n` copies at `delay_ms` each; an animation is used as-is.
fn ensure_loop(frames: Vec<(RgbaImage, u32)>, n: usize, delay_ms: u32) -> Vec<(RgbaImage, u32)> {
    if frames.len() > 1 {
        return frames;
    }
    let Some((img, _)) = frames.into_iter().next() else {
        return Vec::new();
    };
    (0..n).map(|_| (img.clone(), delay_ms)).collect()
}

/// hue-cycle across the loop: frame k is rotated k/N of a full turn, so the
/// loop point is seamless.
fn rainbow(frames: Vec<(RgbaImage, u32)>) -> Vec<(RgbaImage, u32)> {
    let frames = ensure_loop(frames, SPIN_FRAMES, SPIN_DELAY_MS);
    let n = frames.len().max(1);
    frames
        .into_iter()
        .enumerate()
        .map(|(k, (img, d))| {
            let deg = (k * 360 / n) as i32;
            (image::imageops::huerotate(&img, deg), d)
        })
        .collect()
}

/// jitter around the origin: each frame shifts by a small fixed offset, empty
/// edge pixels transparent. deterministic — a cache identity must always build
/// the same bytes.
fn shake(frames: Vec<(RgbaImage, u32)>) -> Vec<(RgbaImage, u32)> {
    const PATTERN: [(i64, i64); 6] = [(0, 0), (1, -1), (-1, 1), (1, 1), (-1, 0), (0, 1)];
    let frames = ensure_loop(frames, SHAKE_FRAMES, SHAKE_DELAY_MS);
    frames
        .into_iter()
        .enumerate()
        .map(|(k, (img, d))| {
            let (w, h) = img.dimensions();
            // ~6% of the emote's size, at least one pixel — visible at 32px,
            // proportional at 128px.
            let amp = (w.min(h) as i64 / 16).max(1);
            let (dx, dy) = PATTERN[k % PATTERN.len()];
            let mut canvas = RgbaImage::new(w, h);
            image::imageops::replace(&mut canvas, &img, dx * amp, dy * amp);
            (canvas, d)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn px(r: u8, g: u8, b: u8) -> Vec<(RgbaImage, u32)> {
        vec![(RgbaImage::from_pixel(4, 8, Rgba([r, g, b, 255])), 0)]
    }

    #[test]
    fn wide_doubles_width_only() {
        let out = apply(px(9, 9, 9), &[Effect::Wide]);
        assert_eq!(out[0].0.dimensions(), (8, 8));
    }

    #[test]
    fn rotate_swaps_dimensions() {
        for fx in [Effect::RotateL, Effect::RotateR] {
            let out = apply(px(9, 9, 9), &[fx]);
            assert_eq!(out[0].0.dimensions(), (8, 4), "{fx:?}");
        }
    }

    #[test]
    fn flips_mirror_the_pixels() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
        let out = apply(vec![(img, 0)], &[Effect::FlipX]);
        assert_eq!(out[0].0.get_pixel(0, 0), &Rgba([0, 255, 0, 255]));
    }

    #[test]
    fn cursed_is_gray_and_darker() {
        let out = apply(px(200, 100, 50), &[Effect::Cursed]);
        let p = out[0].0.get_pixel(0, 0);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
        assert!(p[0] < 130, "darkened: {}", p[0]);
        assert_eq!(p[3], 255, "alpha untouched");
    }

    #[test]
    fn rainbow_animates_a_static_emote() {
        let out = apply(px(255, 0, 0), &[Effect::Rainbow]);
        assert_eq!(out.len(), SPIN_FRAMES);
        // hue actually moves across the loop
        assert_ne!(
            out[0].0.get_pixel(0, 0),
            out[SPIN_FRAMES / 2].0.get_pixel(0, 0)
        );
    }

    #[test]
    fn shake_animates_and_keeps_dimensions() {
        let out = apply(px(9, 9, 9), &[Effect::Shake]);
        assert_eq!(out.len(), SHAKE_FRAMES);
        assert_eq!(out[0].0.dimensions(), (4, 8));
        // a shifted frame has a transparent edge the origin frame doesn't
        assert_ne!(out[0].0, out[1].0);
    }

    #[test]
    fn effects_are_deterministic() {
        let a = apply(px(10, 200, 30), &[Effect::Shake, Effect::Rainbow]);
        let b = apply(px(10, 200, 30), &[Effect::Shake, Effect::Rainbow]);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.0, y.0);
        }
    }

    #[test]
    fn empty_input_survives_every_effect() {
        for fx in [
            Effect::RotateL,
            Effect::Wide,
            Effect::Cursed,
            Effect::Rainbow,
            Effect::Shake,
        ] {
            assert!(apply(Vec::new(), &[fx]).is_empty());
        }
    }
}
