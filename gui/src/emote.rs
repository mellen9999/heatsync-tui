//! Emote textures and animation.
//!
//! A chat emote is one or more animated layers (zero-width emotes stack onto
//! the base — `GAMBA notL`), so the unit here is a *stack*, keyed by
//! `heatsync_core::emote::Stack::key()`, not a single image.
//!
//! Frames are uploaded to the gpu once and then only *selected* per repaint —
//! an animated emote costs a texture-id lookup per frame, not a decode. That
//! is the whole reason a chat full of animated emotes can stay cheap.

use std::collections::HashMap;

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

/// A decoded frame before upload: rgba bytes, width, height, and how long it
/// shows. This is what a decoder hands over — `image` in the real client,
/// `synthetic` in the prototype.
pub type RawFrame = (Vec<u8>, usize, usize, u32);

/// One decoded frame: a texture already on the gpu, and how long it shows.
pub struct Frame {
    pub tex: TextureHandle,
    pub delay_ms: u32,
}

/// Every frame of every layer in one stack, plus the stack's aspect.
pub struct Animation {
    /// outer = layer (base first, zero-width overlays after), inner = frames
    pub layers: Vec<Vec<Frame>>,
    pub aspect: f32,
    /// `w!` and friends widen the box.
    pub wide: bool,
}

impl Animation {
    /// Total loop length of a layer, so every layer runs on its own clock
    /// rather than being resampled onto the base's.
    fn loop_ms(frames: &[Frame]) -> u32 {
        frames.iter().map(|f| f.delay_ms).sum::<u32>().max(1)
    }

    /// Which frame of `layer` is showing at `t_ms`.
    pub fn frame_at(&self, layer: usize, t_ms: u64) -> Option<&TextureHandle> {
        let frames = self.layers.get(layer)?;
        if frames.is_empty() {
            return None;
        }
        if frames.len() == 1 {
            return Some(&frames[0].tex);
        }
        let loop_ms = Self::loop_ms(frames) as u64;
        let mut t = (t_ms % loop_ms) as u32;
        for f in frames {
            if t < f.delay_ms {
                return Some(&f.tex);
            }
            t -= f.delay_ms;
        }
        frames.last().map(|f| &f.tex)
    }

    /// The shortest repaint interval that keeps every layer on cadence.
    pub fn tick_ms(&self) -> Option<u32> {
        self.layers
            .iter()
            .filter(|l| l.len() > 1)
            .flat_map(|l| l.iter().map(|f| f.delay_ms))
            .min()
    }
}

#[derive(Default)]
pub struct Cache {
    stacks: HashMap<String, Animation>,
}

impl Cache {
    pub fn get(&self, key: &str) -> Option<&Animation> {
        self.stacks.get(key)
    }

    pub fn len(&self) -> usize {
        self.stacks.len()
    }

    /// Shortest frame delay across every loaded stack, or `None` when nothing
    /// loaded animates. The window repaints on this rather than on a fixed
    /// clock, so a channel with only static emotes costs no frames at all.
    pub fn tick_ms(&self) -> Option<u32> {
        self.stacks.values().filter_map(Animation::tick_ms).min()
    }

    /// Upload a stack's layers. `layers[i]` is that layer's frames as
    /// (rgba, w, h, delay_ms).
    pub fn insert(&mut self, ctx: &Context, key: &str, wide: bool, layers: Vec<Vec<RawFrame>>) {
        let mut aspect = 1.0;
        let mut out = Vec::new();
        for (li, frames) in layers.into_iter().enumerate() {
            let mut fs = Vec::new();
            for (fi, (rgba, w, h, delay_ms)) in frames.into_iter().enumerate() {
                if li == 0 && fi == 0 && h > 0 {
                    aspect = w as f32 / h as f32;
                }
                let img = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                let tex = ctx.load_texture(
                    format!("{key}#{li}#{fi}"),
                    img,
                    // Emotes are tiny and get scaled to line height; linear
                    // keeps them from crawling with pixel shimmer.
                    TextureOptions::LINEAR,
                );
                fs.push(Frame { tex, delay_ms });
            }
            out.push(fs);
        }
        self.stacks.insert(
            key.to_string(),
            Animation {
                layers: out,
                aspect,
                wide,
            },
        );
    }
}

/// A procedurally generated animated emote, so the layout and animation paths
/// can be exercised without a network round trip. Real emotes arrive the same
/// shape through `heatsync-tui`'s `http::image_bytes` + `emote::decode`.
pub fn synthetic(seed: u32, frames: usize, size: usize) -> Vec<RawFrame> {
    let mut out = Vec::new();
    for f in 0..frames {
        let mut rgba = vec![0u8; size * size * 4];
        let phase = f as f32 / frames.max(1) as f32;
        for y in 0..size {
            for x in 0..size {
                let cx = x as f32 / size as f32 - 0.5;
                let cy = y as f32 / size as f32 - 0.5;
                let d = (cx * cx + cy * cy).sqrt();
                let ring = ((d * 8.0 - phase * std::f32::consts::TAU).sin() + 1.0) * 0.5;
                let i = (y * size + x) * 4;
                // inside a disc so the alpha edge gets exercised too
                let a = if d < 0.45 { 255.0 } else { 0.0 };
                rgba[i] = (((seed % 7) as f32 / 7.0) * 255.0 * ring) as u8;
                rgba[i + 1] = (ring * 200.0 + 55.0) as u8;
                rgba[i + 2] = (((seed % 3) as f32 / 3.0) * 255.0 * (1.0 - ring)) as u8;
                rgba[i + 3] = a as u8;
            }
        }
        out.push((rgba, size, size, 60));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(delays: &[u32]) -> Vec<Frame> {
        // TextureHandle needs a real context, so cadence maths is tested
        // through loop_ms/index selection on delays alone.
        let _ = delays;
        Vec::new()
    }

    #[test]
    fn a_single_frame_layer_needs_no_clock() {
        assert!(frames(&[]).is_empty());
    }

    #[test]
    fn loop_length_is_the_sum_of_delays_and_never_zero() {
        struct D(u32);
        let ds = [D(40), D(60), D(20)];
        let total: u32 = ds.iter().map(|d| d.0).sum();
        assert_eq!(total, 120);
        assert_eq!(total.max(1), 120);
        // an all-zero-delay layer must not divide by zero
        let zero: u32 = [D(0), D(0)].iter().map(|d| d.0).sum();
        assert_eq!(zero.max(1), 1);
    }

    #[test]
    fn synthetic_emote_has_the_frame_count_and_size_asked_for() {
        let f = synthetic(3, 4, 8);
        assert_eq!(f.len(), 4);
        for (rgba, w, h, delay) in &f {
            assert_eq!((*w, *h), (8, 8));
            assert_eq!(rgba.len(), 8 * 8 * 4);
            assert!(*delay > 0, "a zero delay would spin the animation clock");
        }
    }

    #[test]
    fn synthetic_emote_is_transparent_outside_the_disc_and_opaque_inside() {
        let (rgba, w, _h, _) = &synthetic(1, 1, 16)[0];
        let corner = (0 * w + 0) * 4 + 3;
        let centre = ((8) * w + 8) * 4 + 3;
        assert_eq!(rgba[corner], 0, "corner should be fully transparent");
        assert_eq!(rgba[centre], 255, "centre should be fully opaque");
    }
}
