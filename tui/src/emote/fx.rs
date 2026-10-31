//! effect pixels — BTTV `w!`-style prefixes and FFZ `ffzX`-style words, applied
//! to a composited emote stack's RGBA frames before scaling/encoding. core
//! parses the grammar ([`Effect`]); this is the render half.
//!
//! each layer of a stack carries its own effects, applied in enum order
//! (geometry → color → motion — core already sorted them), with geometry
//! composed (scales multiply, rotations add). rainbow, shake and the ffz motion
//! effects ANIMATE a static emote: they synthesize a short loop first, then
//! treat it like any animation — the rest of the pipeline (subsample, palette
//! share, per-frame encode) is unchanged.

use std::f32::consts::TAU;

use heatsync_core::emote::{compose, Effect, Geometry};
use image::imageops::FilterType;
use image::RgbaImage;

/// frames a synthesized (rainbow/shake) loop runs over — enough for a smooth
/// hue sweep, few enough to stay cheap on a passive-cooled box.
const SPIN_FRAMES: usize = 18;
const SPIN_DELAY_MS: u32 = 70;
const SHAKE_FRAMES: usize = 6;
const SHAKE_DELAY_MS: u32 = 40;

/// apply one layer's effects (sorted, from the stack key) and tint to its
/// frames: composed geometry first, then color, the hue tint, then motion.
pub fn apply(
    frames: Vec<(RgbaImage, u32)>,
    fx: &[Effect],
    hue: Option<u16>,
) -> Vec<(RgbaImage, u32)> {
    let mut frames = geometry(frames, compose(fx));
    for &f in fx.iter().filter(|f| !f.is_motion()) {
        match f {
            Effect::Cursed => each_mut(&mut frames, curse),
            Effect::Hyper => each_mut(&mut frames, hyper),
            Effect::Party => {
                each_mut(&mut frames, party);
                frames = rainbow(frames);
            }
            Effect::Rainbow => frames = rainbow(frames),
            _ => {} // geometry — composed above
        }
    }
    if let Some(deg) = hue {
        each_mut(&mut frames, |img| {
            *img = image::imageops::huerotate(img, deg as i32);
            saturate(img, 1.6);
        });
    }
    for &f in fx.iter().filter(|f| f.is_motion()) {
        frames = match f {
            Effect::Shake => shake(frames),
            Effect::Bounce => animate(frames, |img, t| {
                let lift = (img.height() as f32 * 0.2 * (t * TAU).sin().abs()).round();
                place(img, 1.0, 1.0, 0.0, -lift)
            }),
            Effect::Jam => animate(frames, |img, t| {
                let k = 0.15 * (t * TAU).sin();
                place(img, 1.0 + k, 1.0 - k, 0.0, 0.0)
            }),
            Effect::Slide => animate(frames, |img, t| {
                let p = ease_in(t);
                place(img, 1.0, 1.0, -(img.width() as f32) * (1.0 - p), 0.0)
            }),
            Effect::Arrive => animate(frames, |img, t| {
                let p = ease_in(t);
                place(img, p, p, 0.0, 0.0)
            }),
            Effect::Leave => animate(frames, |img, t| {
                let p = 1.0 - ease_out(t);
                place(img, p, p, 0.0, 0.0)
            }),
            Effect::Spin => animate(frames, |img, t| spin(img, t * TAU)),
            _ => frames,
        };
    }
    frames
}

/// the composed rotate/mirror/scale (see core's [`compose`]): rotation first,
/// then mirror, then integer stretch — the extension's `scale() rotate()`.
fn geometry(mut frames: Vec<(RgbaImage, u32)>, g: Geometry) -> Vec<(RgbaImage, u32)> {
    if g == (Geometry {
        sx: 1,
        sy: 1,
        rot: 0,
    }) {
        return frames;
    }
    each(&mut frames, |i| {
        let mut out = match g.rot {
            90 => image::imageops::rotate90(i),
            180 => image::imageops::rotate180(i),
            270 => image::imageops::rotate270(i),
            _ => i.clone(),
        };
        if g.sx < 0 {
            image::imageops::flip_horizontal_in_place(&mut out);
        }
        if g.sy < 0 {
            image::imageops::flip_vertical_in_place(&mut out);
        }
        let (w, h) = out.dimensions();
        let (nw, nh) = (
            w.saturating_mul(g.sx.unsigned_abs()).max(1),
            h.saturating_mul(g.sy.unsigned_abs()).max(1),
        );
        if (nw, nh) != (w, h) {
            out = image::imageops::resize(&out, nw, nh, FilterType::Triangle);
        }
        out
    });
    frames
}

fn each(frames: &mut [(RgbaImage, u32)], f: impl Fn(&RgbaImage) -> RgbaImage) {
    for (img, _) in frames.iter_mut() {
        *img = f(img);
    }
}

fn each_mut(frames: &mut [(RgbaImage, u32)], f: impl Fn(&mut RgbaImage)) {
    for (img, _) in frames.iter_mut() {
        f(img);
    }
}

/// scale each pixel's chroma away from / toward its luma, alpha untouched.
fn saturate(img: &mut RgbaImage, k: f32) {
    for px in img.pixels_mut() {
        let l = px[0] as f32 * 0.299 + px[1] as f32 * 0.587 + px[2] as f32 * 0.114;
        for c in &mut px.0[..3] {
            *c = (l + (*c as f32 - l) * k).clamp(0.0, 255.0) as u8;
        }
    }
}

/// push contrast around mid-gray, alpha untouched.
fn contrast(img: &mut RgbaImage, k: f32) {
    for px in img.pixels_mut() {
        for c in &mut px.0[..3] {
            *c = ((*c as f32 - 128.0) * k + 128.0).clamp(0.0, 255.0) as u8;
        }
    }
}

/// the loud color half of ffzHyper (its shake is a motion effect).
fn hyper(img: &mut RgbaImage) {
    saturate(img, 3.0);
    contrast(img, 1.5);
}

/// the color half of p!: half-sepia, then oversaturated (the hue cycle is a
/// loop, added after).
fn party(img: &mut RgbaImage) {
    for px in img.pixels_mut() {
        let [r, g, b] = [px[0] as f32, px[1] as f32, px[2] as f32];
        let sepia = [
            r * 0.393 + g * 0.769 + b * 0.189,
            r * 0.349 + g * 0.686 + b * 0.168,
            r * 0.272 + g * 0.534 + b * 0.131,
        ];
        for (c, s) in px.0[..3].iter_mut().zip(sepia) {
            *c = ((*c as f32 + s) / 2.0).clamp(0.0, 255.0) as u8;
        }
    }
    saturate(img, 2.5);
}

/// run `f(frame, t)` over a looping timeline, t = k/N in [0,1). a static emote
/// first becomes a loop; an animation keeps its own frames and delays.
fn animate(
    frames: Vec<(RgbaImage, u32)>,
    f: impl Fn(&RgbaImage, f32) -> RgbaImage,
) -> Vec<(RgbaImage, u32)> {
    let frames = ensure_loop(frames, SPIN_FRAMES, SPIN_DELAY_MS);
    let n = frames.len().max(1) as f32;
    frames
        .into_iter()
        .enumerate()
        .map(|(k, (img, d))| (f(&img, k as f32 / n), d))
        .collect()
}

/// 0→1 over the first half of the loop, then held.
fn ease_in(t: f32) -> f32 {
    (t * 2.0).min(1.0)
}

/// 0 over the first half of the loop, then 0→1.
fn ease_out(t: f32) -> f32 {
    ((t - 0.5) * 2.0).max(0.0)
}

/// the frame scaled about its center by (sx, sy) and shifted by (dx, dy), on a
/// transparent canvas of the SAME size (clipped at the edges).
fn place(img: &RgbaImage, sx: f32, sy: f32, dx: f32, dy: f32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let mut canvas = RgbaImage::new(w, h);
    let (nw, nh) = ((w as f32 * sx).round(), (h as f32 * sy).round());
    if nw < 1.0 || nh < 1.0 {
        return canvas;
    }
    let scaled;
    let src = if (nw as u32, nh as u32) == (w, h) {
        img
    } else {
        scaled = image::imageops::resize(img, nw as u32, nh as u32, FilterType::Triangle);
        &scaled
    };
    let x = ((w as f32 - nw) / 2.0 + dx).round() as i64;
    let y = ((h as f32 - nh) / 2.0 + dy).round() as i64;
    image::imageops::replace(&mut canvas, src, x, y);
    canvas
}

/// the frame turned `rad` clockwise about its center, same canvas size
/// (corners clip). nearest-neighbor — emotes are tiny and it stays cheap.
fn spin(img: &RgbaImage, rad: f32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let mut out = RgbaImage::new(w, h);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let (sin, cos) = rad.sin_cos();
    for y in 0..h {
        for x in 0..w {
            // inverse-rotate the destination pixel back into the source
            let (px, py) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let (sx, sy) = (cx + px * cos + py * sin, cy - px * sin + py * cos);
            if sx >= 0.0 && sy >= 0.0 && (sx as u32) < w && (sy as u32) < h {
                out.put_pixel(x, y, *img.get_pixel(sx as u32, sy as u32));
            }
        }
    }
    out
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

    fn run(fx: &[Effect]) -> Vec<(RgbaImage, u32)> {
        apply(px(9, 9, 9), fx, None)
    }

    /// two-pixel strip: red | green, for mirror / rotation checks.
    fn strip() -> Vec<(RgbaImage, u32)> {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
        vec![(img, 0)]
    }

    #[test]
    fn wide_and_tall_stretch_one_axis() {
        assert_eq!(run(&[Effect::Wide])[0].0.dimensions(), (8, 8));
        assert_eq!(run(&[Effect::Tall])[0].0.dimensions(), (4, 16));
        assert_eq!(
            run(&[Effect::Wide, Effect::Tall])[0].0.dimensions(),
            (8, 16)
        );
    }

    #[test]
    fn stretch_composes_and_clamps_at_four() {
        assert_eq!(run(&[Effect::Wide; 2])[0].0.dimensions(), (16, 8));
        assert_eq!(run(&[Effect::Wide; 3])[0].0.dimensions(), (16, 8)); // 8 → 4
                                                                        // a mirror before a stretch keeps the magnitude
        assert_eq!(
            run(&[Effect::FlipX, Effect::Wide])[0].0.dimensions(),
            (8, 8)
        );
    }

    #[test]
    fn rotate_swaps_dimensions() {
        for fx in [Effect::RotateL, Effect::RotateR] {
            assert_eq!(run(&[fx])[0].0.dimensions(), (8, 4), "{fx:?}");
        }
    }

    #[test]
    fn rotations_add() {
        // L+L = 180: dims kept, strip reversed
        let out = apply(strip(), &[Effect::RotateL, Effect::RotateL], None);
        assert_eq!(out[0].0.dimensions(), (2, 1));
        assert_eq!(out[0].0.get_pixel(0, 0), &Rgba([0, 255, 0, 255]));
        // L+R cancel
        let out = apply(strip(), &[Effect::RotateL, Effect::RotateR], None);
        assert_eq!(out[0].0.get_pixel(0, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn flips_mirror_and_cancel() {
        let out = apply(strip(), &[Effect::FlipX], None);
        assert_eq!(out[0].0.get_pixel(0, 0), &Rgba([0, 255, 0, 255]));
        let out = apply(strip(), &[Effect::FlipX, Effect::FlipX], None);
        assert_eq!(out[0].0.get_pixel(0, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn cursed_is_gray_and_darker() {
        let out = apply(px(200, 100, 50), &[Effect::Cursed], None);
        let p = out[0].0.get_pixel(0, 0);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
        assert!(p[0] < 130, "darkened: {}", p[0]);
        assert_eq!(p[3], 255, "alpha untouched");
    }

    #[test]
    fn hyper_pushes_color_and_shakes_via_its_own_word_only() {
        let base = px(200, 100, 50);
        let out = apply(base.clone(), &[Effect::Hyper], None);
        assert_eq!(out.len(), 1, "hyper is color only");
        let (a, b) = (base[0].0.get_pixel(0, 0), out[0].0.get_pixel(0, 0));
        let spread = |p: &Rgba<u8>| p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
        assert!(spread(b) > spread(a), "more saturated");
        assert_eq!(b[3], 255);
    }

    #[test]
    fn rainbow_animates_a_static_emote() {
        let out = apply(px(255, 0, 0), &[Effect::Rainbow], None);
        assert_eq!(out.len(), SPIN_FRAMES);
        // hue actually moves across the loop
        assert_ne!(
            out[0].0.get_pixel(0, 0),
            out[SPIN_FRAMES / 2].0.get_pixel(0, 0)
        );
    }

    #[test]
    fn party_cycles_hue_too() {
        let out = apply(px(255, 0, 0), &[Effect::Party], None);
        assert_eq!(out.len(), SPIN_FRAMES);
        assert_ne!(
            out[0].0.get_pixel(0, 0),
            out[SPIN_FRAMES / 2].0.get_pixel(0, 0)
        );
    }

    #[test]
    fn hue_tint_shifts_color_and_keeps_alpha() {
        let out = apply(px(255, 0, 0), &[], Some(120));
        let p = out[0].0.get_pixel(0, 0);
        assert!(p[1] > p[0], "red rotated toward green: {p:?}");
        assert_eq!(p[3], 255);
        assert_eq!(out.len(), 1, "tint is static");
    }

    #[test]
    fn shake_animates_and_keeps_dimensions() {
        let out = run(&[Effect::Shake]);
        assert_eq!(out.len(), SHAKE_FRAMES);
        assert_eq!(out[0].0.dimensions(), (4, 8));
        // a shifted frame has a transparent edge the origin frame doesn't
        assert_ne!(out[0].0, out[1].0);
    }

    /// every over-time effect: loops a static emote, keeps the canvas size,
    /// and actually changes across the loop.
    #[test]
    fn motion_effects_loop_and_move() {
        let mut img = RgbaImage::new(16, 16);
        for y in 0..16 {
            for x in 0..16 {
                // asymmetric so spin / slide / bounce are visible
                let a = if x < 10 && y < 12 { 255 } else { 0 };
                img.put_pixel(x, y, Rgba([200, 50, 20, a]));
            }
        }
        for fx in [
            Effect::Bounce,
            Effect::Jam,
            Effect::Slide,
            Effect::Arrive,
            Effect::Leave,
            Effect::Spin,
        ] {
            let out = apply(vec![(img.clone(), 0)], &[fx], None);
            assert_eq!(out.len(), SPIN_FRAMES, "{fx:?} loops");
            assert!(
                out.iter().all(|(f, _)| f.dimensions() == (16, 16)),
                "{fx:?}"
            );
            let distinct = out.iter().filter(|(f, _)| *f != out[0].0).count();
            assert!(distinct > 0, "{fx:?} moves");
        }
    }

    #[test]
    fn arrive_grows_and_leave_vanishes() {
        let solid = || vec![(RgbaImage::from_pixel(8, 8, Rgba([9, 9, 9, 255])), 0)];
        let opaque = |f: &RgbaImage| f.pixels().filter(|p| p[3] > 0).count();
        let arrive = apply(solid(), &[Effect::Arrive], None);
        assert_eq!(opaque(&arrive[0].0), 0, "starts empty");
        assert_eq!(opaque(&arrive[SPIN_FRAMES - 1].0), 64, "ends full");
        let leave = apply(solid(), &[Effect::Leave], None);
        assert_eq!(opaque(&leave[0].0), 64, "starts full");
        assert!(opaque(&leave[SPIN_FRAMES - 1].0) < 16, "ends nearly gone");
    }

    #[test]
    fn motion_keeps_an_animations_own_frames() {
        let anim = vec![
            (RgbaImage::from_pixel(8, 8, Rgba([255, 0, 0, 255])), 50),
            (RgbaImage::from_pixel(8, 8, Rgba([0, 0, 255, 255])), 90),
        ];
        let out = apply(anim, &[Effect::Bounce], None);
        assert_eq!(out.len(), 2);
        assert_eq!(out.iter().map(|f| f.1).collect::<Vec<_>>(), vec![50, 90]);
    }

    #[test]
    fn spin_quarter_turn_matches_rotate() {
        let mut img = RgbaImage::new(4, 4);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let turned = spin(&img, std::f32::consts::FRAC_PI_2);
        // clockwise quarter turn: top-left lands top-right
        assert_eq!(turned.get_pixel(3, 0), &Rgba([255, 0, 0, 255]));
        assert_eq!(turned.get_pixel(0, 0)[3], 0);
    }

    #[test]
    fn effects_are_deterministic() {
        let fx = [
            Effect::Cursed,
            Effect::Rainbow,
            Effect::Shake,
            Effect::Spin,
            Effect::Jam,
        ];
        let a = apply(px(10, 200, 30), &fx, Some(40));
        let b = apply(px(10, 200, 30), &fx, Some(40));
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.0, y.0);
        }
    }

    #[test]
    fn empty_input_survives_every_effect() {
        for fx in heatsync_core::emote::split_key("u\n#lrxywtcHpRsbjdaeo")[0]
            .fx
            .iter()
        {
            assert!(apply(Vec::new(), &[*fx], None).is_empty(), "{fx:?}");
        }
        assert!(apply(Vec::new(), &[], Some(90)).is_empty());
    }
}
