//! emote frame decode — webp/gif/png/jpeg → a flat list of rgba frames + delays.
//! pure-Rust (`image` crate), no libwebp/libgif C deps. static images decode to a
//! single frame; animated webp/gif expand to their real frames. the render layer
//! resizes to cell-pixel dims — decode stays terminal-agnostic and testable.

use std::io::Cursor;

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ImageFormat, RgbaImage};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// one decoded frame: pixels + how long to show it.
#[derive(Clone)]
pub struct Frame {
    pub img: RgbaImage,
    pub delay_ms: u32,
}

/// a fully decoded emote — one or more frames. `animated` is a convenience flag.
pub struct Decoded {
    pub frames: Vec<Frame>,
    pub width: u32,
    pub height: u32,
}

impl Decoded {
    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }
}

/// decode raw image bytes into frames. animated formats expand; everything else
/// is a single frame. a zero-delay animation frame is floored to 20ms so a
/// malformed emote can't spin the animator at infinite fps (bulletproof).
pub fn decode(bytes: &[u8]) -> Result<Decoded, BoxErr> {
    let fmt = image::guess_format(bytes)?;
    let frames = match fmt {
        ImageFormat::Gif => {
            let dec = GifDecoder::new(Cursor::new(bytes))?;
            collect(dec)?
        }
        ImageFormat::WebP => {
            let dec = WebPDecoder::new(Cursor::new(bytes))?;
            if dec.has_animation() {
                collect(dec)?
            } else {
                single(bytes)?
            }
        }
        _ => single(bytes)?,
    };
    let (width, height) = frames
        .first()
        .map(|f| f.img.dimensions())
        .unwrap_or((0, 0));
    if frames.is_empty() {
        return Err("decoded zero frames".into());
    }
    Ok(Decoded {
        frames,
        width,
        height,
    })
}

fn single(bytes: &[u8]) -> Result<Vec<Frame>, BoxErr> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    Ok(vec![Frame {
        img,
        delay_ms: 0,
    }])
}

fn collect<'a>(dec: impl AnimationDecoder<'a>) -> Result<Vec<Frame>, BoxErr> {
    let mut out = Vec::new();
    for f in dec.into_frames() {
        let f = f?;
        let (num, den) = f.delay().numer_denom_ms();
        let ms = if den == 0 { 0 } else { num / den };
        out.push(Frame {
            img: f.into_buffer(),
            delay_ms: ms.max(20), // floor: never spin faster than 50fps per frame
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame as IFrame, Rgba};

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(px))
    }

    #[test]
    fn static_png_is_one_frame() {
        let mut buf = Vec::new();
        solid(8, 8, [255, 135, 0, 255])
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let d = decode(&buf).unwrap();
        assert_eq!(d.frames.len(), 1);
        assert!(!d.animated());
        assert_eq!((d.width, d.height), (8, 8));
    }

    #[test]
    fn animated_gif_expands_to_all_frames() {
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            for px in [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]] {
                let frame = IFrame::from_parts(
                    solid(4, 4, px),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                );
                enc.encode_frame(frame).unwrap();
            }
        }
        let d = decode(&buf).unwrap();
        assert_eq!(d.frames.len(), 3);
        assert!(d.animated());
        assert!(d.frames.iter().all(|f| f.delay_ms >= 20));
    }

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode(b"not an image at all").is_err());
    }
}
